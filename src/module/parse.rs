//! Parsing a file and deciding whether it can be described finely.

use std::fs;
use std::path::Path;

use oxc_allocator::Allocator;
use oxc_ast::ast::*;
use oxc_ast_visit::{Visit, walk};
use oxc_parser::Parser as OxcParser;
use oxc_semantic::{Semantic, SemanticBuilder};
use oxc_span::{GetSpan, SourceType};

use super::{Decl, FineModule, ModuleAnalysis, Span, decls, exports, init, refs};

/// Extensions we parse for further imports. Anything else that resolves — images,
/// fonts, stylesheets, JSON — is a leaf: it can be reported as affected, but it is
/// never opened looking for dependencies of its own.
pub const SOURCE_EXTENSIONS: &[&str] = &["ts", "tsx", "mts", "cts", "js", "jsx", "mjs", "cjs"];

pub fn is_source_file(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| SOURCE_EXTENSIONS.contains(&ext))
}

/// Byte offsets of the start of each line, so a diff's line numbers can be compared
/// against AST spans.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LineTable {
    starts: Vec<u32>,
    len: u32,
}

impl LineTable {
    pub fn new(source: &str) -> Self {
        let mut starts = vec![0];
        for (offset, byte) in source.bytes().enumerate() {
            if byte == b'\n' {
                starts.push(offset as u32 + 1);
            }
        }
        Self {
            starts,
            len: source.len() as u32,
        }
    }

    pub fn line_count(&self) -> u32 {
        self.starts.len() as u32
    }

    /// Byte offset where 1-based `line` begins. Lines past the end clamp to the end.
    pub fn line_start(&self, line: u32) -> u32 {
        if line == 0 {
            return 0;
        }
        self.starts
            .get(line as usize - 1)
            .copied()
            .unwrap_or(self.len)
    }

    /// Byte offset just past the end of 1-based `line`.
    pub fn line_end(&self, line: u32) -> u32 {
        self.starts.get(line as usize).copied().unwrap_or(self.len)
    }
}

pub(crate) struct Ctx<'a> {
    pub semantic: &'a Semantic<'a>,
    /// Top-level statement spans, in source order.
    pub statements: Vec<Span>,
}

impl Ctx<'_> {
    /// The top-level statement containing `offset`, if any.
    pub fn statement_at(&self, offset: u32) -> Option<usize> {
        self.statements.iter().position(|s| s.contains(offset))
    }
}

pub fn analyse_file(path: &Path) -> Option<(ModuleAnalysis, LineTable)> {
    if !is_source_file(path) {
        return None;
    }

    let source_text = fs::read_to_string(path).ok()?;
    let line_table = LineTable::new(&source_text);

    let allocator = Allocator::default();
    let source_type = SourceType::from_path(path).unwrap_or_default();
    let parsed = OxcParser::new(&allocator, &source_text, source_type).parse();

    let sources = collect_sources(&parsed.program);

    // A parse error means the AST is a guess, so nothing finer than the file is safe.
    if !parsed.diagnostics.is_empty() {
        return Some((ModuleAnalysis::Coarse { sources }, line_table));
    }

    if let Some(reason) = coarsening_reason(&parsed.program) {
        let _ = reason;
        return Some((ModuleAnalysis::Coarse { sources }, line_table));
    }

    let semantic = SemanticBuilder::new()
        // Reference spans are read back through the node table, which is opt-in.
        .with_build_nodes(true)
        .build(&parsed.program)
        .semantic;

    let analysis = match build_fine(&parsed.program, &semantic, &sources) {
        Some(module) => ModuleAnalysis::Fine(Box::new(module)),
        None => ModuleAnalysis::Coarse { sources },
    };
    Some((analysis, line_table))
}

fn build_fine(
    program: &Program<'_>,
    semantic: &Semantic<'_>,
    sources: &[String],
) -> Option<FineModule> {
    let statements = program
        .body
        .iter()
        .map(|statement| span_of(statement.span()))
        .collect();
    let ctx = Ctx {
        semantic,
        statements,
    };

    let drafts = decls::collect(program)?;
    let imports = decls::collect_imports(program, sources)?;
    let (exports, export_stars) = exports::collect(program, sources, &drafts)?;

    let mut decls: Vec<Decl> = drafts
        .iter()
        .map(|draft| Decl {
            name: draft.name.clone(),
            span: draft.span,
            refs: Vec::new(),
            imports: Vec::new(),
        })
        .collect();

    let import_spans = refs::link(&ctx, &drafts, &imports, &mut decls);
    let init_decls = init::collect(&ctx, program, &drafts, &decls);
    let bare_sources = decls::bare_sources(program, sources);

    Some(FineModule {
        decls,
        exports,
        export_stars,
        sources: sources.to_vec(),
        init_decls,
        bare_sources,
        import_spans,
    })
}

pub(crate) fn span_of(span: oxc_span::Span) -> Span {
    Span {
        start: span.start,
        end: span.end,
    }
}

