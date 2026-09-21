//! Reading a single module: what does this file import?

use std::fs;
use std::path::Path;

use oxc_allocator::Allocator;
use oxc_ast::ast::*;
use oxc_ast_visit::{Visit, walk};
use oxc_parser::Parser as OxcParser;
use oxc_span::SourceType;

/// Extensions we parse for further imports. Anything else that resolves — images,
/// fonts, stylesheets, JSON — is a leaf: it can be reported as affected, but it is
/// never opened looking for dependencies of its own.
pub const SOURCE_EXTENSIONS: &[&str] = &["ts", "tsx", "mts", "cts", "js", "jsx", "mjs", "cjs"];

pub fn is_source_file(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| SOURCE_EXTENSIONS.contains(&ext))
}

/// Every import specifier written in `file_path`, in source order.
///
/// `None` means the file has no outgoing edges to offer: it is a leaf, or it could
/// not be read.
pub fn imported_specifiers(file_path: &Path) -> Option<Vec<String>> {
    if !is_source_file(file_path) {
        return None;
    }

    let source_text = fs::read_to_string(file_path).ok()?;
    let allocator = Allocator::default();
    let source_type = SourceType::from_path(file_path).unwrap_or_default();
    let ret = OxcParser::new(&allocator, &source_text, source_type).parse();

    let mut extractor = SpecifierExtractor {
        specifiers: Vec::with_capacity(32),
    };
    extractor.visit_program(&ret.program);

    Some(
        extractor
            .specifiers
            .into_iter()
            .map(|s| s.to_string())
            .collect(),
    )
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
