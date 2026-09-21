use std::collections::VecDeque;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::{Arc, RwLock};

use ahash::{AHashMap, AHashSet};
use clap::Parser as ClapParser;

use oxc_allocator::Allocator;
use oxc_ast::ast::*;
use oxc_ast_visit::{Visit, walk};
use oxc_parser::Parser as OxcParser;
use oxc_resolver::{ResolveOptions, Resolver, TsconfigDiscovery};
use oxc_span::SourceType;

/// Extensions we parse for further imports. Anything else that resolves — images,
/// fonts, stylesheets, JSON — is a leaf: it can be reported as affected, but it is
/// never opened looking for dependencies of its own.
const SOURCE_EXTENSIONS: &[&str] = &["ts", "tsx", "mts", "cts", "js", "jsx", "mjs", "cjs"];

#[derive(ClapParser)]
struct Cli {
    /// Target page component(s) (e.g., src/pages/CheckoutPage.tsx)
    #[arg(short, long, value_delimiter = ' ')]
    anchor: Vec<PathBuf>,

    /// Changed files passed from git diff
    #[arg(short, long, value_delimiter = ' ')]
    changed: Vec<PathBuf>,

    /// Root directory to scan for source files (default: anchor's parent or current dir)
    #[arg(short, long)]
    root: Option<PathBuf>,
}

fn main() -> ExitCode {
    let cli = Cli::parse();

    if cli.anchor.is_empty() {
        eprintln!("Error: At least one anchor must be provided");
        return ExitCode::FAILURE;
    }

    let root = cli.root.as_deref().unwrap_or(Path::new("."));
    let root_canonical = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());

    let options = ResolveOptions {
        extensions: vec![
            ".tsx".to_string(),
            ".ts".to_string(),
            ".cts".to_string(),
            ".mts".to_string(),
            ".jsx".to_string(),
            ".js".to_string(),
            ".mjs".to_string(),
            ".cjs".to_string(),
            ".json".to_string(),
        ],
        tsconfig: Some(TsconfigDiscovery::Auto),
        ..ResolveOptions::default()
    };
    let resolver = Arc::new(Resolver::new(options));
    let resolve_cache = Arc::new(RwLock::new(
        AHashMap::<(PathBuf, String), Option<PathBuf>>::default(),
    ));

    let mut anchor_files = Vec::new();
    let mut missing_anchors = Vec::new();

    for anchor in &cli.anchor {
        let anchor_path = if anchor.is_relative() {
            root_canonical.join(anchor)
        } else {
            anchor.clone()
        };
        let anchor_canonical = anchor_path.canonicalize().unwrap_or_else(|_| anchor_path);
        if anchor_canonical.exists() {
            anchor_files.push(anchor_canonical);
        } else {
            missing_anchors.push(anchor_canonical.display().to_string());
        }
    }

    if !missing_anchors.is_empty() {
        eprintln!("Error: Anchor(s) not found: {}", missing_anchors.join(", "));
        return ExitCode::FAILURE;
    }

    let changed_files: AHashSet<PathBuf> = cli
        .changed
        .iter()
        .filter_map(|p| p.canonicalize().ok())
        .collect();

    if changed_files.is_empty() {
        println!("No reachability impact detected");
        return ExitCode::FAILURE;
    }

    let (downstream_hit, changed_file) = check_downstream(
        &anchor_files,
        &changed_files,
        &resolver,
        &resolve_cache,
        root,
    );
    if downstream_hit {
        if let Some(cf) = changed_file {
            println!("Impact detected on target anchor via: {:?}", cf);
        } else {
            println!("Impact detected on target anchor (downstream)");
        }
        return ExitCode::SUCCESS;
    }

    let (upstream_hit, changed_file) = check_upstream(
        &anchor_files,
        &changed_files,
        &resolver,
        &resolve_cache,
        root,
    );
    if upstream_hit {
        if let Some(cf) = changed_file {
            println!("Impact detected on target anchor via: {:?}", cf);
        } else {
            println!("Impact detected on target anchor (upstream)");
        }
        return ExitCode::SUCCESS;
    }

    println!("No reachability impact detected");
    ExitCode::FAILURE
}

