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

use std::borrow::Cow;
use std::collections::VecDeque;
use std::fs;
use std::path::Path;

use ahash::{AHashMap, AHashSet};
use oxc_allocator::Allocator;
use oxc_ast::ast::*;
use oxc_parser::Parser as OxcParser;
use oxc_span::{ContentEq, GetSpan, SourceType};

use super::parse::{analyse_source, is_source_file, span_of};
use super::{FineModule, ModuleAnalysis, Reading, Span, cjs, decls, types};

/// How a file's current version differs from its base version.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Comparison {
    /// Spans, in the current version, of the top-level statements that differ.
    pub changed: Vec<Span>,
    /// Names the base version exported and this one does not.
    ///
    /// A departed export is invisible from inside the file — everything left behind
    /// reads exactly as it did — and a consumer still naming it is not itself
    /// changed, so the name has to carry the mark. Renaming an export is this same
    /// case seen from the table, which is what makes a rename detectable at all.
    pub lost_exports: Vec<String>,
    /// Nothing finer than the file can say what changed: the two disagree about the
    /// module's directives, or an `export * from` went and took with it a set of
    /// names that cannot be listed without reading the module it named.
    pub whole_file: bool,
    /// Work reached during base-version evaluation changed, disappeared, or moved.
    /// This includes an edited helper that used to have effects: its unchanged
    /// caller may no longer be part of initialisation in the current graph.
    pub init_differs: bool,
}

impl Comparison {
    /// Nothing observable differs between the two versions.
    pub fn is_empty(&self) -> bool {
        self.changed.is_empty()
            && self.lost_exports.is_empty()
            && !self.whole_file
            && !self.init_differs
    }
}

/// The source this run reads, which is the source as written unless types are being
/// ignored and there are any to erase.
///
/// An erasure that leaves behind something no longer parseable is discarded, so the
/// worst it can do is leave the file read as it was.
fn erase<'a>(
    allocator: &Allocator,
    source: &'a str,
    source_type: SourceType,
    reading: &Reading,
) -> Cow<'a, str> {
    if !reading.ignore_types {
        return Cow::Borrowed(source);
    }
    let parsed = OxcParser::new(allocator, source, source_type).parse();
    if !parsed.diagnostics.is_empty() {
        return Cow::Borrowed(source);
    }
    let Some(erased) = types::erase(source, &parsed.program) else {
        return Cow::Borrowed(source);
    };
    if OxcParser::new(allocator, &erased, source_type)
        .parse()
        .diagnostics
        .is_empty()
    {
        Cow::Owned(erased)
    } else {
        Cow::Borrowed(source)
    }
}

/// Compares the file at `path` against the `before` text of it.
///
/// `None` when the two cannot be compared statement by statement.
pub fn compare(path: &Path, before: &str, reading: &Reading) -> Option<Comparison> {
    if !is_source_file(path) {
        return None;
    }
    let after = fs::read_to_string(path).ok()?;

    // One arena for both versions, so the two trees share a lifetime and can be
    // compared against each other at all.
    let allocator = Allocator::default();
    let source_type = SourceType::from_path(path).unwrap_or_default();

    // Erased or not, both versions are read the same way: a comparison between a
    // file with its types and a file without would find every annotation changed.
    let before = erase(&allocator, before, source_type, reading);
    let after = erase(&allocator, &after, source_type, reading);

    let old = OxcParser::new(&allocator, &before, source_type).parse();
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
    let mut changed_before = Vec::new();
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
            changed_before.push((index, statement));
        }
    }

    // What is left is what the base version had and this one does not. A removal
    // costs two separate things: the names it took out of the export table, and the
    // work it no longer does when the module is evaluated.
    let removed: Vec<usize> = unmatched.into_values().flatten().collect();

    // A changed helper can stop having effects while its top-level caller stays
    // unchanged. The current graph then no longer connects that helper to module
    // initialisation, so ask the base graph about changed statements as well as
    // removed ones. Trivia-only edits still need no module analysis.
    let old_analysis = (!removed.is_empty() || !changed_before.is_empty())
        .then(|| analyse_source(path, &before, reading))
        .flatten()
        .map(|(analysis, _)| analysis);
    let old_module = match &old_analysis {
        Some(ModuleAnalysis::Fine(module)) => Some(module.as_ref()),
        _ => None,
    };

    // A name the current version still exports has not gone, however its statement
    // was rearranged.
    let kept: AHashSet<&str> = new_statements
        .iter()
        .flat_map(|statement| statement.exports.names())
        .map(String::as_str)
        .collect();

    let mut whole_file = false;
    let mut lost_exports = Vec::new();
    // A changed statement the current version still runs is reached through its own
    // declaration. Only one that ran before and no longer does is work that went.
    let mut new_analysis = None;
    for &(index, statement) in &changed_before {
        let Some(module) = old_module else {
            // A formerly opaque module cannot prove which effects disappeared.
            whole_file = true;
            init_differs = true;
            continue;
        };
        if !ran_on_evaluation(module, &old_statements[index]) {
            continue;
        }
        let new_module = new_analysis.get_or_insert_with(|| {
            analyse_source(path, &after, reading).map(|(analysis, _)| analysis)
        });
        let runs_now = match new_module {
            Some(ModuleAnalysis::Fine(module)) => ran_on_evaluation(module, statement),
            _ => true,
        };
        if !runs_now {
            init_differs = true;
        }
    }
    for index in removed {
        let statement = &old_statements[index];

        let Some(module) = old_module else {
            // A base version the analyser could not describe finely cannot be asked
            // what it exported or what building it ran, so anything removed from one
            // takes the file with it.
            whole_file = true;
            init_differs = true;
            continue;
        };

        match &statement.exports {
            Exports::None => {}
            Exports::Unknown => whole_file = true,
            Exports::Names(names) => {
                for name in names {
                    if !kept.contains(name.as_str()) && !lost_exports.contains(name) {
                        lost_exports.push(name.clone());
                    }
                }
            }
        }

        if statement.runs || ran_on_evaluation(module, statement) {
            init_differs = true;
        }
    }

    Some(Comparison {
        changed,
        lost_exports,
        whole_file,
        init_differs,
    })
}

