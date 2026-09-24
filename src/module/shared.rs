//! How a declaration touches a module-scope binding, for the shared-state rule.
//!
//! The rule connects a declaration that uses a binding to every declaration that
//! could change what it holds. For most bindings a use is a use of the whole value:
//! a `let` number, a `Map`, a context, a call's result. A binding initialised with
//! a plain object literal is different, because its properties are separate slots:
//! a write to `state.theme` cannot change what `state.volume` reads back. For such
//! a binding each use is attributed to the property it goes through, and only uses
//! of the same property — or of the whole object — are connected.
//!
//! Whatever cannot be attributed to one property is a use of the whole object: a
//! method call, which hands the method the object as `this`; passing the object on;
//! a computed key; `__proto__`. A `const` alias of the object or of one property is
//! followed to its own uses, which are credited to the declarations they are written
//! in, since that is where the write happens. An alias that is exported, or is not a
//! `const`, is taken as writing the whole of what it aliases.

use oxc_ast::AstKind;
use oxc_ast::ast::*;
use oxc_semantic::{AstNodes, NodeId, SymbolId};
use oxc_span::{GetSpan, Span as OxcSpan};

use super::members::object_literal;
use super::parse::Ctx;
use super::refs::{Use, classify};

/// One way a reference touches a shared binding.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Access {
    /// The property it goes through, or `None` for the whole value.
    pub property: Option<String>,
    /// Whether it could change what the binding, or that property, holds.
    pub write: bool,
}

impl Access {
    fn whole(write: bool) -> Self {
        Self {
            property: None,
            write,
        }
    }

    /// Whether a change made through `write` can be seen through `self`.
    pub fn sees(&self, write: &Access) -> bool {
        write.write
            && match (&self.property, &write.property) {
                (Some(read), Some(written)) => read == written,
                _ => true,
            }
    }
}

/// How the uses of one binding are read.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) enum Mode<'m> {
    /// Every use is a use of the whole value, as the rule has always read them.
    Whole,
    /// An object whose properties are independent, used through them.
    Properties,
    /// An object read only for its members. Calling one of the members listed,
    /// which are known not to reach the object through `this`, reads that member
    /// and leaves the object be; calling any other may reach all of it.
    Members(&'m [String]),
}

/// How far aliases of aliases are followed before the whole object is assumed.
const ALIAS_DEPTH: usize = 4;

/// Whether `symbol` holds an object whose properties are independent of each other:
/// a plain object literal with no accessor and no prototype of its own, bound by a
/// declaration nothing reassigns.
pub(crate) fn independent_properties(ctx: &Ctx<'_>, symbol: SymbolId) -> bool {
    let scoping = ctx.semantic.scoping();
    if scoping
        .get_resolved_reference_ids(symbol)
        .iter()
        .any(|id| scoping.get_reference(*id).is_write())
    {
        return false;
    }
    let AstKind::VariableDeclarator(declarator) = ctx
        .semantic
        .nodes()
        .kind(scoping.symbol_declaration(symbol))
    else {
        return false;
    };
    if !matches!(&declarator.id, BindingPattern::BindingIdentifier(_)) {
        return false;
    }
    let Some(object) = declarator.init.as_ref().and_then(object_literal) else {
        return false;
    };
    // A getter or setter runs code on a read or a write that can reach any other
    // property, and `__proto__` lends the object properties it never lists. A
    // spread copies values, so what it brings in are ordinary slots.
    object.properties.iter().all(|property| match property {
        ObjectPropertyKind::ObjectProperty(property) => {
            property.kind == PropertyKind::Init
                && (property.computed
                    || property.shorthand
                    || property
                        .key
                        .static_name()
                        .is_none_or(|name| name != "__proto__"))
        }
        ObjectPropertyKind::SpreadProperty(_) => true,
    })
}

/// Every access one reference makes, each with the offset it is written at.
///
/// The offset is the reference's own unless an alias carried the access elsewhere,
/// which is what lets a write through an alias be credited to the declaration that
/// makes it.
pub(crate) fn accesses(ctx: &Ctx<'_>, node_id: NodeId, mode: Mode) -> Vec<(u32, Access)> {
    let at = ctx.semantic.nodes().get_node(node_id).kind().span().start;
    if mode == Mode::Whole {
        let write = classify(ctx.semantic.nodes(), node_id) == Use::Mutate;
        return vec![(at, Access::whole(write))];
    }
    reference_accesses(ctx, node_id, mode, 0)
}