fn check_downstream(
    anchors: &[PathBuf],
    changed: &AHashSet<PathBuf>,
    resolver: &Arc<Resolver>,
    resolve_cache: &Arc<RwLock<AHashMap<(PathBuf, String), Option<PathBuf>>>>,
    _root: &Path,
) -> (bool, Option<PathBuf>) {
    let mut visited = AHashSet::default();
    let mut queue = VecDeque::new();

    for anchor in anchors {
        queue.push_back(anchor.clone());
        visited.insert(anchor.clone());
    }

    while let Some(current) = queue.pop_front() {
        if changed.contains(&current) {
            return (true, Some(current));
        }

        let specifiers = match extract_specifiers(&current) {
            Some(s) => s,
            None => continue,
        };
        for specifier in specifiers {
            if let Some(resolved) = resolve_import(resolver, resolve_cache, &current, &specifier) {
                if visited.insert(resolved.clone()) {
                    queue.push_back(resolved);
                }
            }
        }
    }
    (false, None)
}

fn check_upstream(
    anchors: &[PathBuf],
    changed: &AHashSet<PathBuf>,
    resolver: &Arc<Resolver>,
    resolve_cache: &Arc<RwLock<AHashMap<(PathBuf, String), Option<PathBuf>>>>,
    _root: &Path,
) -> (bool, Option<PathBuf>) {
    let anchor_set: AHashSet<PathBuf> = anchors.iter().cloned().collect();

    for changed_file in changed {
        let mut visited = AHashSet::default();
        let mut queue = VecDeque::new();
        queue.push_back(changed_file.clone());
        visited.insert(changed_file.clone());

        while let Some(current) = queue.pop_front() {
            if anchor_set.contains(&current) {
                return (true, Some(changed_file.clone()));
            }

            let specifiers = match extract_specifiers(&current) {
                Some(s) => s,
                None => continue,
            };
            for specifier in specifiers {
                if let Some(resolved) =
                    resolve_import(resolver, resolve_cache, &current, &specifier)
                {
                    if visited.insert(resolved.clone()) {
                        queue.push_back(resolved);
                    }
                }
            }
        }
    }
    (false, None)
}

fn extract_specifiers(file_path: &Path) -> Option<Vec<String>> {
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

fn is_source_file(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| SOURCE_EXTENSIONS.contains(&ext))
}

/// Webpack lets an import name its loaders inline, as in `!!file-loader!./logo.png`.
/// Only the trailing request is a path. Returns `None` when there is nothing to strip.
///
/// A `?query` or `#fragment` suffix needs no such treatment: the resolver parses those
/// itself, which is why the resolved path is read back without them.
fn strip_inline_loaders(specifier: &str) -> Option<&str> {
    if !specifier.contains('!') {
        return None;
    }

    let request = specifier.rsplit('!').next().unwrap_or(specifier);
    if request.is_empty() || request == specifier {
        None
    } else {
        Some(request)
    }
}

fn resolve_import(
    resolver: &Arc<Resolver>,
    cache: &Arc<RwLock<AHashMap<(PathBuf, String), Option<PathBuf>>>>,
    from_file: &Path,
    specifier: &str,
) -> Option<PathBuf> {
    let cache_key = (from_file.to_path_buf(), specifier.to_string());
    if let Some(cached) = cache.read().unwrap().get(&cache_key) {
        return cached.clone();
    }
    let mut result = resolver
        .resolve_file(from_file, specifier)
        .ok()
        .map(|r| r.into_path_buf());
    if result.is_none() {
        if let Some(request) = strip_inline_loaders(specifier) {
            result = resolver
                .resolve_file(from_file, request)
                .ok()
                .map(|r| r.into_path_buf());
        }
    }
    cache.write().unwrap().insert(cache_key, result.clone());
    result
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