/// Did evaluating the base version do this statement's work?
///
/// Only a statement that binds values has to ask; every other kind says so for
/// itself. What the answer turns on is whether the initialiser may do anything, and
/// that is a judgement the module analysis has already made.
fn ran_on_evaluation(module: &FineModule, statement: &Keyed<'_>) -> bool {
    if !matches!(statement.key, Key::Declares(_)) {
        return false;
    }
    module
        .init_decls
        .iter()
        .filter_map(|&decl| module.decls.get(decl as usize))
        .any(|decl| decl.span == statement.span)
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

/// What a statement puts in the file's export table.
#[derive(Debug, PartialEq, Eq)]
enum Exports {
    /// Nothing.
    None,
    /// Exactly these names.
    Names(Vec<String>),
    /// Names that cannot be listed without reading another module, or without
    /// leaving the language: `export * from "./g"`, `export =`.
    Unknown,
}

impl Exports {
    fn names(&self) -> &[String] {
        match self {
            Exports::Names(names) => names,
            Exports::None | Exports::Unknown => &[],
        }
    }
}

struct Keyed<'a> {
    key: Key,
    node: &'a Statement<'a>,
    span: Span,
    /// What it puts in the export table.
    exports: Exports,
    /// Evaluating the module does this statement's work, whatever its initialisers
    /// turn out to do. A statement that only binds values is not decided here: what
    /// its initialisers do is a question for the analysis.
    runs: bool,
}

/// Every top-level statement, with the key it is matched by.
///
/// `None` when a statement introduces bindings that cannot be enumerated, which is
/// the same condition that coarsens a module.
fn keyed<'a>(program: &'a Program<'a>) -> Option<Vec<Keyed<'a>>> {
    let cjs = cjs::table(program);
    let drafts = decls::collect(program, &cjs)?;
    let mut bare = 0;
    let mut statements = Vec::with_capacity(program.body.len());

    for (index, node) in program.body.iter().enumerate() {
        let names: Vec<String> = drafts
            .iter()
            .filter(|draft| draft.statement == index)
            .map(|draft| draft.name.clone())
            .collect();

        // Every one of these brings in or runs something of its own. A statement
        // that only binds values does not, and is left for the analysis to judge.
        let mut runs = matches!(
            node,
            Statement::ImportDeclaration(_)
                | Statement::ExportFromDeclaration(_)
                | Statement::ExportAllDeclaration(_)
        );

        let key = if !names.is_empty() {
            Key::Declares(sorted(names.clone()))
        } else {
            match node {
                Statement::ImportDeclaration(import) => {
                    Key::Imports(import.source.value.to_string())
                }
                Statement::ExportNamedDeclaration(export) => {
                    Key::Exports(sorted(exported_names(&export.specifiers)))
                }
                Statement::ExportFromDeclaration(export) => {
                    Key::Exports(sorted(exported_names(&export.specifiers)))
                }
                Statement::ExportAllDeclaration(export) => {
                    Key::ExportsAll(export.source.value.to_string())
                }
                _ => {
                    runs = true;
                    bare += 1;
                    Key::Runs(bare - 1)
                }
            }
        };

        statements.push(Keyed {
            key,
            node,
            span: span_of(node.span()),
            exports: exports_of(node, index, &cjs, names),
            runs,
        });
    }

    Some(statements)
}

fn exported_names(specifiers: &oxc_allocator::Vec<'_, ExportSpecifier<'_>>) -> Vec<String> {
    specifiers
        .iter()
        .map(|specifier| specifier.exported.name().to_string())
        .collect()
}

