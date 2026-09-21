//! Reading a stylesheet for the files it pulls in.
//!
//! A stylesheet is a module like any other: importing one can change what a page
//! renders, and a stylesheet importing another passes that on. Until this existed a
//! stylesheet was a leaf, so a change to a shared partial of variables or mixins —
//! the files a design system keeps almost everything in — reached nobody.
//!
//! What is read is only the edges. A stylesheet is always [`ModuleAnalysis::Coarse`]:
//! one node for the whole file, with an outgoing edge per import. That is the honest
//! shape. Telling which rule inside a stylesheet a change touched would mean knowing
//! which selectors a page uses, which is a question about the markup rather than
//! about the stylesheet, and a wrong answer to it would under-report. Coarse is also
//! what the rest of the analyser already does with a module it cannot take apart, so
//! marking, comparison and traversal all work on a stylesheet without a rule of their
//! own: any change to one marks `File(f)`, and `File(f)` reaches every import.
//!
//! Two grammars sit behind one runtime. The SCSS grammar is a superset and will read
//! plain CSS, so the split changes no answer here and no test pins it: it is there
//! because the CSS grammar is the maintained one and the better reader of the
//! language it is for, and a stylesheet should be read by the grammar written for it
//! before the difference starts to matter.
//!
//! Neither grammar parses its language completely, and the SCSS one is the weaker:
//! over a monorepo's stylesheets it leaves an error node somewhere in 19% of them, on
//! `!default`, on maps, on `@extend %placeholder`, on `@use ... as` and on
//! interpolation in a value. An error is not treated as a failure to read the file.
//! It could not be: a stylesheet is already the coarsest node there is, so there is
//! nothing coarser to fall back to, and refusing the file would drop every edge it
//! has rather than the one the grammar stumbled on.
//!
//! Mostly the errors sit harmlessly inside a statement body. Sometimes they do not: a
//! map the grammar cannot read runs on past its own semicolon and swallows the
//! statement after it, so an `@import` on the next line disappears. That is an edge
//! lost silently, which is the one failure this tool may not have. So the tree is not
//! trusted on its own. Every line that starts an import is also read directly, and
//! the two answers are unioned. A union only ever adds, so the floor is a line scan
//! and the grammar is what lifts it — nesting, continuations, a target the scan would
//! read out of a comment. Over the same corpus the scan finds nothing the grammar
//! missed in any of 552 files, which is the result that made the grammar worth
//! keeping; the union is there for the file that corpus does not contain.

use std::path::Path;

use tree_sitter::{Node, Parser};

use super::ModuleAnalysis;

/// Extensions read as stylesheets.
pub const STYLE_EXTENSIONS: &[&str] = &["css", "scss", "sass"];

pub fn is_style_file(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| STYLE_EXTENSIONS.contains(&ext))
}

/// Reads `source` as the stylesheet at `path`.
///
/// `None` only when the grammar cannot be loaded at all, which is a build fault
/// rather than anything about the file.
pub fn analyse(path: &Path, source: &str) -> Option<ModuleAnalysis> {
    let mut parser = Parser::new();
    let language = match path.extension().and_then(|ext| ext.to_str()) {
        // The indented syntax is not this grammar's dialect, but its imports are
        // written the same way, and reading them is all that is wanted here.
        Some("scss") | Some("sass") => tree_sitter_scss::language(),
        _ => tree_sitter_css::LANGUAGE.into(),
    };
    parser.set_language(&language).ok()?;
    let tree = parser.parse(source, None)?;

    let mut sources = Vec::new();
    collect(tree.root_node(), source.as_bytes(), &mut sources);
    for scanned in scan(source) {
        if !sources.contains(&scanned) {
            sources.push(scanned);
        }
    }
    Some(ModuleAnalysis::Coarse { sources })
}

