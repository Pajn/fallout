//! Parsing a file and deciding whether it can be described finely.

use std::fs;
use std::path::Path;

use ahash::AHashSet;
use oxc_allocator::Allocator;
use oxc_ast::ast::*;
use oxc_ast_visit::{Visit, walk};
use oxc_parser::Parser as OxcParser;
use oxc_semantic::{Semantic, SemanticBuilder};
use oxc_span::{GetSpan, SourceType};

use super::{
    Decl, FineModule, ModuleAnalysis, Reading, Span, cjs, decls, exports, factories, init, members,
    refs, types,
};

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
    /// Lines with nothing left on them once types were erased, by 0-based index.
    ///
    /// Empty when nothing was erased, which is also how "do not ask" is spelled: a
    /// blank line in a file read as it was written is just a blank line, and marking
    /// nothing for it would be a narrowing nobody asked for.
    runtime_blank: Vec<bool>,
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
            runtime_blank: Vec::new(),
        }
    }

    /// A table over source whose type-only syntax has been blanked out, which also
    /// records which lines that left with nothing on them.
    pub fn erased(source: &str) -> Self {
        let mut table = Self::new(source);
        table.runtime_blank = source.lines().map(|line| line.trim().is_empty()).collect();
        table
    }

    /// Whether every line from `start` for `len` lines has nothing on it that runs.
    ///
    /// Always false without an erasure to judge by, and false for a line that holds
    /// a type and a value both: which of the two a diff touched is not a question
    /// line numbers can answer. A range with no lines in it is false for the same
    /// reason — what a change removed is not there to be read.
    pub fn runs_nothing(&self, start: u32, len: u32) -> bool {
        if self.runtime_blank.is_empty() || len == 0 {
            return false;
        }
        (start..start + len).all(|line| {
            self.runtime_blank
                .get(line.saturating_sub(1) as usize)
                .copied()
                .unwrap_or(false)
        })
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
        // Statements are in source order and do not overlap, and this is asked of
        // every reference in the file.
        let index = self
            .statements
            .partition_point(|statement| statement.end <= offset);
        self.statements
            .get(index)
            .is_some_and(|statement| statement.contains(offset))
            .then_some(index)
    }
}

pub fn analyse_file(path: &Path, reading: &Reading) -> Option<(ModuleAnalysis, LineTable)> {
    // A stylesheet is read for its imports and nothing else, so it never reaches the
    // JavaScript parser. See [`super::style`] for why it is always coarse.
    if super::style::is_style_file(path) {
        let source = fs::read_to_string(path).ok()?;
        let analysis = super::style::analyse(path, &source)?;
        return Some((analysis, LineTable::new(&source)));
    }
    if !is_source_file(path) {
        return None;
    }
    analyse_source(path, &fs::read_to_string(path).ok()?, reading)
}

/// Analyses text as if it were the contents of `path`.
///
/// `path` decides the dialect and nothing else, so an earlier version of a file can
/// be analysed without being written anywhere.
pub fn analyse_source(
    path: &Path,
    source_text: &str,
    reading: &Reading,
) -> Option<(ModuleAnalysis, LineTable)> {
    if !is_source_file(path) {
        return None;
    }

    let allocator = Allocator::default();
    let source_type = SourceType::from_path(path).unwrap_or_default();
    let parsed = OxcParser::new(&allocator, source_text, source_type).parse();

    // A parse error means the AST is a guess, so nothing finer than the file is safe.
    if !parsed.diagnostics.is_empty() {
        return Some((
            ModuleAnalysis::Coarse {
                sources: collect_sources(&parsed.program),
            },
            LineTable::new(source_text),
        ));
    }

    // Erasing leaves the file the same length and the same shape, so everything below
    // reads spans and lines exactly as it would have. What it must not do is leave
    // behind something that is no longer the language: if it has, read the original.
    let erased = reading
        .ignore_types
        .then(|| types::erase(source_text, &parsed.program))
        .flatten();
    let reparsed = erased
        .as_deref()
        .map(|erased| OxcParser::new(&allocator, erased, source_type).parse());
    let (erased_text, parsed) = match (&erased, &reparsed) {
        (Some(erased), Some(reparsed)) if reparsed.diagnostics.is_empty() => {
            (Some(erased.as_str()), reparsed)
        }
        _ => (None, &parsed),
    };

    let line_table = match erased_text {
        Some(erased) => LineTable::erased(erased),
        None => LineTable::new(source_text),
    };
    let sources = collect_sources(&parsed.program);

    // Read first, so the coarsener knows which mentions of `module` and `exports` an
    // export table it could read has already spoken for.
    let cjs = cjs::table(&parsed.program);

    if let Some(reason) = coarsening_reason(&parsed.program, &cjs.accounted) {
        let _ = reason;
        return Some((ModuleAnalysis::Coarse { sources }, line_table));
    }

    let semantic = SemanticBuilder::new()
        // Reference spans are read back through the node table, which is opt-in.
        .with_build_nodes(true)
        .build(&parsed.program)
        .semantic;

    // Which callees count as pure is the claim of the files above this one, not of
    // the run: a package may call its own factory pure without saying so for an app.
    let chain = reading.configs.chain(path);
    let analysis = match build_fine(&parsed.program, &semantic, &sources, chain.pure(), &cjs) {
        Some(module) => ModuleAnalysis::Fine(Box::new(module)),
        None => ModuleAnalysis::Coarse { sources },
    };
    Some((analysis, line_table))
}