fn reference_accesses(
    ctx: &Ctx<'_>,
    node_id: NodeId,
    mode: Mode,
    depth: usize,
) -> Vec<(u32, Access)> {
    let nodes = ctx.semantic.nodes();
    let at = nodes.get_node(node_id).kind().span().start;
    let (top, span) = through_wrappers(nodes, node_id);
    let whole = |write| vec![(at, Access::whole(write))];

    let property = match nodes.parent_kind(top) {
        AstKind::StaticMemberExpression(member) if member.object.span() == span => {
            Some(member.property.name.to_string())
        }
        AstKind::ComputedMemberExpression(member) if member.object.span() == span => {
            match &member.expression {
                Expression::StringLiteral(key) => Some(key.value.to_string()),
                // Which property an unknown key names is unknown.
                _ => None,
            }
        }
        AstKind::VariableDeclarator(declarator)
            if declarator
                .init
                .as_ref()
                .is_some_and(|init| init.span() == span) =>
        {
            // `const view = state`: what `view` does, `state` does.
            return match followed_alias(ctx, top, mode, depth) {
                Some(uses) => uses,
                None => whole(true),
            };
        }
        // Reading the value in place, as the whole-value rule does.
        AstKind::UnaryExpression(unary) if unary.operator == UnaryOperator::Typeof => {
            return whole(false);
        }
        // `export default utils` of an object read only for its members exports it
        // as an `export { }` list does, which is no use of it in this file. Any
        // other object is handed to importers free to change it.
        AstKind::ExportDefaultDeclaration(_) if matches!(mode, Mode::Members(_)) => {
            return Vec::new();
        }
        _ => return whole(classify(nodes, node_id) == Use::Mutate),
    };

    // `__proto__` is the prototype, through which every property can change.
    if property.as_deref() == Some("__proto__") {
        return whole(true);
    }
    match property_use(ctx, nodes.parent_id(top), depth) {
        PropertyUse::Receiver
            if matches!(mode, Mode::Members(callable)
                if property.as_ref().is_some_and(|name| callable.contains(name))) =>
        {
            vec![(
                at,
                Access {
                    property,
                    write: false,
                },
            )]
        }
        PropertyUse::Receiver => whole(true),
        PropertyUse::Uses(uses) => uses
            .into_iter()
            .map(|(offset, write)| {
                let offset = offset.unwrap_or(at);
                (
                    offset,
                    Access {
                        property: property.clone(),
                        write,
                    },
                )
            })
            .collect(),
    }
}

/// What is done with one property once it has been read off the object.
enum PropertyUse {
    /// It is called as a method, which hands it the whole object as `this`.
    Receiver,
    /// Each use of the property's value, with where it is written if an alias
    /// carried it elsewhere, and whether it could change the value.
    Uses(Vec<(Option<u32>, bool)>),
}

/// Follows a property read to what is done with it: `state.list.push(x)` writes
/// `list` however deep the chain, and `state.theme()` is a method call.
fn property_use(ctx: &Ctx<'_>, member: NodeId, depth: usize) -> PropertyUse {
    let nodes = ctx.semantic.nodes();
    let mut current = member;
    let mut deeper = false;
    loop {
        let (outer, span) = through_wrappers(nodes, current);
        match nodes.parent_kind(outer) {
            AstKind::StaticMemberExpression(next) if next.object.span() == span => {
                current = nodes.parent_id(outer);
                deeper = true;
            }
            AstKind::ComputedMemberExpression(next) if next.object.span() == span => {
                current = nodes.parent_id(outer);
                deeper = true;
            }
            AstKind::ChainExpression(_) => current = nodes.parent_id(outer),
            _ => break,
        }
    }

    let (top, span) = through_wrappers(nodes, current);
    let written = |write| PropertyUse::Uses(vec![(None, write)]);
    match nodes.parent_kind(top) {
        AstKind::CallExpression(call) if call.callee.span() == span => {
            if deeper {
                written(true)
            } else {
                PropertyUse::Receiver
            }
        }
        AstKind::TaggedTemplateExpression(tagged) if tagged.tag.span() == span => {
            if deeper {
                written(true)
            } else {
                PropertyUse::Receiver
            }
        }
        AstKind::VariableDeclarator(declarator)
            if declarator
                .init
                .as_ref()
                .is_some_and(|init| init.span() == span) =>
        {
            // `const settings = state.settings`: a write through `settings` is a
            // write to the property, wherever it is made.
            // The alias holds the property's value, not the object, so whatever it
            // calls is called on that value.
            match followed_alias(ctx, top, Mode::Properties, depth) {
                Some(uses) => PropertyUse::Uses(
                    uses.into_iter()
                        .map(|(offset, access)| (Some(offset), access.write))
                        .collect(),
                ),
                None => written(true),
            }
        }
        // Handed to other code, stored, written, or deleted: the value can change.
        AstKind::CallExpression(_)
        | AstKind::NewExpression(_)
        | AstKind::SpreadElement(_)
        | AstKind::AssignmentExpression(_)
        | AstKind::UpdateExpression(_)
        | AstKind::AssignmentTargetPropertyIdentifier(_)
        | AstKind::AssignmentTargetPropertyProperty(_)
        | AstKind::ArrayAssignmentTarget(_)
        | AstKind::AssignmentTargetRest(_)
        | AstKind::AssignmentTargetWithDefault(_)
        | AstKind::ForInStatement(_)
        | AstKind::ForOfStatement(_)
        | AstKind::JSXOpeningElement(_)
        | AstKind::JSXClosingElement(_) => written(true),
        AstKind::UnaryExpression(unary) if unary.operator == UnaryOperator::Delete => written(true),
        // Read where it stands, as the whole-value rule reads `s.x`.
        _ => written(false),
    }
}

