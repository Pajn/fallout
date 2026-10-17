//! The comments a build reads.
//!
//! Comments are left out of every comparison, because nearly all of them are for
//! people. A few are for the compiler or the bundler, and change what it produces
//! though the syntax around them stays the same:
//!
//! - a JSX pragma, `@jsx`, `@jsxFrag`, `@jsxImportSource` or `@jsxRuntime`, which
//!   decides what every element in the file compiles to, and so belongs to the file;
//! - an annotation, which belongs to the code it sits in or just before: `#__PURE__`,
//!   `#__NO_SIDE_EFFECTS__` and `#__KEY__` (each also spelled with `@`),
//!   `@vite-ignore`, and webpack's and Turbopack's magic comments, such as
//!   `webpackChunkName`.
//!
//! Recognising one is a matter of its text alone. A comment that only mentions a tag
//! is taken to be one, which can over-report and never under-report.

use oxc_ast::ast::Program;
use oxc_span::GetSpan;

/// The comments of one version of a file that a build reads, by where they apply.
#[derive(Debug, PartialEq, Eq)]
pub struct BuildComments {
    /// The file's JSX pragmas, in the order written.
    pub pragmas: Vec<String>,
    /// The annotations of each top-level statement, by its index in the program's
    /// body, each in the order written.
    pub annotations: Vec<Vec<String>>,
}

impl BuildComments {
    /// Reads them out of `program`, whose text is `source`.
    ///
    /// An annotation belongs to the first statement that ends after it starts: the
    /// one it sits in, or else the one it comes before. One after the last statement
    /// applies to nothing, and is dropped.
    pub fn read(program: &Program<'_>, source: &str) -> Self {
        let mut pragmas = Vec::new();
        let mut annotations = vec![Vec::new(); program.body.len()];
        for comment in &program.comments {
            let Some(text) = source.get(comment.span.start as usize..comment.span.end as usize)
            else {
                continue;
            };
            // Git may hand the base version back with other line breaks than the
            // working tree has, and a block comment's are its own.
            let text = text.replace("\r\n", "\n");
            if is_pragma(&text) {
                pragmas.push(text);
            } else if is_annotation(&text) {
                let statement = program
                    .body
                    .iter()
                    .position(|statement| statement.span().end > comment.span.start);
                if let Some(index) = statement {
                    annotations[index].push(text);
                }
            }
        }
        Self {
            pragmas,
            annotations,
        }
    }
}

/// The pragmas the JSX transforms of Babel, TypeScript, esbuild and SWC read.
const PRAGMAS: &[&str] = &["Frag", "ImportSource", "Runtime", ""];

/// Whether `text` names a JSX pragma: `@jsx`, alone or with one of the suffixes
/// above, followed by whitespace or by the end of the text.
///
/// A diff line is text of this kind as well, which is what lets a run with no base
/// version ask it of the lines a change adds or removes.
pub fn is_pragma(text: &str) -> bool {
    text.match_indices("@jsx").any(|(at, tag)| {
        let rest = &text[at + tag.len()..];
        PRAGMAS.iter().any(|suffix| {
            rest.strip_prefix(suffix)
                .is_some_and(|after| after.chars().next().is_none_or(char::is_whitespace))
        })
    })
}

/// The annotations spelled the same however they are used.
const ANNOTATIONS: &[&str] = &["__PURE__", "__NO_SIDE_EFFECTS__", "__KEY__"];

/// Whether `text` holds an annotation a bundler or minifier reads.
fn is_annotation(text: &str) -> bool {
    ANNOTATIONS.iter().any(|annotation| {
        text.contains(&format!("#{annotation}")) || text.contains(&format!("@{annotation}"))
    }) || text.contains("@vite-ignore")
        || has_magic_key(text, "webpack")
        || has_magic_key(text, "turbopack")
}

/// Whether `text` holds a magic comment's key: `prefix` followed by a capital, as
/// in `webpackChunkName` or `turbopackOptional`. The keys are read anywhere in the
/// comment, which webpack parses as the body of an object literal.
fn has_magic_key(text: &str, prefix: &str) -> bool {
    text.match_indices(prefix).any(|(at, key)| {
        text[at + key.len()..]
            .chars()
            .next()
            .is_some_and(|next| next.is_ascii_uppercase())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxc_allocator::Allocator;
    use oxc_parser::Parser;
    use oxc_span::SourceType;

    fn read(source: &str) -> BuildComments {
        let allocator = Allocator::default();
        let parsed = Parser::new(&allocator, source, SourceType::tsx()).parse();
        assert!(parsed.diagnostics.is_empty(), "{:?}", parsed.diagnostics);
        BuildComments::read(&parsed.program, source)
    }

    #[test]
    fn every_jsx_pragma_is_one_and_a_longer_tag_is_not() {
        for text in [
            "/** @jsx h */",
            "/* @jsxFrag Fragment */",
            "/** @jsxImportSource @emotion/react */",
            "// @jsxRuntime classic",
            "/**\n * @jsxImportSource preact\n */",
            "/* @jsx */",
        ] {
            assert!(is_pragma(text), "{text}");
        }
        for text in ["/* @jsxPragmaFrag x */", "// a JSX file", "/* @jsxs */"] {
            assert!(!is_pragma(text), "{text}");
        }
    }

    #[test]
    fn a_pragma_belongs_to_the_file_wherever_it_is_written() {
        let comments = read("const a = 1;\n/** @jsxImportSource preact */\nconst b = 2;\n");
        assert_eq!(comments.pragmas, ["/** @jsxImportSource preact */"]);
        assert_eq!(comments.annotations, [Vec::<String>::new(), Vec::new()]);
    }

    #[test]
    fn an_annotation_belongs_to_the_statement_it_sits_in_or_comes_before() {
        let comments = read(
            "export const load = () => import(/* webpackChunkName: \"a\" */ \"./a\");\n\
             /* #__NO_SIDE_EFFECTS__ */\n\
             export function make() {}\n\
             // a note for people\n\
             export const v = /* @__PURE__ */ make();\n\
             /* @vite-ignore */\n",
        );
        assert_eq!(
            comments.annotations,
            [
                vec!["/* webpackChunkName: \"a\" */".to_string()],
                vec!["/* #__NO_SIDE_EFFECTS__ */".to_string()],
                vec!["/* @__PURE__ */".to_string()],
            ]
        );
        assert!(comments.pragmas.is_empty());
    }

    #[test]
    fn a_magic_key_is_read_anywhere_in_the_comment() {
        assert!(is_annotation(
            "/* webpackMode: \"lazy\", webpackPrefetch: true */"
        ));
        assert!(is_annotation("/* turbopackOptional: true */"));
        assert!(!is_annotation("/* built with webpack */"));
        assert!(!is_annotation("// the webpack config lives elsewhere"));
    }
}