fn build_fine(
    program: &Program<'_>,
    semantic: &Semantic<'_>,
    sources: &[String],
    pure: &crate::pure::PureList,
    cjs: &cjs::Table,
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

    let drafts = decls::collect(program, cjs)?;
    let imports = decls::collect_imports(program, sources)?;
    let (exports, export_stars) = exports::collect(program, sources, &drafts, &imports, cjs)?;

    let mut decls: Vec<Decl> = drafts
        .iter()
        .map(|draft| Decl {
            name: draft.name.clone(),
            span: draft.span,
            refs: Vec::new(),
            member_refs: Vec::new(),
            imports: Vec::new(),
            members: Vec::new(),
            interior: Span::default(),
            derived: None,
            factory: None,
        })
        .collect();

    let mut objects = members::find(&ctx, program, &drafts, cjs);
    // A call's result read by property is linked the way an object's is, and is
    // never read as one only for its members: calling it is not a read of one.
    let candidates = factories::find(&ctx, program, &drafts, &imports);
    for &(symbol, decl) in &candidates.by_symbol {
        objects
            .by_symbol
            .entry(symbol)
            .or_insert(members::ObjectRef {
                decl,
                callable: Vec::new(),
            });
    }
    let (import_spans, shared) =
        refs::link(&ctx, &drafts, &imports, &objects.by_symbol, &mut decls);
    let requires = decls::require_calls(program, sources)?;
    let init_requires = refs::attach_requires(&ctx, &drafts, &requires, &mut decls);
    let init_dynamic = refs::attach_dynamic_imports(&ctx, &drafts, sources, &mut decls)?;
    let conditional = candidates.conditional.clone();
    let by_symbol = objects.by_symbol.clone();
    members::attach(&ctx, &drafts, &imports, objects, &shared, &mut decls);
    factories::attach(
        &ctx, &drafts, &imports, &by_symbol, candidates, &shared, &mut decls,
    );
    let origins = init::origins(&imports, sources);
    let (init_decls, conditional_init) =
        init::collect(&ctx, program, &drafts, &decls, &origins, pure, &conditional);

    // A `require` outside every declaration runs on evaluation, exactly like a bare
    // `import "./x"`, so the two share a list.
    let mut bare_sources = decls::bare_sources(program, sources);
    for source in init_requires.into_iter().chain(init_dynamic) {
        if !bare_sources.contains(&source) {
            bare_sources.push(source);
        }
    }

    Some(FineModule {
        decls,
        exports,
        export_stars,
        sources: sources.to_vec(),
        init_decls,
        bare_sources,
        import_spans,
        conditional_init,
    })
}

pub(crate) fn span_of(span: oxc_span::Span) -> Span {
    Span {
        start: span.start,
        end: span.end,
    }
}

/// Is this a call to `require`? A local binding of that name is not ruled out: the
/// worst a wrong guess does is add an edge, and an extra edge only widens the answer.
pub(crate) fn is_require(expr: &CallExpression<'_>) -> bool {
    matches!(&expr.callee, Expression::Identifier(ident) if ident.name == "require")
}

