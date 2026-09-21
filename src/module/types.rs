//! Erasing what TypeScript erases.
//!
//! A type cannot change what a page renders. It can break the build, but a broken
//! build breaks every page at once and needs no answer about reachability — which is
//! what makes `--ignore-types` defensible, and why it is a flag rather than the
//! default.
//!
//! The erasure happens once, on the source, before anything reads it: every byte of
//! type-only syntax becomes a space, and newlines stay newlines. Everything
//! downstream then falls out on its own. An `interface` is no longer a statement, so
//! nothing declares it and nothing depends on it. An `import type` is no longer an
//! import, so it is no longer an edge. Two versions of a file that differ only in
//! their annotations become the same text, so comparing them finds nothing. And
//! because byte offsets and line numbers survive untouched, every span still points
//! at the line it came from in the file on disk.
//!
//! What is erased is only what the language erases. A parameter property, an enum,
//! a namespace and an `import x = require(…)` all run, and none of them is touched.
//! Anything not listed here is left alone and compared as before, which costs
//! precision and never costs safety.
//!
//! A type does not always come away cleanly. A name in a list is held there by a
//! comma, `implements` needs something to name, and `x!: T` carries its mark in
//! front of the annotation — each of those leaves behind something that is no
//! longer the language unless it is taken along. One case cannot be taken along at
//! all: nothing may come between an arrow function's parameters and its `=>` but
//! spaces, so a return type written across lines is holding the two apart and stays
//! where it is.
//!
//! The caller reparses what is left and reads the original if it will not parse, so
//! a corner nobody has met yet costs precision rather than an answer.

use oxc_ast::ast::*;
use oxc_ast_visit::{Visit, walk};
use oxc_span::{GetSpan, Span};

/// The source with its type-only syntax blanked out.
///
/// `None` when there was nothing to erase, so that a caller can go on using the text
/// and the tree it already has.
pub(crate) fn erase(source: &str, program: &Program<'_>) -> Option<String> {
    let mut eraser = Eraser {
        source,
        spans: Vec::new(),
    };
    eraser.visit_program(program);
    if eraser.spans.is_empty() {
        return None;
    }

    let mut erased = source.as_bytes().to_vec();
    for span in eraser.spans {
        let start = span.start as usize;
        let end = (span.end as usize).min(erased.len());
        for byte in &mut erased[start.min(end)..end] {
            // Newlines stay, so that every line keeps its number. Everything else
            // becomes a space, one for one, so that every offset keeps its meaning.
            if *byte != b'\n' {
                *byte = b' ';
            }
        }
    }

    // Blanking never splits a character: a span covers whole tokens, and each byte of
    // a multi-byte one becomes one space.
    String::from_utf8(erased).ok()
}

struct Eraser<'s> {
    source: &'s str,
    spans: Vec<Span>,
}

