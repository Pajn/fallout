//! Comparing a file against an earlier version of itself.
//!
//! A diff describes lines. This describes statements: each top-level statement is
//! matched to its counterpart in the base version and the two are compared as syntax
//! trees, with spans and comments left out of the comparison. A statement that was
//! reformatted, recommented or merely moved therefore differs in no way that matters,
//! and a statement that really did change is known exactly rather than inferred from
//! the lines around it.
//!
//! Anything that stops the two versions being compared — a parse error in either, or
//! a statement whose bindings cannot be enumerated — is reported as "no answer", and
//! the caller falls back on the diff's line ranges.

use std::collections::VecDeque;
use std::fs;
use std::path::Path;

use ahash::AHashMap;
use oxc_allocator::Allocator;
use oxc_ast::ast::*;
use oxc_parser::Parser as OxcParser;
use oxc_span::{ContentEq, GetSpan, SourceType};

use super::parse::{is_source_file, span_of};
use super::{Span, decls};

/// How a file's current version differs from its base version.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Comparison {
    /// Spans, in the current version, of the top-level statements that differ.
    pub changed: Vec<Span>,
    /// Nothing finer than the file can say what changed: the base version exported
    /// something this one does not, or the two disagree about the module's
    /// directives.
    ///
    /// A departed export is invisible from inside the file — everything left behind
    /// reads exactly as it did — and a consumer still naming it is not itself
    /// changed. A name missing from an export table resolves to the file in any
    /// case, so the file is both the honest mark and the only reachable one.
    /// Renaming an export is this same case seen from the table, which is what makes
    /// a rename detectable at all.
    pub whole_file: bool,
    /// Something the base version did on evaluation is no longer done, or is done in
    /// a different order: an import or a bare statement went, a private declaration
    /// that may have run something went, or two statements swapped places. Nothing a
    /// declaration *is* has changed, only when the module gets round to it.
    pub init_differs: bool,
}

impl Comparison {
    /// Nothing observable differs between the two versions.
    pub fn is_empty(&self) -> bool {
        self.changed.is_empty() && !self.whole_file && !self.init_differs
    }
}

/// Compares the file at `path` against the `before` text of it.
///
/// `None` when the two cannot be compared statement by statement.
pub fn compare(path: &Path, before: &str) -> Option<Comparison> {
    if !is_source_file(path) {
        return None;
    }
    let after = fs::read_to_string(path).ok()?;

    // One arena for both versions, so the two trees share a lifetime and can be
    // compared against each other at all.
    let allocator = Allocator::default();
    let source_type = SourceType::from_path(path).unwrap_or_default();
    let old = OxcParser::new(&allocator, before, source_type).parse();
    let new = OxcParser::new(&allocator, &after, source_type).parse();

    // A parse error means one of the trees is a guess. Guessing is what the
    // line-based fallback is for.
    if !old.diagnostics.is_empty() || !new.diagnostics.is_empty() {
        return None;
    }

    // A directive or a hashbang decides how the module as a whole is treated —
    // `"use client"` most of all — and belongs to no statement, so a change to one
    // is a change to the file.
    if old.program.directives.content_ne(&new.program.directives)
        || old.program.hashbang.content_ne(&new.program.hashbang)
    {
        return Some(Comparison {
            whole_file: true,
            ..Comparison::default()
        });
    }

    let old_statements = keyed(&old.program)?;
    let new_statements = keyed(&new.program)?;

    // Counterparts are handed out first come, first served, so two statements sharing
    // a key — TypeScript's overload signatures, two imports of one specifier — are
    // matched in the order they are written.
    let mut unmatched: AHashMap<&Key, VecDeque<usize>> = AHashMap::default();
    for (index, statement) in old_statements.iter().enumerate() {
        unmatched
            .entry(&statement.key)
            .or_default()
            .push_back(index);
    }

    let mut changed = Vec::new();
    let mut init_differs = false;
    let mut furthest = 0;

    for statement in &new_statements {
        let counterpart = unmatched
            .get_mut(&statement.key)
            .and_then(VecDeque::pop_front);

        let Some(index) = counterpart else {
            changed.push(statement.span);
            continue;
        };

        // Counterparts are handed out in the order the current version writes its
        // statements, so one that comes from further back than a statement already
        // matched is one that has moved.
        if index < furthest {
            init_differs = true;
        }
        furthest = furthest.max(index);

        if old_statements[index].node.content_ne(statement.node) {
            changed.push(statement.span);
        }
    }

    // What is left is what the base version had and this one does not. A statement
    // that put a name in the export table takes the whole file with it; one that only
    // ran something takes module initialisation.
    let mut whole_file = false;
    for index in unmatched.into_values().flatten() {
        if old_statements[index].exports {
            whole_file = true;
        } else {
            init_differs = true;
        }
    }

    Some(Comparison {
        changed,
        whole_file,
        init_differs,
    })
}