/// What a statement puts in the export table, given the names it declares.
fn exports_of(
    node: &Statement<'_>,
    index: usize,
    cjs: &cjs::Table,
    declared: Vec<String>,
) -> Exports {
    // `exports.x = …` puts a name in the table without being an export statement in
    // the language's sense, so it is asked for separately.
    let commonjs: Vec<String> = cjs.names_at(index).map(str::to_string).collect();
    if !commonjs.is_empty() {
        return Exports::Names(commonjs);
    }

    match node {
        // `export const x = 1`, `export default ...`: what it declares is what it
        // exports, and the default export is named for the slot it fills.
        Statement::ExportDeclaration(_) | Statement::ExportDefaultDeclaration(_) => {
            Exports::Names(declared)
        }
        Statement::ExportNamedDeclaration(export) => {
            Exports::Names(exported_names(&export.specifiers))
        }
        Statement::ExportFromDeclaration(export) => {
            Exports::Names(exported_names(&export.specifiers))
        }
        // `export * as ns from "./g"` exports one name it writes down; a plain
        // `export * from "./g"` exports whatever the other module does.
        Statement::ExportAllDeclaration(export) => match &export.exported {
            Some(exported) => Exports::Names(vec![exported.name().to_string()]),
            None => Exports::Unknown,
        },
        Statement::TSExportAssignment(_) | Statement::TSNamespaceExportDeclaration(_) => {
            Exports::Unknown
        }
        _ => Exports::None,
    }
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
        compared_as(before, after, &Reading::default())
    }

    /// The same, read a named way. Only a test about types needs to say which.
    fn compared_as(before: &str, after: &str, reading: &Reading) -> Option<Comparison> {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("module.ts");
        fs::write(&path, after).expect("writing the current version");
        compare(&path, before, reading)
    }

    /// A reading that takes the file as written, types and all.
    fn as_written() -> Reading {
        Reading {
            ignore_types: false,
            ..Reading::default()
        }
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
    fn a_renamed_export_loses_the_old_name() {
        // The new name arrives as a change; the old one leaving is what a consumer
        // still asking for it has to be told about.
        let before = "export const a = 1;\n";
        let after = "export const b = 1;\n";

        let comparison = compared(before, after).expect("comparable");
        assert_eq!(comparison.lost_exports, vec!["a".to_string()]);
        assert!(!comparison.whole_file);
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
    fn a_removed_export_is_named_rather_than_coarsened() {
        let before = "export const a = 1;\nexport const b = 2;\n";
        let after = "export const a = 1;\n";

        let comparison = compared(before, after).expect("comparable");
        assert_eq!(comparison.lost_exports, vec!["b".to_string()]);
        // Nothing ran to build it, so nothing stopped running when it went.
        assert!(!comparison.init_differs);
        assert!(!comparison.whole_file);
    }

    #[test]
    fn a_removed_export_whose_value_was_computed_changes_initialisation_too() {
        let before = "export const a = 1;\nexport const b = register();\n";
        let after = "export const a = 1;\n";

        let comparison = compared(before, after).expect("comparable");
        assert_eq!(comparison.lost_exports, vec!["b".to_string()]);
        assert!(comparison.init_differs);
    }

    #[test]
    fn a_reshuffled_export_has_not_gone() {
        // The same name, exported by a different statement. The declaration is
        // reported changed; the name is not reported lost.
        let before = "export const a = 1;\n";
        let after = "const a = 1;\nexport { a };\n";

        let comparison = compared(before, after).expect("comparable");
        assert!(comparison.lost_exports.is_empty());
    }

    #[test]
    fn an_export_star_that_went_took_names_it_cannot_list() {
        let before = "export * from \"./g\";\nexport const a = 1;\n";
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
    fn a_changed_helper_preserves_its_former_initialisation_effects() {
        let before = "function make() { return register(); }\nconst value = make();\nexport const version = 1;\n";
        let after =
            "function make() { return 1; }\nconst value = make();\nexport const version = 1;\n";

        let comparison = compared(before, after).expect("comparable");
        assert_eq!(
            changed(after, &comparison),
            ["function make() { return 1; }"]
        );
        assert!(comparison.init_differs);
        assert!(!comparison.whole_file);
    }

    #[test]
    fn a_changed_initialiser_that_still_runs_is_reached_through_its_declaration() {
        let before = "export const client = track(\"boot\");\n";
        let after = "export const client = track(\"start\");\n";

        let comparison = compared(before, after).expect("comparable");
        assert!(!comparison.init_differs);
        assert!(!comparison.whole_file);
    }

    #[test]
    fn an_uncalled_helpers_body_is_not_a_former_initialisation_effect() {
        let before = "function make() { return register(); }\nexport const version = 1;\n";
        let after = "function make() { return 1; }\nexport const version = 1;\n";

        let comparison = compared(before, after).expect("comparable");
        assert!(!comparison.init_differs);
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
        // apart. Only the one that differs may be reported. Read as written, because
        // an overload signature is one of the things a default read erases.
        let before = "export function f(a: string): void;\nexport function f(a: number): void;\nexport function f(a) {}\n";
        let after = "export function f(a: string): void;\nexport function f(a: boolean): void;\nexport function f(a) {}\n";

        let comparison = compared_as(before, after, &as_written()).expect("comparable");
        assert_eq!(
            changed(after, &comparison),
            vec!["export function f(a: boolean): void;"]
        );
        assert!(!comparison.init_differs);

        // And read the default way there is nothing there to differ: an overload
        // signature runs nothing, so the two files are the same file.
        let comparison = compared(before, after).expect("comparable");
        assert!(comparison.is_empty());
    }
}