impl Eraser<'_> {
    fn erase(&mut self, span: Span) {
        self.spans.push(span);
    }

    /// The type-only names in a list of names, each with the comma that holds it
    /// there.
    ///
    /// Blanking a name on its own would leave that comma behind, and a list with a
    /// hole in it is not a list. The comma after the name is the one to take; a name
    /// with nothing after it takes the one in front of it instead, and a name that
    /// is the only one there has none to take.
    fn erase_names(&mut self, names: &[(Span, bool)]) {
        for (span, type_only) in names {
            if !type_only {
                continue;
            }
            let end = self.past_comma(span.end);
            let start = if end == span.end {
                self.back_to_comma(span.start)
            } else {
                span.start
            };
            self.erase(Span::new(start, end));
        }
    }

    /// Past the comma that follows `from`, or `from` itself when none does.
    ///
    /// Only trivia stands between a name and its comma, and trivia is blanked as
    /// harmlessly here as anywhere else.
    fn past_comma(&self, from: u32) -> u32 {
        for (offset, character) in self.source[from as usize..].char_indices() {
            if character == ',' {
                return from + offset as u32 + 1;
            }
            if !character.is_whitespace() {
                break;
            }
        }
        from
    }

    /// Back to the comma that precedes `from`, or `from` itself when none does.
    fn back_to_comma(&self, from: u32) -> u32 {
        for (offset, character) in self.source[..from as usize].char_indices().rev() {
            if character == ',' {
                return offset as u32;
            }
            if !character.is_whitespace() {
                break;
            }
        }
        from
    }

    /// Whether erasing this annotation would leave an arrow on a line of its own.
    ///
    /// Only the return type of an arrow function can stand in that position, and
    /// nothing may come between an arrow's parameters and its `=>` but spaces. An
    /// annotation written across lines is holding the two apart, so it stays where
    /// it is: one declaration read as it was written, and nothing else changed.
    fn strands_an_arrow(&self, span: Span) -> bool {
        let after = &self.source[span.end as usize..];
        let gap = after.len() - after.trim_start().len();
        after[gap..].starts_with("=>")
            && self.source[span.start as usize..span.end as usize + gap].contains('\n')
    }

    /// Back over the `!` or `?` written in front of an annotation.
    ///
    /// Both are claims about the type rather than about the value, and both are only
    /// allowed where there is an annotation for them to qualify: `x!: T` erased to
    /// `x!` is not something the language has.
    fn back_over_mark(&self, from: u32) -> u32 {
        let before = self.source[..from as usize].trim_end();
        match before.chars().next_back() {
            Some('!' | '?') => before.len() as u32 - 1,
            _ => from,
        }
    }

    /// The names inside an export that carries values too:
    /// `export { widget, type Widget }`.
    fn erase_type_only(&mut self, specifiers: &[ExportSpecifier<'_>]) {
        let names: Vec<(Span, bool)> = specifiers
            .iter()
            .map(|specifier| (specifier.span, specifier.export_kind.is_type()))
            .collect();
        self.erase_names(&names);
    }

    /// The type part of an expression written `<value> <keyword> <type>`, which is
    /// everything after the value it is written about.
    fn erase_suffix(&mut self, value: &Expression<'_>, whole: Span) {
        self.erase(Span::new(value.span().end, whole.end));
    }
}

impl<'a> Visit<'a> for Eraser<'_> {
    // `: T`, in every position one can appear. The span includes the colon, and
    // the `!` or `?` in front of it belongs to it.
    fn visit_ts_type_annotation(&mut self, node: &TSTypeAnnotation<'a>) {
        if !self.strands_an_arrow(node.span) {
            self.erase(Span::new(
                self.back_over_mark(node.span.start),
                node.span.end,
            ));
        }
    }

    // `<T>` on a declaration, and `<string>` on a call or a type reference.
    fn visit_ts_type_parameter_declaration(&mut self, node: &TSTypeParameterDeclaration<'a>) {
        self.erase(node.span);
    }

    fn visit_ts_type_parameter_instantiation(&mut self, node: &TSTypeParameterInstantiation<'a>) {
        self.erase(node.span);
    }

    fn visit_ts_as_expression(&mut self, node: &TSAsExpression<'a>) {
        self.erase_suffix(&node.expression, node.span);
        walk::walk_ts_as_expression(self, node);
    }

    fn visit_ts_satisfies_expression(&mut self, node: &TSSatisfiesExpression<'a>) {
        self.erase_suffix(&node.expression, node.span);
        walk::walk_ts_satisfies_expression(self, node);
    }

    fn visit_ts_non_null_expression(&mut self, node: &TSNonNullExpression<'a>) {
        self.erase_suffix(&node.expression, node.span);
        walk::walk_ts_non_null_expression(self, node);
    }

    // `implements Foo, Bar` says nothing about what the class does. The keyword
    // goes with the names: left on its own it would be a class implementing
    // nothing, which is not a class.
    fn visit_class(&mut self, class: &Class<'a>) {
        if let (Some(first), Some(last)) = (class.implements.first(), class.implements.last()) {
            let before = &self.source[..first.span.start as usize];
            if let Some(keyword) = before.rfind("implements") {
                self.erase(Span::new(keyword as u32, last.span.end));
            }
        }
        walk::walk_class(self, class);
    }

    fn visit_statement(&mut self, statement: &Statement<'a>) {
        if let Some(span) = erased_whole(statement) {
            self.erase(span);
            return;
        }
        walk::walk_statement(self, statement);
    }

    // A type-only name inside an import or export that also carries values:
    // `import { widget, type Widget } from "./widget"`.
    fn visit_import_declaration(&mut self, node: &ImportDeclaration<'a>) {
        // Only the names in braces are a list. A default import beside them is not
        // one of them, and reaching back past the brace for its comma would take the
        // brace too.
        let names: Vec<(Span, bool)> = node
            .specifiers
            .iter()
            .flatten()
            .filter_map(|specifier| match specifier {
                ImportDeclarationSpecifier::ImportSpecifier(specifier) => {
                    Some((specifier.span, specifier.import_kind.is_type()))
                }
                _ => None,
            })
            .collect();
        self.erase_names(&names);
        walk::walk_import_declaration(self, node);
    }

    fn visit_export_named_declaration(&mut self, node: &ExportNamedDeclaration<'a>) {
        self.erase_type_only(&node.specifiers);
        walk::walk_export_named_declaration(self, node);
    }

    fn visit_export_from_declaration(&mut self, node: &ExportFromDeclaration<'a>) {
        self.erase_type_only(&node.specifiers);
        walk::walk_export_from_declaration(self, node);
    }
}