/// What `Object.freeze(x)` freezes, when `Object` is the global.
///
/// Without `scoping` the callee is taken by its spelling. Only a caller whose answer
/// is checked again against the file's bindings may ask that way: the comparison
/// against a base revision, which reports where an object changed but leaves it to
/// the analysis of the file to decide whether the object is read by property.
pub(crate) fn frozen<'e, 'a>(
    call: &'e CallExpression<'a>,
    scoping: Option<&oxc_semantic::Scoping>,
) -> Option<&'e Expression<'a>> {
    use oxc_semantic::IsGlobalReference;

    let Expression::StaticMemberExpression(callee) = &call.callee else {
        return None;
    };
    let Expression::Identifier(object) = &callee.object else {
        return None;
    };
    let global = match scoping {
        Some(scoping) => object.is_global_reference_name("Object".into(), scoping),
        None => object.name == "Object",
    };
    let [argument] = call.arguments.as_slice() else {
        return None;
    };
    (global && callee.property.name == "freeze")
        .then(|| argument.as_expression())
        .flatten()
}

/// The module a call names, when its first argument spells one as a plain string.
pub(crate) fn static_specifier<'a>(expr: &CallExpression<'a>) -> Option<&'a str> {
    match expr.arguments.first()?.as_expression()? {
        Expression::StringLiteral(lit) => Some(lit.value.as_str()),
        _ => None,
    }
}

/// Syntax that makes a declaration-level description unsafe. Each of these gets its
/// own step later; until then the whole file is one node.
fn coarsening_reason<'a>(
    program: &Program<'a>,
    accounted: &'a AHashSet<Span>,
) -> Option<&'static str> {
    let mut detector = Coarsener {
        reason: None,
        accounted,
    };
    detector.visit_program(program);
    detector.reason
}

struct Coarsener<'a> {
    reason: Option<&'static str>,
    /// Mentions of `module` and `exports` that a readable export table accounts for.
    accounted: &'a AHashSet<Span>,
}

impl Coarsener<'_> {
    fn flag(&mut self, reason: &'static str) {
        if self.reason.is_none() {
            self.reason = Some(reason);
        }
    }
}

impl<'a> Visit<'a> for Coarsener<'_> {
    fn visit_call_expression(&mut self, expr: &CallExpression<'a>) {
        if is_require(expr) {
            // A `require("./x")` naming its module in a plain string is an ordinary
            // dependency, recorded against the declaration that contains it. Only an
            // unreadable specifier leaves the target unknown.
            if static_specifier(expr).is_none() {
                self.flag("dynamic require");
            }
        } else if let Expression::Identifier(ident) = &expr.callee {
            // `eval` can reach any binding in scope by name.
            if ident.name == "eval" {
                self.flag("eval");
            }
        }
        walk::walk_call_expression(self, expr);
    }

    fn visit_import_expression(&mut self, expr: &ImportExpression<'a>) {
        // Same rule as `require`: a computed specifier is an edge we cannot record.
        if !matches!(&expr.source, Expression::StringLiteral(_)) {
            self.flag("dynamic import");
        }
        walk::walk_import_expression(self, expr);
    }

    fn visit_member_expression(&mut self, expr: &MemberExpression<'a>) {
        // A mention of `module.exports` or `exports.x` that the export table did not
        // account for is a way of writing one we cannot read: a computed key, an
        // `Object.assign`, a table handed to someone else, a table assigned from
        // inside a branch.
        if let MemberExpression::StaticMemberExpression(member) = expr
            && !self.accounted.contains(&span_of(member.span))
            && let Expression::Identifier(object) = &member.object
        {
            if object.name == "module" && member.property.name == "exports" {
                self.flag("module.exports");
            } else if object.name == "exports" {
                self.flag("exports.*");
            }
        }
        walk::walk_member_expression(self, expr);
    }

    fn visit_identifier_reference(&mut self, identifier: &IdentifierReference<'a>) {
        // The table named on its own, rather than through a property: handed to
        // `Object.assign`, to a compiler's `__exportStar` helper, to anyone. What
        // they do with it is theirs, so the table is not ours to describe.
        if (identifier.name == "module" || identifier.name == "exports")
            && !self.accounted.contains(&span_of(identifier.span))
        {
            self.flag("the export table by name");
        }
        walk::walk_identifier_reference(self, identifier);
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
        if let Expression::Identifier(ident) = &expr.callee
            && ident.name == "URL"
            && expr.arguments.len() >= 2
            && let Some(Expression::StringLiteral(lit)) =
                expr.arguments.first().and_then(|arg| arg.as_expression())
        {
            let value = lit.value.as_str();
            if value.starts_with("./") || value.starts_with("../") {
                self.specifiers.push(value);
            }
        }
        walk::walk_new_expression(self, expr);
    }

    fn visit_call_expression(&mut self, expr: &CallExpression<'a>) {
        if is_require(expr)
            && let Some(specifier) = static_specifier(expr)
        {
            self.specifiers.push(specifier);
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