/// How a top-level statement is matched to its counterpart in the base version.
///
/// Matching on what a statement introduces, rather than on where it sits, is what
/// lets a statement move — or have another inserted above it — without that counting
/// as a change to either.
#[derive(Debug, PartialEq, Eq, Hash)]
enum Key {
    /// Declares these top-level names.
    Declares(Vec<String>),
    /// Exports these names without declaring them.
    Exports(Vec<String>),
    /// `export * from "./g"`, which names no export of its own.
    ExportsAll(String),
    /// Imports from this specifier.
    Imports(String),
    /// Introduces nothing, so there is nothing to match it by but its place among
    /// the other such statements.
    Runs(usize),
}

struct Keyed<'a> {
    key: Key,
    node: &'a Statement<'a>,
    span: Span,
    /// Puts at least one name in the file's export table.
    exports: bool,
}

/// Every top-level statement, with the key it is matched by.
///
/// `None` when a statement introduces bindings that cannot be enumerated, which is
/// the same condition that coarsens a module.
fn keyed<'a>(program: &'a Program<'a>) -> Option<Vec<Keyed<'a>>> {
    let drafts = decls::collect(program)?;
    let mut runs = 0;
    let mut statements = Vec::with_capacity(program.body.len());

    for (index, node) in program.body.iter().enumerate() {
        let names: Vec<String> = drafts
            .iter()
            .filter(|draft| draft.statement == index)
            .map(|draft| draft.name.clone())
            .collect();

        let key = if !names.is_empty() {
            Key::Declares(sorted(names))
        } else {
            match node {
                Statement::ImportDeclaration(import) => {
                    Key::Imports(import.source.value.to_string())
                }
                Statement::ExportNamedDeclaration(export) => Key::Exports(sorted(
                    export
                        .specifiers
                        .iter()
                        .map(|specifier| specifier.exported.name().to_string())
                        .collect(),
                )),
                Statement::ExportFromDeclaration(export) => Key::Exports(sorted(
                    export
                        .specifiers
                        .iter()
                        .map(|specifier| specifier.exported.name().to_string())
                        .collect(),
                )),
                Statement::ExportAllDeclaration(export) => {
                    Key::ExportsAll(export.source.value.to_string())
                }
                _ => {
                    runs += 1;
                    Key::Runs(runs - 1)
                }
            }
        };

        statements.push(Keyed {
            key,
            node,
            span: span_of(node.span()),
            exports: matches!(
                node,
                Statement::ExportDeclaration(_)
                    | Statement::ExportDefaultDeclaration(_)
                    | Statement::ExportNamedDeclaration(_)
                    | Statement::ExportFromDeclaration(_)
                    | Statement::ExportAllDeclaration(_)
                    | Statement::TSExportAssignment(_)
                    | Statement::TSNamespaceExportDeclaration(_)
            ),
        });
    }

    Some(statements)
}