/// A whole statement the language erases, if this is one.
///
/// An enum, a namespace and an `import x = require(…)` are left out on purpose: each
/// of them declares something that exists while the program runs.
fn erased_whole(statement: &Statement<'_>) -> Option<Span> {
    match statement {
        // `import type { Widget } from "./widget"`, which imports no module at all.
        Statement::ImportDeclaration(node) if node.import_kind.is_type() => Some(node.span),
        Statement::ExportNamedDeclaration(node) if node.export_kind.is_type() => Some(node.span),
        Statement::ExportFromDeclaration(node) if node.export_kind.is_type() => Some(node.span),
        Statement::ExportAllDeclaration(node) if node.export_kind.is_type() => Some(node.span),

        // `export interface Widget {…}`. The export goes with what it exports: a name
        // that is not in a table resolves to the whole file, which coarsens and so
        // cannot mislead anyone reading it.
        Statement::ExportDeclaration(node) if erased_declaration(&node.declaration) => {
            Some(node.span)
        }
        Statement::ExportDefaultDeclaration(node) => matches!(
            node.declaration,
            ExportDefaultDeclarationKind::TSInterfaceDeclaration(_)
        )
        .then_some(node.span),

        statement => statement
            .as_declaration()
            .filter(|declaration| erased_declaration(declaration))
            .map(|_| statement.span()),
    }
}