/// The accesses made through a `const` alias declared by the declarator above
/// `value`, or `None` where the alias cannot be followed: it is exported, it can
/// be reassigned, it destructures, or aliases have nested too deep.
fn followed_alias(
    ctx: &Ctx<'_>,
    value: NodeId,
    mode: Mode,
    depth: usize,
) -> Option<Vec<(u32, Access)>> {
    if depth >= ALIAS_DEPTH {
        return None;
    }
    let nodes = ctx.semantic.nodes();
    let declarator_id = nodes.parent_id(value);
    let AstKind::VariableDeclarator(declarator) = nodes.kind(declarator_id) else {
        return None;
    };
    let BindingPattern::BindingIdentifier(alias) = &declarator.id else {
        return None;
    };
    let declaration = nodes.parent_id(declarator_id);
    let AstKind::VariableDeclaration(variable) = nodes.kind(declaration) else {
        return None;
    };
    if variable.kind != VariableDeclarationKind::Const
        || matches!(
            nodes.parent_kind(declaration),
            AstKind::ExportDeclaration(_)
        )
    {
        return None;
    }
    let symbol = alias.symbol_id.get()?;
    let scoping = ctx.semantic.scoping();
    let mut uses = Vec::new();
    for reference_id in scoping.get_resolved_reference_ids(symbol) {
        let node_id = scoping.get_reference(*reference_id).node_id();
        uses.extend(reference_accesses(ctx, node_id, mode, depth + 1));
    }
    Some(uses)
}

