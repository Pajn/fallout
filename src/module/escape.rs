//! What happens to a value where it is used: whether it stays in place, or leaves
//! the expression it is read in for code that could change it.
//!
//! The shared-state rule asks this of every use of a mutable module-scope binding,
//! and of what a Zustand store or a collection the file makes hands out. The walk
//! out through the wrappers that leave a value as it was is the same for all of
//! them, so this module holds it.

use oxc_ast::AstKind;
use oxc_ast::ast::JSXElementName;
use oxc_semantic::{AstNodes, NodeId};
use oxc_span::{GetSpan, Span as OxcSpan};

/// Whether a value in the JSX expression container at `container` is only
/// rendered where it stands, rather than handed to code that could change it.
///
/// A component is handed its children as a prop, free to call them or change what
/// they hold, as it is handed every other prop. An element of the platform's own,
/// such as `<li>`, or a fragment, only renders its children. React takes `key` for
/// itself, as a string, and hands it to no component. Any other prop is handed on,
/// even by an element of the platform's own, which may call it as a handler.
pub(crate) fn rendered_in_place(nodes: &AstNodes<'_>, container: NodeId) -> bool {
    match nodes.parent_kind(container) {
        AstKind::JSXElement(element) => {
            matches!(element.opening_element.name, JSXElementName::Identifier(_))
        }
        AstKind::JSXFragment(_) => true,
        AstKind::JSXAttribute(attribute) => attribute.is_key(),
        _ => false,
    }
}

/// Walks out through parentheses and type-only wrappers, which leave the value as
/// it was, returning the outermost node standing for it and its span.
pub(crate) fn through_wrappers(nodes: &AstNodes<'_>, node_id: NodeId) -> (NodeId, OxcSpan) {
    let mut current = node_id;
    let mut span = nodes.get_node(node_id).kind().span();
    loop {
        match nodes.parent_kind(current) {
            AstKind::ParenthesizedExpression(_)
            | AstKind::TSNonNullExpression(_)
            | AstKind::TSAsExpression(_)
            | AstKind::TSSatisfiesExpression(_)
            | AstKind::TSTypeAssertion(_) => {
                current = nodes.parent_id(current);
                span = nodes.get_node(current).kind().span();
            }
            _ => return (current, span),
        }
    }
}

#[cfg(test)]
mod tests {
    use oxc_allocator::Allocator;
    use oxc_parser::Parser;
    use oxc_semantic::SemanticBuilder;
    use oxc_span::SourceType;

    use crate::module::parse::Ctx;
    use crate::module::shared::{self, Collection, Mode};

    /// What happens to a value where it is used.
    #[derive(Copy, Clone, Debug, PartialEq, Eq)]
    enum Fate {
        /// It stays in the expression it is read in.
        InPlace,
        /// It is handed to code that could change it.
        Escapes,
        /// It is changed where it stands.
        Writes,
    }

    /// One cell of the table below.
    #[derive(Copy, Clone, Debug)]
    enum Cell {
        Is(Fate),
        /// Read as in place, although the value leaves the expression.
        KnownUnderReport,
        /// The template is not valid code for this column.
        Invalid,
    }

    const I: Cell = Cell::Is(Fate::InPlace);
    const E: Cell = Cell::Is(Fate::Escapes);
    const W: Cell = Cell::Is(Fate::Writes);
    const NA: Cell = Cell::Invalid;

    /// Marks a known under-report: a use read as in place although the value
    /// leaves the expression. It is asserted as in place as it stands. Escapes after
    /// the soundness PR.
    macro_rules! known_under_report {
        () => {
            Cell::KnownUnderReport
        };
    }

    /// The columns: what `@` stands for in each, and how the reference to `S` in it
    /// is read.
    ///
    /// - `WB`, the whole value used bare, and `WP`, a part read off it: a binding
    ///   read as one value, such as a reassigned `let` or a call's result.
    /// - `PR`: a property of an object shared property by property.
    /// - `ME`: a member of an object read only for its members, where `p` is known
    ///   not to reach the object through `this`.
    /// - `CE`: an element of an array the file makes.
    /// - `CS`: what a `Map` the file makes hands out from `get`.
    /// - `ZS`: a Zustand store's state from `getState()`, and `ZP`, a property of it.
    const COLUMNS: [(&str, &str); 8] = [
        ("WB", "S"),
        ("WP", "S.p"),
        ("PR", "S.p"),
        ("ME", "S.p"),
        ("CE", "S[0]"),
        ("CS", "S.get(k)"),
        ("ZS", "S.getState()"),
        ("ZP", "S.getState().p"),
    ];