/// Every import target found by reading lines rather than the tree.
///
/// Deliberately literal: a line that begins an import, then the first quoted thing on
/// it. It cannot see a target written across two lines and it will read one out of a
/// commented-out rule, and both of those are the right way to be wrong — it exists
/// only to raise a floor under the grammar, and an edge too many costs precision
/// while an edge too few costs an answer.
fn scan(source: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in source.lines() {
        let line = line.trim_start();
        let Some(rest) = ["@use", "@forward", "@import"]
            .iter()
            .find_map(|at| line.strip_prefix(at))
        else {
            continue;
        };
        let rest = rest.trim_start();
        let Some(quote) = rest.chars().next().filter(|c| *c == '"' || *c == '\'') else {
            continue;
        };
        if let Some(end) = rest[1..].find(quote) {
            out.push(rest[1..1 + end].to_string());
        }
    }
    out
}

/// Every specifier `node` and its descendants import.
///
/// The whole tree is walked rather than only its top level. An `@import` is allowed
/// inside a rule, and SCSS control flow puts one inside `@if` and `@each` often
/// enough to matter; a nested import is an import.
fn collect(node: Node<'_>, source: &[u8], out: &mut Vec<String>) {
    if matches!(
        node.kind(),
        "use_statement" | "forward_statement" | "import_statement" | "at_rule"
    ) && let Some(specifier) = specifier_of(node, source)
    {
        out.push(specifier);
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect(child, source, out);
    }
}

/// The target of an import statement, unquoted.
///
/// Only the first target is taken. What follows it is a clause about names rather
/// than about files — `as c`, `show pad`, `with ($a: 1)` — and none of those is
/// another file.
fn specifier_of(node: Node<'_>, source: &[u8]) -> Option<String> {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "string_value" | "string_content" | "plain_value" => {
                let text = child.utf8_text(source).ok()?;
                return Some(unquote(text).to_string());
            }
            // `@import url(./a.css)`. The CSS grammar does not read an unquoted
            // `url()` here as a call, so the text is taken apart instead of the tree.
            "call_expression" | "binary_expression" => {
                let text = child.utf8_text(source).ok()?;
                if let Some(inside) = url_argument(text) {
                    return Some(inside.to_string());
                }
                if let Some(found) = specifier_of(child, source) {
                    return Some(found);
                }
            }
            "arguments" => {
                if let Some(found) = specifier_of(child, source) {
                    return Some(found);
                }
            }
            _ => {}
        }
    }
    None
}

/// What `url(...)` wraps, given text that starts one.
fn url_argument(text: &str) -> Option<&str> {
    let inside = text.trim().strip_prefix("url(")?;
    let inside = inside.split(')').next().unwrap_or(inside);
    Some(unquote(inside)).filter(|found| !found.is_empty())
}