/// Whether a declaration is one the language erases, exported or not.
fn erased_declaration(declaration: &Declaration<'_>) -> bool {
    match declaration {
        Declaration::TSInterfaceDeclaration(_) | Declaration::TSTypeAliasDeclaration(_) => true,

        // `declare const x: number`: a claim about something declared elsewhere.
        Declaration::VariableDeclaration(node) => node.declare,
        Declaration::ClassDeclaration(node) => node.declare,
        Declaration::TSEnumDeclaration(node) => node.declare,
        Declaration::TSNamespaceDeclaration(node) => node.declare,
        Declaration::TSExternalModuleDeclaration(_) | Declaration::TSGlobalDeclaration(_) => true,

        // An overload signature, which is a function declaration with nothing to run.
        Declaration::FunctionDeclaration(node) => node.declare || node.body.is_none(),

        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxc_allocator::Allocator;
    use oxc_parser::Parser;
    use oxc_span::SourceType;

    /// The erased source, with each run of spaces squeezed so that the shape of what
    /// is left is readable.
    fn erased(source: &str) -> String {
        let allocator = Allocator::default();
        let parsed = Parser::new(
            &allocator,
            source,
            SourceType::default().with_typescript(true),
        )
        .parse();
        assert!(parsed.diagnostics.is_empty(), "fixture should parse");

        let erased = erase(source, &parsed.program).unwrap_or_else(|| source.to_string());
        assert_eq!(erased.len(), source.len(), "offsets must survive erasure");
        assert_eq!(
            erased.lines().count(),
            source.lines().count(),
            "line numbers must survive erasure"
        );

        // What is left has to be the same language, or nothing downstream can read it.
        let allocator = Allocator::default();
        let reparsed = Parser::new(
            &allocator,
            &erased,
            SourceType::default().with_typescript(true),
        )
        .parse();
        assert!(
            reparsed.diagnostics.is_empty(),
            "erased source should parse: {erased:?}"
        );

        erased.split_whitespace().collect::<Vec<_>>().join(" ")
    }

    #[test]
    fn an_annotation_goes_and_the_value_stays() {
        assert_eq!(erased("const count: number = 1;"), "const count = 1;");
        assert_eq!(
            erased("function f(a: string, b: number): void {}"),
            "function f(a , b ) {}"
        );
    }

    #[test]
    fn type_parameters_and_arguments_go() {
        assert_eq!(
            erased("function f<T>(a: T): T { return a; }"),
            "function f (a ) { return a; }"
        );
        assert_eq!(
            erased("const m = new Map<string, number>();"),
            "const m = new Map ();"
        );
    }

    #[test]
    fn an_assertion_is_only_a_claim_about_the_value() {
        assert_eq!(erased("const a = b as unknown as C;"), "const a = b ;");
        assert_eq!(erased("const a = b satisfies C;"), "const a = b ;");
        assert_eq!(erased("const a = b!.c;"), "const a = b .c;");
    }

    #[test]
    fn a_type_only_statement_goes_whole() {
        assert_eq!(
            erased("interface Widget { size: number }\nconst a = 1;"),
            "const a = 1;"
        );
        assert_eq!(erased("type Size = number;\nconst a = 1;"), "const a = 1;");
        assert_eq!(
            erased("declare const version: string;\nconst a = 1;"),
            "const a = 1;"
        );
        // An overload signature runs nothing; the implementation below it does.
        assert_eq!(
            erased("function f(a: string): void;\nfunction f(a: unknown) {}"),
            "function f(a ) {}"
        );
    }

    #[test]
    fn an_exported_type_goes_with_its_export() {
        assert_eq!(
            erased("export interface Widget { size: number }\nconst a = 1;"),
            "const a = 1;"
        );
        assert_eq!(
            erased("export type Size = number;\nconst a = 1;"),
            "const a = 1;"
        );
        assert_eq!(
            erased("export declare const version: string;\nconst a = 1;"),
            "const a = 1;"
        );
        assert_eq!(
            erased("export function f(a: string): void;\nexport function f(a: unknown) {}"),
            "export function f(a ) {}"
        );
        // What is exported and also runs stays exported.
        assert_eq!(
            erased("export enum Size { Small }"),
            "export enum Size { Small }"
        );
    }

    #[test]
    fn a_type_only_import_is_not_an_import() {
        assert_eq!(
            erased("import type { Widget } from \"./widget\";\nconst a = 1;"),
            "const a = 1;"
        );
        // The comma that held the name there goes with it, wherever in the list it
        // stood.
        assert_eq!(
            erased("import { widget, type Widget } from \"./widget\";"),
            "import { widget } from \"./widget\";"
        );
        assert_eq!(
            erased("import { type Widget, widget } from \"./widget\";"),
            "import { widget } from \"./widget\";"
        );
        assert_eq!(
            erased("import { a, type B, type C, d } from \"./widget\";"),
            "import { a, d } from \"./widget\";"
        );
        assert_eq!(
            erased("import widget, { type Widget } from \"./widget\";"),
            "import widget, { } from \"./widget\";"
        );
        assert_eq!(erased("export { a, type B };"), "export { a };");
        assert_eq!(erased("export type { Widget };"), "");
    }

    #[test]
    fn an_annotation_holding_an_arrow_together_stays() {
        // Nothing may come between an arrow's parameters and its `=>` but spaces,
        // so an annotation written across lines is left where it is.
        let held = "const f = (): {\n  a: number\n} => ({ a: 1 });";
        assert_eq!(erased(held), "const f = (): { a: number } => ({ a: 1 });");
        // One on a single line is holding nothing apart.
        assert_eq!(erased("const f = (): number => 1;"), "const f = () => 1;");
    }

    #[test]
    fn what_runs_is_left_alone() {
        // An enum, a namespace and an import-equals all exist while the program runs.
        let enumeration = "enum Size { Small }";
        assert_eq!(erased(enumeration), enumeration);
        let namespace = "namespace app { export const a = 1; }";
        assert_eq!(erased(namespace), namespace);
        // A class implementing nothing is not a class, so the keyword goes too.
        assert_eq!(
            erased("class A implements B, C { x = 1; }"),
            "class A { x = 1; }"
        );
        // A parameter property assigns a field, however type-like it reads.
        let property = "class A { constructor(private readonly a: number) {} }";
        assert_eq!(
            erased(property),
            "class A { constructor(private readonly a ) {} }"
        );
    }

    #[test]
    fn a_file_with_no_types_is_left_as_it_was() {
        let allocator = Allocator::default();
        let source = "const a = 1;";
        let parsed = Parser::new(&allocator, source, SourceType::default()).parse();
        assert!(erase(source, &parsed.program).is_none());
    }
}