    /// Whether the one reference to `S` in `template`, with `@` standing for what
    /// `column` says, stays in place, or `None` where that is not valid code.
    ///
    /// A template is a statement in the body of an async generator, so that
    /// `await` and `yield` can be written, or a top-level `export`.
    fn in_place(template: &str, column: usize) -> Option<bool> {
        let body = template.replace('@', COLUMNS[column].1);
        let source = if body.starts_with("export") {
            format!("let S: any;\n{body}\n")
        } else {
            format!("let S: any;\nasync function* a() {{\n{body}\n}}\n")
        };
        let allocator = Allocator::default();
        let parsed = Parser::new(&allocator, &source, SourceType::tsx()).parse();
        if !parsed.diagnostics.is_empty() {
            return None;
        }
        let semantic = SemanticBuilder::new()
            .with_build_nodes(true)
            .build(&parsed.program)
            .semantic;
        let ctx = Ctx {
            semantic: &semantic,
            statements: Vec::new(),
        };
        let scoping = semantic.scoping();
        let symbol = scoping
            .symbol_ids()
            .find(|&symbol| scoping.symbol_name(symbol) == "S")
            .expect("S");
        let &[reference] = scoping.get_resolved_reference_ids(symbol) else {
            panic!("expected one reference to S: {source}");
        };
        let node = scoping.get_reference(reference).node_id();
        let callable = ["p".to_string()];
        let mode = match COLUMNS[column].0 {
            "PR" => Mode::Properties,
            "ME" => Mode::Members(&callable),
            "CE" => Mode::Collection(Collection::Array),
            "CS" => Mode::Collection(Collection::Map),
            _ => Mode::Whole,
        };
        let accesses = shared::accesses(&ctx, node, mode);
        // Whether a store's method only reads it is the graph's to say, where it
        // knows the binding holds a store, so a read call is in place here.
        let zustand = COLUMNS[column].0.starts_with('Z');
        Some(
            accesses
                .iter()
                .all(|(_, access)| !access.write || (zustand && access.read_call.is_some())),
        )
    }

    /// One row per template, with `@` where the value is used, and one cell per
    /// column. `U` is short for [`known_under_report!`].
    macro_rules! characterise {
        (@cell U) => { known_under_report!() };
        (@cell $cell:ident) => { $cell };
        ($($template:literal => [$($cell:ident),*];)*) => {
            &[$(($template, [$(characterise!(@cell $cell)),*])),*]
        };
    }