/// Syntax that makes a declaration-level description unsafe. Each of these gets its
/// own step later; until then the whole file is one node.
fn coarsening_reason(program: &Program<'_>) -> Option<&'static str> {
    let mut detector = Coarsener { reason: None };
    detector.visit_program(program);
    detector.reason
}

struct Coarsener {
    reason: Option<&'static str>,
}

impl Coarsener {
    fn flag(&mut self, reason: &'static str) {
        if self.reason.is_none() {
            self.reason = Some(reason);
        }
    }
}

impl<'a> Visit<'a> for Coarsener {
    fn visit_call_expression(&mut self, expr: &CallExpression<'a>) {
        if let Expression::Identifier(ident) = &expr.callee {
            match ident.name.as_str() {
                // CommonJS has no export table we can read yet.
                "require" => self.flag("require"),
                // `eval` can reach any binding in scope by name.
                "eval" => self.flag("eval"),
                _ => {}
            }
        }
        walk::walk_call_expression(self, expr);
    }

    fn visit_member_expression(&mut self, expr: &MemberExpression<'a>) {
        // `module.exports` / `exports.x` are an export table we cannot read yet.
        if let MemberExpression::StaticMemberExpression(member) = expr {
            if let Expression::Identifier(object) = &member.object {
                if object.name == "module" && member.property.name == "exports" {
                    self.flag("module.exports");
                } else if object.name == "exports" {
                    self.flag("exports.*");
                }
            }
        }
        walk::walk_member_expression(self, expr);
    }

    fn visit_with_statement(&mut self, statement: &WithStatement<'a>) {
        self.flag("with");
        walk::walk_with_statement(self, statement);
    }

    fn visit_ts_namespace_declaration(&mut self, declaration: &TSNamespaceDeclaration<'a>) {
        // A namespace merges declarations across statements and files.
        self.flag("namespace");
        walk::walk_ts_namespace_declaration(self, declaration);
    }

    fn visit_decorator(&mut self, decorator: &Decorator<'a>) {
        // A decorator can rewrite the thing it decorates.
        self.flag("decorator");
        walk::walk_decorator(self, decorator);
    }
}

/// Every import specifier in the file, deduplicated, in source order.
///
/// This works whether or not the module can be described finely, which is what lets a
/// coarse module still have outgoing edges.
fn collect_sources(program: &Program<'_>) -> Vec<String> {
    let mut extractor = SpecifierExtractor {
        specifiers: Vec::with_capacity(32),
    };
    extractor.visit_program(program);

    let mut sources: Vec<String> = Vec::with_capacity(extractor.specifiers.len());
    for specifier in extractor.specifiers {
        let specifier = specifier.to_string();
        if !sources.contains(&specifier) {
            sources.push(specifier);
        }
    }
    sources
}

struct SpecifierExtractor<'a> {
    specifiers: Vec<&'a str>,
}

impl<'a> Visit<'a> for SpecifierExtractor<'a> {
    fn visit_import_declaration(&mut self, decl: &ImportDeclaration<'a>) {
        self.specifiers.push(decl.source.value.as_str());
    }

    fn visit_export_from_declaration(&mut self, decl: &ExportFromDeclaration<'a>) {
        self.specifiers.push(decl.source.value.as_str());
    }

    fn visit_export_all_declaration(&mut self, decl: &ExportAllDeclaration<'a>) {
        self.specifiers.push(decl.source.value.as_str());
    }

    fn visit_import_expression(&mut self, expr: &ImportExpression<'a>) {
        if let Expression::StringLiteral(lit) = &expr.source {
            self.specifiers.push(lit.value.as_str());
        }
        walk::walk_import_expression(self, expr);
    }

    fn visit_new_expression(&mut self, expr: &NewExpression<'a>) {
        // `new URL("./logo.png", import.meta.url)` is the bundler-agnostic way to
        // reference an asset. Only relative specifiers are edges; `new URL(absolute)`
        // is an ordinary runtime URL.
        if let Expression::Identifier(ident) = &expr.callee {
            if ident.name == "URL" && expr.arguments.len() >= 2 {
                if let Some(Expression::StringLiteral(lit)) =
                    expr.arguments.first().and_then(|arg| arg.as_expression())
                {
                    let value = lit.value.as_str();
                    if value.starts_with("./") || value.starts_with("../") {
                        self.specifiers.push(value);
                    }
                }
            }
        }
        walk::walk_new_expression(self, expr);
    }

    fn visit_call_expression(&mut self, expr: &CallExpression<'a>) {
        if let Expression::Identifier(ident) = &expr.callee {
            if ident.name == "require" {
                if let Some(arg) = expr.arguments.first() {
                    if let Expression::StringLiteral(lit) = arg.to_expression() {
                        self.specifiers.push(lit.value.as_str());
                    }
                }
            }
        }
        walk::walk_call_expression(self, expr);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_table_maps_lines_to_offsets() {
        let table = LineTable::new("a\nbb\nccc");
        assert_eq!(table.line_count(), 3);
        assert_eq!(table.line_start(1), 0);
        assert_eq!(table.line_end(1), 2);
        assert_eq!(table.line_start(2), 2);
        assert_eq!(table.line_end(2), 5);
        assert_eq!(table.line_start(3), 5);
        assert_eq!(table.line_end(3), 8);
    }

    #[test]
    fn line_table_clamps_past_the_end() {
        let table = LineTable::new("a\n");
        assert_eq!(table.line_start(99), 2);
        assert_eq!(table.line_end(99), 2);
    }

    #[test]
    fn spans_intersect_inclusively() {
        let span = Span { start: 10, end: 20 };
        assert!(span.intersects(5, 15));
        assert!(span.intersects(15, 25));
        assert!(span.intersects(10, 20));
        assert!(!span.intersects(0, 10));
        assert!(!span.intersects(20, 30));
        // A pure deletion is zero-width and lands inside or it does not.
        assert!(span.intersects(15, 15));
        assert!(!span.intersects(20, 20));
    }
}