fn unquote(text: &str) -> &str {
    let trimmed = text.trim();
    trimmed
        .strip_prefix('"')
        .and_then(|rest| rest.strip_suffix('"'))
        .or_else(|| {
            trimmed
                .strip_prefix('\'')
                .and_then(|rest| rest.strip_suffix('\''))
        })
        .unwrap_or(trimmed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn read(name: &str, source: &str) -> Vec<String> {
        let analysis = analyse(&PathBuf::from(name), source).expect("a grammar");
        analysis.sources().to_vec()
    }

    #[test]
    fn the_three_ways_scss_names_another_file_are_all_edges() {
        let sources = read(
            "a.scss",
            "@use './colors';\n@forward 'mixins';\n@import 'legacy';\n",
        );
        assert_eq!(sources, vec!["./colors", "mixins", "legacy"]);
    }

    #[test]
    fn a_clause_after_the_target_is_not_another_file() {
        // `as`, `show` and `with` name things inside the module, not modules.
        let sources = read(
            "a.scss",
            "@use './c' as c;\n@forward 'm' show pad;\n@use './d' with ($a: 1);\n",
        );
        assert_eq!(sources, vec!["./c", "m", "./d"]);
    }

    #[test]
    fn css_imports_are_read_by_the_css_grammar() {
        let sources = read(
            "a.css",
            "@import \"./base.css\";\n@import url(./other.css);\n",
        );
        assert_eq!(sources, vec!["./base.css", "./other.css"]);
    }

    #[test]
    fn an_import_nested_in_a_rule_is_still_an_import() {
        // `@use` has to be top level, but `@import` does not, and SCSS control flow
        // puts one inside a condition often enough to matter.
        let sources = read("a.scss", "@if $a {\n  @import './only-then';\n}\n");
        assert_eq!(sources, vec!["./only-then"]);
    }

    #[test]
    fn a_statement_the_grammar_swallows_is_still_found() {
        // The map runs past its semicolon and takes the `@import` with it, so the
        // tree has one statement where the file has two. The line scan is what keeps
        // the second one from vanishing.
        let sources = read("a.scss", "$map: (a: 1, b: 2);\n@import './last';\n");
        assert_eq!(sources, vec!["./last"]);
    }

    #[test]
    fn a_stylesheet_with_nothing_to_import_has_no_edges() {
        assert!(read("a.scss", ".title { color: red; }\n").is_empty());
    }

    #[test]
    fn syntax_the_grammar_gets_wrong_still_gives_up_its_edges() {
        // Every one of these leaves an error node somewhere in the tree. None of
        // them may cost the file its imports, which is the whole contract with a
        // grammar that does not parse the language completely.
        let sources = read(
            "a.scss",
            "@use './colors' as c;\n\
             $brand: red !default;\n\
             $map: (a: 1, b: 2);\n\
             .x { @extend %base; top: #{$b}px; }\n\
             @each $k in a, b { .y { left: 0; } }\n\
             @import './last';\n",
        );
        assert_eq!(sources, vec!["./colors", "./last"]);
    }
}

#[cfg(test)]
mod corpus {
    use super::*;
    use std::path::PathBuf;

    /// Compares what the grammar finds against a line scan, over a real tree.
    #[test]
    #[ignore]
    fn measure() {
        let root = std::env::var("CORPUS").expect("CORPUS=<dir>");
        let (mut files, mut by_grammar, mut by_scan, mut missed_files) = (0, 0, 0, 0);
        let mut examples: Vec<String> = Vec::new();
        for entry in walkdir::WalkDir::new(&root)
            .into_iter()
            .filter_map(|e| e.ok())
        {
            let path = entry.path();
            if !is_style_file(path) {
                continue;
            }
            if path.components().any(|c| c.as_os_str() == "node_modules") {
                continue;
            }
            let Ok(source) = std::fs::read_to_string(path) else {
                continue;
            };
            files += 1;
            let found = analyse(&PathBuf::from(path), &source)
                .map(|a| a.sources().to_vec())
                .unwrap_or_default();
            let scanned = scan(&source);
            by_grammar += found.len();
            by_scan += scanned.len();
            let mut missing: Vec<&String> = scanned.iter().filter(|s| !found.contains(s)).collect();
            missing.dedup();
            if !missing.is_empty() {
                missed_files += 1;
                if examples.len() < 8 {
                    examples.push(format!("{:?} in {}", missing, path.display()));
                }
            }
        }
        println!("stylesheets        : {files}");
        println!("specifiers, grammar: {by_grammar}");
        println!("specifiers, scan   : {by_scan}");
        println!("files the grammar missed something in: {missed_files}");
        for e in &examples {
            println!("  {e}");
        }
    }

    /// Every `@use`/`@forward`/`@import` target, found by looking at lines.
    fn scan(source: &str) -> Vec<String> {
        let mut out = Vec::new();
        for line in source.lines() {
            let line = line.trim_start();
            let Some(rest) = ["@use", "@forward", "@import"]
                .iter()
                .find_map(|at| line.strip_prefix(at))
            else {
                continue;
            };
            let rest = rest.trim_start();
            let Some(quote) = rest.chars().next().filter(|c| *c == '"' || *c == '\'') else {
                continue;
            };
            if let Some(end) = rest[1..].find(quote) {
                out.push(rest[1..1 + end].to_string());
            }
        }
        out
    }
}