    /// How each use is read, as it stands.
    ///
    /// The shared-state rule acts only on whether a use stays in place, so that is
    /// what is asserted. Whether one that does not escapes or writes is recorded
    /// for the reader: a write changes the value where it stands, through an
    /// assignment, an update, a `delete`, a destructuring target or a method called
    /// on what it was read off; an escape hands it to code that could.
    #[rustfmt::skip]
    const TABLE: &[(&str, [Cell; 8])] = characterise! {
        //                                              WB  WP  PR  ME  CE  CS  ZS  ZP
        // Checked, tested, coerced or dropped where it stands.
        "@;"                                         => [E,  I,  I,  I,  I,  I,  I,  I ];
        "typeof @;"                                  => [I,  I,  I,  I,  I,  I,  I,  I ];
        "void @;"                                    => [E,  I,  I,  I,  I,  I,  I,  I ];
        "!@;"                                        => [E,  I,  I,  I,  I,  I,  I,  I ];
        "-@;"                                        => [E,  I,  I,  I,  I,  I,  I,  I ];
        "@ === o;"                                   => [I,  I,  I,  I,  I,  I,  I,  I ];
        "k in @;"                                    => [I,  I,  I,  I,  I,  I,  I,  I ];
        "@ instanceof C;"                            => [I,  I,  I,  I,  I,  I,  I,  I ];
        "@ < 1;"                                     => [I,  I,  I,  I,  I,  I,  I,  I ];
        "@ + 1;"                                     => [E,  I,  I,  I,  I,  I,  I,  I ];
        "@ | 1;"                                     => [E,  I,  I,  I,  I,  I,  I,  I ];
        "`${@}`;"                                    => [E,  I,  I,  I,  I,  I,  I,  I ];
        "if (@) go();"                               => [E,  I,  I,  I,  I,  I,  I,  I ];
        "@ ? 1 : 2;"                                 => [E,  I,  I,  I,  I,  I,  I,  I ];
        "while (@) go();"                            => [E,  I,  I,  I,  I,  E,  E,  E ];
        "switch (@) {}"                              => [E,  I,  I,  I,  I,  E,  E,  E ];
        "class K { #x; m() { if (#x in @) go(); } }" => [I,  I,  I,  I,  I,  E,  E,  E ];
        // Read from where it stands.
        "@.x;"                                       => [I,  I,  I,  I,  I,  I,  I,  I ];
        "@.x.y;"                                     => [I,  I,  I,  I,  I,  I,  I,  I ];
        "@.x + 1;"                                   => [I,  I,  I,  I,  I,  I,  I,  I ];
        "@.length;"                                  => [I,  I,  I,  I,  I,  I,  I,  I ];
        "@?.x;"                                      => [I,  I,  I,  I,  I,  I,  I,  I ];
        "@[k];"                                      => [I,  I,  I,  I,  I,  I,  I,  I ];
        "(@).x;"                                     => [E,  I,  I,  I,  I,  I,  I,  I ];
        "(@ as any).x;"                              => [E,  I,  I,  I,  I,  I,  I,  I ];
        "@!.x;"                                      => [E,  I,  I,  I,  I,  I,  I,  I ];
        "o[@];"                                      => [I,  I,  I,  I,  I,  E,  E,  E ];
        // Called, constructed, or handed to a call.
        "@();"                                       => [I,  W,  W,  I,  W,  E,  E,  W ];
        "new @();"                                   => [I,  E,  E,  E,  E,  E,  E,  E ];
        "@.x();"                                     => [W,  W,  W,  W,  W,  W,  W,  W ];
        "@.x.y();"                                   => [W,  W,  W,  W,  W,  W,  W,  W ];
        "@?.x();"                                    => [W,  W,  W,  W,  W,  W,  W,  W ];
        "f(@);"                                      => [E,  E,  E,  E,  E,  E,  E,  E ];
        "f(@.x);"                                    => [E,  E,  E,  E,  E,  E,  E,  E ];
        "new C(@);"                                  => [E,  E,  E,  E,  E,  E,  E,  E ];
        "tag`${@}`;"                                 => [E,  U,  U,  U,  U,  E,  E,  E ];
        "@`x`;"                                      => [E,  W,  W,  I,  W,  E,  E,  W ];
        "@.x`y`;"                                    => [W,  W,  W,  W,  W,  W,  W,  W ];
        // Written.
        "@.x = 1;"                                   => [W,  W,  W,  W,  W,  W,  W,  W ];
        "@.x.y = 1;"                                 => [W,  W,  W,  W,  W,  W,  W,  W ];
        "@.x++;"                                     => [W,  W,  W,  W,  W,  W,  W,  W ];
        "delete @.x;"                                => [W,  W,  W,  W,  W,  W,  W,  W ];
        "delete @;"                                  => [W,  W,  W,  W,  W,  W,  W,  W ];
        "[@.x] = o;"                                 => [W,  W,  W,  W,  W,  W,  W,  W ];
        "({ a: @.x } = o);"                          => [W,  W,  W,  W,  W,  W,  W,  W ];
        "for (@.x of o);"                            => [W,  W,  W,  W,  W,  W,  W,  W ];
        // Iterated, stored or spread.
        "for (const x of @);"                        => [E,  E,  E,  E,  E,  E,  E,  E ];
        "for (const x in @);"                        => [E,  E,  E,  E,  E,  E,  E,  E ];
        "o.a = @;"                                   => [E,  E,  E,  E,  E,  E,  E,  E ];
        "o.a = @.x;"                                 => [E,  E,  E,  E,  E,  E,  E,  E ];
        "[...@];"                                    => [E,  E,  E,  E,  E,  E,  E,  E ];
        "({ ...@ });"                                => [E,  E,  E,  E,  E,  E,  E,  E ];
        // Returned, thrown, awaited or yielded.
        "return @;"                                  => [E,  U,  U,  U,  U,  E,  E,  E ];
        "return @.x;"                                => [U,  U,  U,  U,  U,  E,  E,  E ];
        "const g = () => @;"                         => [E,  U,  U,  U,  U,  E,  E,  E ];
        "const g = () => @.x;"                       => [U,  U,  U,  U,  U,  E,  E,  E ];
        "throw @;"                                   => [E,  U,  U,  U,  U,  E,  E,  E ];
        "await @;"                                   => [E,  U,  U,  U,  U,  E,  E,  E ];
        "yield @;"                                   => [E,  U,  U,  U,  U,  E,  E,  E ];
        // Passed through an operator that yields one of its operands.
        "f(o && @);"                                 => [E,  U,  U,  U,  U,  E,  E,  E ];
        "f(o || @);"                                 => [E,  U,  U,  U,  U,  E,  E,  E ];
        "f(o ?? @);"                                 => [E,  U,  U,  U,  U,  E,  E,  E ];
        "f(c ? @ : d);"                              => [E,  U,  U,  U,  U,  E,  E,  E ];
        "f((0, @));"                                 => [E,  U,  U,  U,  U,  E,  E,  E ];
        "return c ? @ : d;"                          => [E,  U,  U,  U,  U,  E,  E,  E ];
        "return o && @;"                             => [E,  U,  U,  U,  U,  E,  E,  E ];
        // Held in a literal.
        "f({ a: @ });"                               => [E,  U,  U,  U,  U,  E,  E,  E ];
        "f([@]);"                                    => [E,  U,  U,  U,  U,  E,  E,  E ];
        "return { a: @ };"                           => [E,  U,  U,  U,  U,  E,  E,  E ];
        "return [@];"                                => [E,  U,  U,  U,  U,  E,  E,  E ];
        // Held in a binding, or a class field or default of one.
        "const v = @; f(v);"                         => [E,  U,  E,  E,  E,  E,  E,  E ];
        "const v = @; return v;"                     => [E,  U,  E,  E,  E,  E,  E,  E ];
        "const v = @; v.x;"                          => [E,  I,  I,  I,  I,  I,  I,  I ];
        "const v = @; v.x = 1;"                      => [E,  U,  E,  E,  E,  E,  E,  E ];
        "const v = @; v.x();"                        => [E,  U,  E,  E,  E,  E,  E,  E ];
        "const { x } = @; f(x);"                     => [E,  U,  E,  E,  E,  E,  E,  E ];
        "const { x } = @; x + 1;"                    => [E,  I,  E,  E,  E,  I,  I,  I ];
        "const { x } = @; x.y = 1;"                  => [E,  U,  E,  E,  E,  E,  E,  E ];
        "let v = @; v.x;"                            => [E,  I,  E,  E,  E,  E,  E,  E ];
        "export const v = @;"                        => [E,  U,  E,  E,  E,  E,  E,  E ];
        "class K { x = @; }"                         => [E,  U,  U,  U,  U,  E,  E,  E ];
        "function g(x = @) {}"                       => [E,  U,  U,  U,  U,  E,  E,  E ];
        "export default @;"                          => [E,  U,  U,  U,  U,  E,  E,  E ];
        // Rendered, or handed to a component.
        "return <li>{@}</li>;"                       => [I,  I,  I,  I,  I,  I,  E,  I ];
        "return <>{@}</>;"                           => [I,  I,  I,  I,  I,  I,  E,  I ];
        "return <li key={@} />;"                     => [I,  I,  I,  I,  I,  I,  E,  I ];
        "return <li>{@.x}</li>;"                     => [I,  I,  I,  I,  I,  I,  I,  I ];
        "return <Foo>{@}</Foo>;"                     => [E,  E,  E,  E,  E,  E,  E,  E ];
        "return <Foo>{@.x}</Foo>;"                   => [E,  E,  E,  E,  E,  E,  E,  E ];
        "return <Foo value={@} />;"                  => [E,  E,  E,  E,  E,  E,  E,  E ];
        "return <div onClick={@} />;"                => [E,  E,  E,  E,  E,  E,  E,  E ];
        "return <Foo {...@} />;"                     => [E,  U,  U,  U,  U,  E,  E,  E ];
        "return <@ />;"                              => [I,  E,  E,  E,  NA, NA, NA, NA];
    };

    #[test]
    fn every_use_is_read_as_the_table_says() {
        let mut wrong = Vec::new();
        for (template, cells) in TABLE {
            for (column, cell) in cells.iter().enumerate() {
                let found = in_place(template, column);
                let expected = match cell {
                    Cell::Is(fate) => Some(*fate == Fate::InPlace),
                    Cell::KnownUnderReport => Some(true),
                    Cell::Invalid => None,
                };
                if found != expected {
                    wrong.push(format!(
                        "{template} in {}: expected {cell:?}, found {found:?}",
                        COLUMNS[column].0
                    ));
                }
            }
        }
        assert!(wrong.is_empty(), "{}", wrong.join("\n"));
    }
}