/// One statement declaring several names must key the same however they were spelled.
fn sorted(mut names: Vec<String>) -> Vec<String> {
    names.sort();
    names
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Compares `before` against `after` as one TypeScript file.
    fn compared(before: &str, after: &str) -> Option<Comparison> {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("module.ts");
        fs::write(&path, after).expect("writing the current version");
        compare(&path, before)
    }

    /// The text of each statement the comparison calls changed.
    fn changed<'a>(after: &'a str, comparison: &Comparison) -> Vec<&'a str> {
        comparison
            .changed
            .iter()
            .map(|span| &after[span.start as usize..span.end as usize])
            .collect()
    }

    #[test]
    fn a_reworded_comment_is_no_change_at_all() {
        let before = "// adds one\nexport const inc = (n) => n + 1;\n";
        let after = "// Adds one to its argument.\nexport const inc = (n) => n + 1;\n";
        assert!(compared(before, after).expect("comparable").is_empty());
    }

    #[test]
    fn reformatting_is_no_change_at_all() {
        let before = "export const inc = (n) => n + 1;\n";
        let after = "export const inc = (n) =>\n  n + 1;\n";
        assert!(compared(before, after).expect("comparable").is_empty());
    }

    #[test]
    fn a_moved_statement_changes_when_the_module_runs_and_nothing_else() {
        let before = "export const a = 1;\nexport const b = 2;\n";
        let after = "export const b = 2;\nexport const a = 1;\n";

        let comparison = compared(before, after).expect("comparable");
        assert!(comparison.changed.is_empty());
        assert!(comparison.init_differs);
        assert!(!comparison.whole_file);
    }

    #[test]
    fn a_statement_that_only_shifted_down_has_not_moved() {
        // Everything after the insertion sits at a different offset, and none of it
        // has changed: matching on names rather than on positions is what sees that.
        let before = "export const a = 1;\n";
        let after = "import \"./setup\";\nexport const a = 1;\n";

        let comparison = compared(before, after).expect("comparable");
        assert_eq!(changed(after, &comparison), vec!["import \"./setup\";"]);
        assert!(!comparison.init_differs);
    }

    #[test]
    fn only_the_statement_that_differs_is_reported() {
        let before = "export const a = 1;\nexport const b = 2;\n";
        let after = "export const a = 1;\nexport const b = 3;\n";

        let comparison = compared(before, after).expect("comparable");
        assert_eq!(changed(after, &comparison), vec!["export const b = 3;"]);
        assert!(!comparison.whole_file);
    }

    #[test]
    fn a_renamed_declaration_is_a_removal() {
        // The new name arrives as a change, and the old one leaving is what makes
        // the file's export table something a consumer cannot be sure about.
        let before = "export const a = 1;\n";
        let after = "export const b = 1;\n";

        let comparison = compared(before, after).expect("comparable");
        assert!(comparison.whole_file);
    }

    #[test]
    fn a_removed_import_changes_only_when_the_module_runs() {
        let before = "import \"./setup\";\nexport const a = 1;\n";
        let after = "export const a = 1;\n";

        let comparison = compared(before, after).expect("comparable");
        assert!(comparison.init_differs);
        assert!(!comparison.whole_file);
    }

    #[test]
    fn a_removed_export_takes_the_file_with_it() {
        let before = "export const a = 1;\nexport const b = 2;\n";
        let after = "export const a = 1;\n";

        assert!(compared(before, after).expect("comparable").whole_file);
    }

    #[test]
    fn a_removed_private_declaration_changes_only_when_the_module_runs() {
        // Nothing outside the file could name it, so nothing outside the file can
        // tell it has gone except by what its initialiser no longer does.
        let before = "const a = register();\nexport const b = 2;\n";
        let after = "export const b = 2;\n";

        let comparison = compared(before, after).expect("comparable");
        assert!(comparison.init_differs);
        assert!(!comparison.whole_file);
    }

    #[test]
    fn a_directive_belongs_to_the_whole_file() {
        let before = "export const a = 1;\n";
        let after = "\"use client\";\nexport const a = 1;\n";

        assert!(compared(before, after).expect("comparable").whole_file);
    }

    #[test]
    fn a_version_that_does_not_parse_has_no_answer() {
        let before = "export const a = (;\n";
        let after = "export const a = 1;\n";

        assert_eq!(compared(before, after), None);
    }

    #[test]
    fn overloads_are_matched_in_the_order_they_are_written() {
        // Three statements share the name `f`, so nothing but their order tells them
        // apart. Only the one that differs may be reported.
        let before = "export function f(a: string): void;\nexport function f(a: number): void;\nexport function f(a) {}\n";
        let after = "export function f(a: string): void;\nexport function f(a: boolean): void;\nexport function f(a) {}\n";

        let comparison = compared(before, after).expect("comparable");
        assert_eq!(
            changed(after, &comparison),
            vec!["export function f(a: boolean): void;"]
        );
        assert!(!comparison.init_differs);
    }
}