/// Walks out through parentheses and type-only wrappers, which leave the value as
/// it was, returning the outermost node standing for it and its span.
fn through_wrappers(nodes: &AstNodes<'_>, node_id: NodeId) -> (NodeId, OxcSpan) {
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
    use crate::module::{ModuleAnalysis, Reading, parse::analyse_source};
    use std::path::Path;

    /// Whether `reader` reaches `writer` in `source`.
    fn reaches(source: &str, reader: &str, writer: &str) -> bool {
        let (ModuleAnalysis::Fine(module), _) =
            analyse_source(Path::new("state.ts"), source, &Reading::default()).unwrap()
        else {
            panic!("expected fine module: {source}")
        };
        let reader = module.decl_named(reader).expect(reader);
        let writer = module.decl_named(writer).expect(writer);
        module.decls[reader as usize].refs.contains(&writer)
    }

    const STATE: &str = "let state = { theme: 'light', volume: 1, list: [] as number[] };\n";

    #[test]
    fn a_write_to_one_property_does_not_reach_a_read_of_another() {
        for writer in [
            "export const write = () => { state.theme = 'dark'; };",
            "export const write = () => { state['theme'] = 'dark'; };",
            "export const write = () => { (state as any).theme = 'dark'; };",
            "export const write = () => { state.theme.length = 0; };",
            "export const write = () => { state.list.push(1); };",
            "export const write = () => { delete state.theme; };",
            "export const write = () => { const view = state; view.theme = 'dark'; };",
            "export const write = () => { const list = state.list; list.push(1); };",
            "export const write = () => { const a = state; const b = a; b.theme = 'dark'; };",
        ] {
            let source = format!(
                "{STATE}{writer}\nexport const read = () => state.volume;\nexport const alsoRead = () => state?.volume;"
            );
            assert!(!reaches(&source, "read", "write"), "{source}");
            assert!(!reaches(&source, "alsoRead", "write"), "{source}");
        }
    }

    #[test]
    fn a_write_reaches_every_read_of_the_same_property() {
        for (writer, reader) in [
            ("state.volume = 2;", "state.volume"),
            ("state['volume'] = 2;", "state.volume"),
            ("state.volume++;", "state.volume"),
            ("state.list.push(1);", "state.list.length"),
            ("state.list.length = 0;", "state.list"),
            ("save(state.list);", "state.list"),
            ("const list = state.list; list.push(1);", "state.list"),
            ("const list = state.list; save(list);", "state.list[0]"),
            ("const view = state; view.volume = 2;", "state.volume"),
            (
                "state.volume = 2;",
                "(() => { const view = state; return view.volume; })()",
            ),
        ] {
            let source = format!(
                "{STATE}export const write = () => {{ {writer} }};\nexport const read = () => {reader};"
            );
            assert!(reaches(&source, "read", "write"), "{source}");
        }
    }

    #[test]
    fn a_use_of_the_whole_object_reaches_and_is_reached_by_every_property() {
        for writer in [
            // A method gets the object as `this`.
            "state.reset();",
            // Code elsewhere gets the object.
            "save(state);",
            "Object.assign(state, {});",
            "const copy = { ...state };",
            // A key nobody can name, and the prototype.
            "state[key] = 1;",
            "state.__proto__ = null;",
            // An alias that cannot be followed.
            "let view = state; view.theme = 'dark';",
            "const { theme } = state;",
        ] {
            let source = format!(
                "{STATE}export const write = () => {{ {writer} }};\nexport const read = () => state.volume;"
            );
            assert!(reaches(&source, "read", "write"), "{source}");
        }
        let source = format!(
            "{STATE}export const write = () => {{ state.theme = 'dark'; }};\nexport const read = () => typeof state;"
        );
        assert!(reaches(&source, "read", "write"), "{source}");
    }

    #[test]
    fn a_write_through_an_alias_is_credited_where_it_is_written() {
        // `view` is declared once and written through by `write`, so it is `write`
        // that a reader of the same property reaches.
        let source = format!(
            "{STATE}const view = state;
            export const write = () => {{ view.volume = 2; }};
            export const other = () => {{ view.theme = 'dark'; }};
            export const read = () => state.volume;"
        );
        assert!(reaches(&source, "read", "write"), "{source}");
        assert!(!reaches(&source, "read", "other"), "{source}");

        // An exported alias hands the object to code this file cannot see.
        let source = format!(
            "{STATE}export const view = state;
            export const read = () => state.volume;"
        );
        assert!(reaches(&source, "read", "view"), "{source}");
    }

    #[test]
    fn calling_a_member_is_a_read_but_changing_its_value_is_a_write() {
        // `utils` is read only for its members, so calling one cannot reach the
        // object; pushing onto one of them still changes what it holds.
        let source = "const utils = { items: [] as number[], format: (n: number) => `${n}` };
            export const add = () => { utils.items.push(1); };
            export const count = () => utils.items.length;
            export const show = () => utils.format(1);
            export const alsoShow = () => utils.format(2);";
        assert!(reaches(source, "count", "add"));
        assert!(!reaches(source, "show", "add"));
        assert!(!reaches(source, "show", "alsoShow"));
        assert!(!reaches(source, "alsoShow", "show"));
    }

    #[test]
    fn a_default_export_hands_a_written_object_to_its_importers() {
        // Importers may write any property of an object this file writes too, so
        // the default export uses all of it. One read only for its members is
        // exported as an `export { }` list would export it.
        let source = format!(
            "{STATE}export default state;
            export const read = () => state.volume;"
        );
        assert!(reaches(&source, "read", "default"), "{source}");

        let source = "const utils = { a: () => 1, b: () => 2 };
            export default utils;
            export const read = () => utils.a();";
        assert!(!reaches(source, "read", "default"), "{source}");
    }

    #[test]
    fn an_object_whose_properties_can_meet_keeps_the_whole_value_rule() {
        for state in [
            // Code runs on a read, and can read anything.
            "let state = { get theme() { return this.volume; }, volume: 1 };",
            "let state = { set theme(value) { this.volume = value; }, volume: 1 };",
            // A prototype of its own lends it properties.
            "let state = { __proto__: base, theme: 'light', volume: 1 };",
            // The binding can come to hold some other object.
            "let state = { theme: 'light', volume: 1 }; export const reset = () => { state = other; };",
            // Not a literal, so nobody knows what it holds.
            "let state = makeState();",
            "let state = new Map();",
        ] {
            let source = format!(
                "{state}\nexport const write = () => {{ state.theme = 'dark'; }};\nexport const read = () => state.volume;"
            );
            assert!(reaches(&source, "read", "write"), "{source}");
        }
    }
}
