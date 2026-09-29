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
//! in, since that is where the write happens. An alias that is exported, is not a
//! `const`, destructures, or is nested too deep is taken as writing the whole of
//! what it aliases, and each of its own uses as a use of the whole of it, credited
//! where that use is written.
//!
//! A collection the file makes itself, a `const` bound to `new Map()` or to an
//! array literal, is used as a whole too, and its aliases are followed the same
//! way. But its methods are known, so calling one that only reads it, in a way that
//! hands none of what it holds to other code, is a read of it rather than a write;
//! see [`collection`].

use ahash::AHashSet;
use oxc_ast::AstKind;
use oxc_ast::ast::*;
use oxc_semantic::{AstNodes, IsGlobalReference, NodeId, SymbolId};
use oxc_span::GetSpan;

use super::escape::{
    ALIAS_DEPTH, CallRule, Held, method_fate, rendered_in_place, through_wrappers,
};
use super::members::object_literal;
use super::parse::Ctx;
use super::refs::{Use, classify};

/// One way a reference touches a shared binding.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct Access {
    /// The property it goes through, or `None` for the whole value.
    pub property: Option<String>,
    /// Whether it could change what the binding, or that property, holds.
    pub write: bool,
    /// The method a write calls on the binding, where it is a call that some
    /// factory's rule declares a read of the value it makes and the call is written
    /// the way that rule's form says. Whether the binding holds such a value is the
    /// graph's to tell, so the access stays a write here; see [`method_fate`].
    pub read_call: Option<String>,
}

impl Access {
    fn whole(write: bool) -> Self {
        Self {
            property: None,
            write,
            read_call: None,
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
    /// A collection the file makes itself, whose methods are known. Every use is a
    /// use of the whole value, but calling a method that only reads it, in a way
    /// that hands none of what it holds to other code, is a read.
    Collection(Collection),
}

/// A built-in collection whose type the file shows: a module-scope `const` made
/// with `new Map()`, `new Set()`, `new WeakMap()` or `new WeakSet()` of the
/// globals, or with an array literal.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) enum Collection {
    Map,
    WeakMap,
    Set,
    WeakSet,
    Array,
}

/// Whether a collection's read method takes a callback as its first argument,
/// which it calls with what the collection holds.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) enum Callback {
    Never,
    Required,
    Optional,
}

impl Collection {
    /// The global whose prototype holds the collection's methods.
    fn constructor(self) -> &'static str {
        match self {
            Collection::Map => "Map",
            Collection::WeakMap => "WeakMap",
            Collection::Set => "Set",
            Collection::WeakSet => "WeakSet",
            Collection::Array => "Array",
        }
    }

    /// What calling `name` hands back and whether it takes a callback, where the
    /// built-in method of that name reads the collection and changes none of it.
    /// What it hands back is a primitive, which holds nothing of the collection's
    /// however it is used, or may be or hold an object the collection holds,
    /// through which a caller could change what every other reader of it sees.
    /// `size` and `length` are properties, which are read in place already.
    pub(crate) fn read_method(self, name: &str) -> Option<(Held, Callback)> {
        use Collection::*;
        use Held::{Primitive, Stored};
        let method = match (self, name) {
            (Map | WeakMap, "get") => (Stored, Callback::Never),
            (Map | WeakMap | Set | WeakSet, "has") => (Primitive, Callback::Never),
            (Map | Set | Array, "forEach") => (Primitive, Callback::Required),
            (Map | Set | Array, "keys" | "values" | "entries") => (Stored, Callback::Never),
            (Array, "every" | "some" | "findIndex" | "findLastIndex") => {
                (Primitive, Callback::Required)
            }
            (Array, "includes" | "indexOf" | "lastIndexOf" | "join" | "toString") => {
                (Primitive, Callback::Never)
            }
            (
                Array,
                "filter" | "find" | "findLast" | "flatMap" | "map" | "reduce" | "reduceRight",
            ) => (Stored, Callback::Required),
            (Array, "toSorted") => (Stored, Callback::Optional),
            (Array, "at" | "concat" | "flat" | "slice" | "toReversed" | "toSpliced" | "with") => {
                (Stored, Callback::Never)
            }
            _ => return None,
        };
        Some(method)
    }
}

/// The collection `symbol` holds, where the file shows what it is and that its
/// methods are the built-in ones.
///
/// The binding must be a `const` that nothing reassigns, initialised with an array
/// literal or with `new` of a global `Map`, `Set`, `WeakMap` or `WeakSet`, which a
/// class of the file or an import of the same name would shadow. A value whose own
/// methods may be replaced, or whose constructor's prototype may be, is not one:
/// anything written through it but an index or an array's `length`, anything
/// handed to a method of `Object` or `Reflect`, such as `Object.defineProperty`,
/// and any use of the constructor but `new`, `instanceof` or a call of one of its
/// own functions, such as `Array.isArray`. Aliases declared from the binding are
/// held to the same.
///
/// Handing the value to other code does not make it any less of a `Map`, and the
/// declaration that does so is a writer of it already.
pub(crate) fn collection(ctx: &Ctx<'_>, symbol: SymbolId) -> Option<Collection> {
    let scoping = ctx.semantic.scoping();
    let nodes = ctx.semantic.nodes();
    if scoping
        .get_resolved_reference_ids(symbol)
        .iter()
        .any(|id| scoping.get_reference(*id).is_write())
    {
        return None;
    }
    let declarator_id = scoping.symbol_declaration(symbol);
    let AstKind::VariableDeclarator(declarator) = nodes.kind(declarator_id) else {
        return None;
    };
    let AstKind::VariableDeclaration(variable) = nodes.parent_kind(declarator_id) else {
        return None;
    };
    if variable.kind != VariableDeclarationKind::Const
        || !matches!(&declarator.id, BindingPattern::BindingIdentifier(_))
    {
        return None;
    }
    let collection = match declarator.init.as_ref()?.get_inner_expression() {
        Expression::ArrayExpression(_) => Collection::Array,
        Expression::NewExpression(new) => match &new.callee {
            Expression::Identifier(callee) if callee.is_global_reference(scoping) => {
                match callee.name.as_str() {
                    "Map" => Collection::Map,
                    "WeakMap" => Collection::WeakMap,
                    "Set" => Collection::Set,
                    "WeakSet" => Collection::WeakSet,
                    _ => return None,
                }
            }
            _ => return None,
        },
        _ => return None,
    };
    (!constructor_touched(ctx, collection) && !methods_replaceable(ctx, symbol, collection))
        .then_some(collection)
}

/// Whether the file uses the collection's global constructor in a way that could
/// reach its prototype: `Map.prototype.get = …`, or handing `Map` to other code.
fn constructor_touched(ctx: &Ctx<'_>, collection: Collection) -> bool {
    let scoping = ctx.semantic.scoping();
    let nodes = ctx.semantic.nodes();
    let Some(references) = scoping
        .root_unresolved_references()
        .get(collection.constructor())
    else {
        return false;
    };
    references.iter().any(|id| {
        let reference = scoping.get_reference(*id);
        if !reference.is_value() {
            return false;
        }
        let (top, span) = through_wrappers(nodes, reference.node_id());
        match nodes.parent_kind(top) {
            AstKind::NewExpression(new) => new.callee.span() != span,
            AstKind::BinaryExpression(binary) => {
                binary.operator != BinaryOperator::Instanceof || binary.right.span() != span
            }
            AstKind::StaticMemberExpression(member)
                if member.object.span() == span && member.property.name != "prototype" =>
            {
                let (outer, outer_span) = through_wrappers(nodes, nodes.parent_id(top));
                !matches!(
                    nodes.parent_kind(outer),
                    AstKind::CallExpression(call) if call.callee.span() == outer_span
                )
            }
            _ => true,
        }
    })
}

/// Whether anything done through `symbol`, or through an alias declared from it,
/// could give the value methods of its own: a property written, deleted or
/// defined on it, or its prototype set. An index or an array's `length` written
/// leaves its methods as they were.
fn methods_replaceable(ctx: &Ctx<'_>, symbol: SymbolId, collection: Collection) -> bool {
    let scoping = ctx.semantic.scoping();
    let nodes = ctx.semantic.nodes();
    let mut visited = AHashSet::default();
    let mut pending = vec![symbol];
    while let Some(symbol) = pending.pop() {
        if !visited.insert(symbol) {
            continue;
        }
        for reference_id in scoping.get_resolved_reference_ids(symbol) {
            let (top, span) =
                through_wrappers(nodes, scoping.get_reference(*reference_id).node_id());
            let member = nodes.parent_id(top);
            match nodes.parent_kind(top) {
                AstKind::StaticMemberExpression(written) if written.object.span() == span => {
                    if member_written(nodes, member)
                        && !(collection == Collection::Array && written.property.name == "length")
                    {
                        return true;
                    }
                }
                AstKind::ComputedMemberExpression(written) if written.object.span() == span => {
                    if member_written(nodes, member)
                        && !(collection == Collection::Array
                            && matches!(written.expression, Expression::NumericLiteral(_)))
                    {
                        return true;
                    }
                }
                AstKind::CallExpression(call) if call.callee.span() != span => {
                    if let Expression::StaticMemberExpression(callee) =
                        call.callee.get_inner_expression()
                        && let Expression::Identifier(object) = callee.object.get_inner_expression()
                        && matches!(object.name.as_str(), "Object" | "Reflect")
                        && object.is_global_reference(scoping)
                    {
                        return true;
                    }
                }
                AstKind::VariableDeclarator(declarator)
                    if declarator
                        .init
                        .as_ref()
                        .is_some_and(|init| init.span() == span) =>
                {
                    pending.extend(
                        declarator
                            .id
                            .get_binding_identifiers()
                            .iter()
                            .filter_map(|binding| binding.symbol_id.get()),
                    );
                }
                _ => {}
            }
        }
    }
    false
}

/// Whether the member expression at `member` is written, deleted or assigned to by
/// a pattern, rather than read.
fn member_written(nodes: &AstNodes<'_>, member: NodeId) -> bool {
    let (top, span) = through_wrappers(nodes, member);
    match nodes.parent_kind(top) {
        AstKind::AssignmentExpression(assignment) => assignment.left.span() == span,
        AstKind::ForInStatement(statement) => statement.left.span() == span,
        AstKind::ForOfStatement(statement) => statement.left.span() == span,
        AstKind::UnaryExpression(unary) => unary.operator == UnaryOperator::Delete,
        AstKind::UpdateExpression(_)
        | AstKind::AssignmentTargetPropertyIdentifier(_)
        | AstKind::AssignmentTargetPropertyProperty(_)
        | AstKind::ArrayAssignmentTarget(_)
        | AstKind::AssignmentTargetRest(_)
        | AstKind::AssignmentTargetWithDefault(_) => true,
        _ => false,
    }
}

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
    let Some(object) = declarator
        .init
        .as_ref()
        .and_then(|init| object_literal(init, Some(scoping)))
    else {
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
    let mut uses = if mode == Mode::Whole {
        let write = classify(ctx.semantic.nodes(), node_id) == Use::Mutate;
        vec![(at, Access::whole(write))]
    } else {
        reference_accesses(ctx, node_id, mode, 0)
    };
    // A method call on the binding writes the whole of it, as far as this file can
    // tell. The graph may yet find that the method only reads what made the value,
    // where some factory's rule declares it a read of the value it makes and the
    // call is written the way that rule's form says.
    if let [(offset, access)] = uses.as_mut_slice()
        && *offset == at
        && access.write
        && access.property.is_none()
    {
        access.read_call = method_fate(ctx, node_id, CallRule::Factory)
            .filter(|(_, fate)| fate.in_place())
            .map(|(method, _)| method.to_string());
    }
    uses
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
                // An alias this cannot follow is taken as writing the whole object
                // where it is declared, and each of its uses as a use of the whole
                // object where that use is written.
                None => {
                    let mut uses = whole(true);
                    uses.extend(
                        untracked_uses(ctx, top, depth)
                            .into_iter()
                            .map(|(offset, write)| (offset, Access::whole(write))),
                    );
                    uses
                }
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
    // A collection is one value, however it is reached into. What is done with an
    // element read off it counts as done to it, however deep the chain goes, so
    // `list[0].count = 1` writes `list`.
    if let Mode::Collection(collection) = mode {
        return match property_use(ctx, nodes.parent_id(top), depth) {
            PropertyUse::Receiver => whole(
                !method_fate(ctx, node_id, CallRule::Collection(collection))
                    .is_some_and(|(_, fate)| fate.in_place()),
            ),
            PropertyUse::Uses(uses) => uses
                .into_iter()
                .map(|(offset, write)| (offset.unwrap_or(at), Access::whole(write)))
                .collect(),
        };
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
                    read_call: None,
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
                        read_call: None,
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
                // As for an alias of the object, but of this one property.
                None => PropertyUse::Uses(
                    std::iter::once((None, true))
                        .chain(
                            untracked_uses(ctx, top, depth)
                                .into_iter()
                                .map(|(offset, write)| (Some(offset), write)),
                        )
                        .collect(),
                ),
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
        AstKind::JSXExpressionContainer(_) => {
            written(!rendered_in_place(nodes, nodes.parent_id(top)))
        }
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

/// The uses of every binding the declarator above `value` introduces, each with
/// where it is written and whether it could change what it reaches, for an alias
/// [`followed_alias`] could not follow.
///
/// `export let view = state` is still a name for `state`, and a write through it
/// elsewhere in the file is a write to `state` made there, whatever else may
/// happen to `view`. Past the depth aliases are followed to, every use is taken as
/// a write, and aliases declared from it are still followed; see [`writes_through`].
fn untracked_uses(ctx: &Ctx<'_>, value: NodeId, depth: usize) -> Vec<(u32, bool)> {
    let nodes = ctx.semantic.nodes();
    let AstKind::VariableDeclarator(declarator) = nodes.kind(nodes.parent_id(value)) else {
        return Vec::new();
    };
    if depth >= ALIAS_DEPTH {
        return writes_through(ctx, declarator);
    }
    let scoping = ctx.semantic.scoping();
    let mut uses = Vec::new();
    for binding in declarator.id.get_binding_identifiers() {
        let Some(symbol) = binding.symbol_id.get() else {
            continue;
        };
        for reference_id in scoping.get_resolved_reference_ids(symbol) {
            let node_id = scoping.get_reference(*reference_id).node_id();
            uses.extend(
                reference_accesses(ctx, node_id, Mode::Properties, depth + 1)
                    .into_iter()
                    .map(|(offset, access)| (offset, access.write)),
            );
        }
    }
    uses
}

/// Every use of the bindings a declarator introduces, each taken as a write, and
/// the uses of every alias declared from one of them in turn, however far the chain
/// goes.
///
/// A declaration that names a binding is not one of its users, so nothing else
/// connects a write through `const f = e` to the readers of what `e` aliases. An
/// assignment is different: `g = e` uses `g`, and the shared-state rule already
/// connects whoever does to whoever writes through `g`.
///
/// A chain can be as long as the file, so it is walked with a worklist rather than
/// by recursion, which would take a stack frame per alias.
fn writes_through(ctx: &Ctx<'_>, declarator: &VariableDeclarator<'_>) -> Vec<(u32, bool)> {
    let nodes = ctx.semantic.nodes();
    let scoping = ctx.semantic.scoping();
    let bindings = |declarator: &VariableDeclarator<'_>| -> Vec<SymbolId> {
        declarator
            .id
            .get_binding_identifiers()
            .iter()
            .filter_map(|binding| binding.symbol_id.get())
            .collect()
    };

    let mut uses = Vec::new();
    let mut visited = AHashSet::default();
    let mut pending = bindings(declarator);
    while let Some(symbol) = pending.pop() {
        if !visited.insert(symbol) {
            continue;
        }
        for reference_id in scoping.get_resolved_reference_ids(symbol) {
            let node_id = scoping.get_reference(*reference_id).node_id();
            let at = nodes.get_node(node_id).kind().span().start;
            // Declaring another alias does nothing to the object; what that alias
            // is used for is followed in its place. One that is exported, or can be
            // reassigned, also hands the object on where it is declared, as it
            // does short of the limit.
            match declared_from(nodes, node_id) {
                Some((next, hands_on)) => {
                    if hands_on {
                        uses.push((at, true));
                    }
                    pending.extend(bindings(next));
                }
                None => uses.push((at, true)),
            }
        }
    }
    uses
}

/// The declarator whose initialiser is this reference, or a property read off it:
/// `const f = e` and `const list = e.list` both declare an alias of what `e` holds.
/// Paired with whether the declaration hands the alias to code this cannot follow:
/// it is exported, or it is not a `const`.
fn declared_from<'n, 'a>(
    nodes: &'n AstNodes<'a>,
    node_id: NodeId,
) -> Option<(&'n VariableDeclarator<'a>, bool)> {
    let mut current = node_id;
    loop {
        let (outer, span) = through_wrappers(nodes, current);
        match nodes.parent_kind(outer) {
            AstKind::StaticMemberExpression(member) if member.object.span() == span => {
                current = nodes.parent_id(outer);
            }
            AstKind::ComputedMemberExpression(member) if member.object.span() == span => {
                current = nodes.parent_id(outer);
            }
            AstKind::ChainExpression(_) => current = nodes.parent_id(outer),
            AstKind::VariableDeclarator(declarator)
                if declarator
                    .init
                    .as_ref()
                    .is_some_and(|init| init.span() == span) =>
            {
                let declaration = nodes.parent_id(nodes.parent_id(outer));
                let hands_on = !matches!(
                    nodes.kind(declaration),
                    AstKind::VariableDeclaration(variable)
                        if variable.kind == VariableDeclarationKind::Const
                ) || matches!(
                    nodes.parent_kind(declaration),
                    AstKind::ExportDeclaration(_)
                );
                return Some((declarator, hands_on));
            }
            _ => return None,
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

    /// The writers recorded on `name` in `source`, by name, with what each writes.
    fn writers(source: &str, name: &str) -> Vec<(String, Option<String>)> {
        let (ModuleAnalysis::Fine(module), _) =
            analyse_source(Path::new("state.ts"), source, &Reading::default()).unwrap()
        else {
            panic!("expected fine module: {source}")
        };
        let decl = module.decl_named(name).expect(name);
        module.decls[decl as usize]
            .writers
            .iter()
            .map(|(writer, written)| (module.decls[*writer as usize].name.clone(), written.clone()))
            .collect()
    }

    const STATE: &str = "let state = { theme: 'light', volume: 1, list: [] as number[] };\n";

    #[test]
    fn a_binding_records_the_declarations_that_write_it() {
        let source = "export const cache = new Map();
            export function reset() { cache.set('a', 1); }
            export const read = () => cache.size;";
        assert_eq!(writers(source, "cache"), [("reset".to_string(), None)]);

        let source = format!(
            "{STATE}export const write = () => {{ state.theme = 'dark'; }};
            export const read = () => state.volume;"
        );
        assert_eq!(
            writers(&source, "state"),
            [("write".to_string(), Some("theme".to_string()))]
        );

        // Nothing writes it, so a reader elsewhere has nothing more to reach.
        let source = "export const cache = new Map();
            export const read = () => cache.size;";
        assert!(writers(source, "cache").is_empty());
    }

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
    fn a_write_through_an_alias_that_cannot_be_followed_still_reaches_readers() {
        for (alias, writer) in [
            ("export let view = state;", "view.volume = 2;"),
            ("let view = state;", "view.volume = 2;"),
            ("export const view = state;", "view.volume = 2;"),
            ("const { list } = state;", "list.push(1);"),
            ("export let volume = state.list;", "volume.push(1);"),
            ("let a = state; let b = a;", "b.volume = 2;"),
            (
                "const a = state; const b = a; const c = b; const d = c; const e = d;",
                "e.volume = 2;",
            ),
        ] {
            let source = format!(
                "{STATE}{alias}
                export const write = () => {{ {writer} }};
                export const read = () => [state.volume, state.list];"
            );
            assert!(reaches(&source, "read", "write"), "{source}");
        }
    }

    #[test]
    fn aliases_declared_past_the_depth_limit_are_still_followed() {
        const CHAIN: &str = "const a = state; const b = a; const c = b; const d = c; const e = d;";
        for (alias, writer) in [
            ("const f = e;", "f.volume = 2;"),
            ("const f = e; const g = f;", "g.volume = 2;"),
            ("const list = e.list;", "list.push(1);"),
            ("const f = e; export let g = f;", "g.volume = 2;"),
        ] {
            let source = format!(
                "{STATE}{CHAIN} {alias}
                export const write = () => {{ {writer} }};
                export const read = () => state.volume;"
            );
            assert!(reaches(&source, "read", "write"), "{source}");
        }
    }

    #[test]
    fn an_alias_exported_past_the_depth_limit_still_hands_the_object_on() {
        const CHAIN: &str = "const a = state; const b = a; const c = b; const d = c; const e = d;";
        for alias in ["export const view = e;", "let view = e;"] {
            let source = format!(
                "{STATE}{CHAIN}
                {alias}
                export const read = () => state.volume;"
            );
            assert!(reaches(&source, "read", "view"), "{source}");
        }
    }

    #[test]
    fn a_chain_of_aliases_as_long_as_the_file_is_followed_to_its_end() {
        // Long enough that a stack frame per alias would not fit in a test thread's
        // stack, as generated code can be.
        let chain: String = (1..10_000)
            .map(|i| format!("const a{i} = a{};\n", i - 1))
            .collect();
        let source = format!(
            "{STATE}const a0 = state;\n{chain}
            export const write = () => {{ a9999.volume = 2; }};
            export const read = () => state.volume;"
        );
        assert!(reaches(&source, "read", "write"));
    }

    #[test]
    fn a_frozen_literal_has_independent_properties_too() {
        let source =
            "let state = Object.freeze({ theme: { name: 'light' }, volume: { level: 1 } });
            export const write = () => { state.theme.name = 'dark'; };
            export const read = () => state.volume.level;";
        assert!(!reaches(source, "read", "write"), "{source}");

        // A local `Object` could return anything, so the binding keeps the
        // whole-value rule, under which any direct write reaches every read.
        let source = "const Object = { freeze: (x) => x };
            let state = Object.freeze({ theme: 'light', volume: 1 });
            export const write = () => { state.theme = 'dark'; };
            export const read = () => state.volume;";
        assert!(reaches(source, "read", "write"), "{source}");
    }

    /// Whether `peek`, declared as `body` beside `binding`, writes `V`: whether a
    /// reader of `V` in the same file reaches it, and whether `V` records it for a
    /// reader in another module. The two must agree.
    fn peek_writes(binding: &str, body: &str) -> bool {
        let source = format!(
            "{binding}\nexport const peek = (k: any, x: any) => {body};\nexport const read = () => typeof V;\n"
        );
        let (ModuleAnalysis::Fine(module), _) =
            analyse_source(Path::new("state.tsx"), &source, &Reading::default()).unwrap()
        else {
            panic!("expected fine module: {source}")
        };
        let decl = |name: &str| module.decl_named(name).expect(name);
        let (read, peek, v) = (decl("read"), decl("peek"), decl("V"));
        let local = module.decls[read as usize].refs.contains(&peek);
        let exported = module.decls[v as usize]
            .writers
            .iter()
            .any(|(writer, _)| *writer == peek);
        assert_eq!(local, exported, "{source}");
        local
    }

    const MAP: &str = "const V = new Map<string, any>();";
    const WEAK_MAP: &str = "const V = new WeakMap<object, any>();";
    const SET: &str = "const V = new Set<any>();";
    const WEAK_SET: &str = "const V = new WeakSet<object>();";
    const ARRAY: &str = "const V: any[] = [];";

    #[test]
    fn a_read_method_on_a_collection_made_in_the_file_does_not_write_it() {
        let map_like = ["V.has(k)", "V.get(k) === x", "{ if (V.get(k)) go(); }"];
        let iterable = [
            "V.forEach((value) => { if (value.count > 0) go(); })",
            "V.keys() === x",
            "V.values() !== x",
            "V.entries() === x",
        ];
        for body in map_like.iter().chain(&iterable) {
            assert!(!peek_writes(MAP, body), "{body}");
        }
        assert!(!peek_writes(MAP, "V.size > 0"));
        for body in map_like {
            assert!(!peek_writes(WEAK_MAP, body), "{body}");
        }
        for body in iterable.iter().chain(&["V.has(k)"]) {
            assert!(!peek_writes(SET, body), "{body}");
        }
        assert!(!peek_writes(WEAK_SET, "V.has(k)"));
        for body in [
            "V.at(0) === x",
            "V.concat([1]) === x",
            "V.entries() === x",
            "V.every((item) => item.count > 0)",
            "V.filter((item) => item.count > 0).length > 0",
            "V.find((item) => item.id === k) !== undefined",
            "V.findIndex((item) => item.id === k)",
            "V.findLast((item) => item.id === k) !== undefined",
            "V.findLastIndex((item) => item.id === k)",
            "V.flat() === x",
            "V.flatMap((item) => item.children).length > 0",
            "V.forEach((item, index) => { if (item.count > index) go(); })",
            "V.includes(x)",
            "V.indexOf(x) >= 0",
            "V.join(', ')",
            "V.keys() === x",
            "V.lastIndexOf(x)",
            "V.map((item) => item.count * 2).length > 0",
            "V.reduce((sum, item) => sum + item.count, 0) > 1",
            "V.reduceRight((sum, item) => sum + item.count, 0) > 1",
            "V.slice(1).length > 0",
            "V.some(({ id }) => id === k)",
            "V.toReversed()[0] === x",
            "V.toSorted((a, b) => a.count - b.count)[0] === x",
            "V.toSorted()[0] === x",
            "V.toSpliced(0, 1).length > 0",
            "V.values() === x",
            "V.with(0, x).length > 0",
            "V.toString()",
            "V.length > 0",
        ] {
            assert!(!peek_writes(ARRAY, body), "{body}");
        }
        // An array literal with elements, and a collection's type or value read
        // through wrappers that change neither.
        assert!(!peek_writes("const V = [1, 2];", "V.includes(x)"));
        assert!(!peek_writes("const V = [] as number[];", "V.includes(x)"));
        assert!(!peek_writes("const V = (new Map());", "V.has(k)"));
        assert!(!peek_writes(MAP, "(V as Map<string, any>).has(k)"));
        assert!(!peek_writes(MAP, "V?.has(k)"));
    }

    #[test]
    fn what_a_read_method_hands_out_is_a_read_only_where_it_is_used_in_place() {
        for body in [
            "V.get(k) === x",
            "V.get(k).count > 0",
            "V.get(k)?.count > 0",
            "V.get(k)['count'] + 1",
            "`${V.get(k).name}`",
            "{ if (V.get(k).ready) go(); }",
            "V.get(k).ready ? 1 : 2",
            "!V.get(k)",
            "{ const { name } = V.get(k); return name === x; }",
            "{ const item = V.get(k); return item.count > 0; }",
            "<p>{V.get(k).name}</p>",
            "<>{V.get(k)}</>",
        ] {
            assert!(!peek_writes(MAP, body), "{body}");
        }
        for body in [
            "V.find((item) => item.id === k) !== undefined",
            "<ul>{V.map((item) => <li key={item.id}>{item.name}</li>)}</ul>",
        ] {
            assert!(!peek_writes(ARRAY, body), "{body}");
        }
        for body in [
            // Returned, passed, stored, spread, or a method called on it.
            "V.get(k)",
            "{ return V.get(k); }",
            "V.get(k).name",
            "register(V.get(k))",
            "{ held = V.get(k); }",
            "({ item: V.get(k) })",
            "[V.get(k)]",
            "[...V.values()]",
            "{ for (const value of V.values()) go(value); }",
            "V.get(k).reset()",
            "V.get(k).items.push(1)",
            "V.get(k).count = 1",
            "{ V.get(k).count++; }",
            "{ delete V.get(k).count; }",
            "{ const item = V.get(k); register(item); }",
            "{ const item = V.get(k); item.count = 1; }",
            "{ let item = V.get(k); return item === x; }",
            "{ const { item } = V.get(k); item.count = 1; }",
            "{ const { ...rest } = V.get(k); return rest === x; }",
            // Handed to a component as a prop, or as its children.
            "<Row item={V.get(k)} />",
            "<Row>{V.get(k)}</Row>",
            "<p title={V.get(k).name} />",
            "V.keys().next()",
        ] {
            assert!(peek_writes(MAP, body), "{body}");
        }
        for body in [
            "V.find((item) => item.id === k)",
            "V.at(0)",
            "V.filter((item) => item.ready)",
            "V.slice()",
            "V.map((item) => item)",
            "V.concat([])",
            "V.toSorted()",
            "V.filter((item) => item.ready).map((item) => item.id)",
            "V.at(0).count = 1",
            "V.reduce((acc, item) => item)",
            "<List>{V.map((item) => <li key={item.id}>{item.name}</li>)}</List>",
        ] {
            assert!(peek_writes(ARRAY, body), "{body}");
        }
        // A method that hands out a primitive is a read however that is used.
        for body in [
            "V.includes(x)",
            "{ return V.indexOf(x); }",
            "register(V.some((item) => item.ready))",
            "register(V.join(', '))",
        ] {
            assert!(!peek_writes(ARRAY, body), "{body}");
        }
        assert!(!peek_writes(MAP, "register(V.has(k))"));
    }

    #[test]
    fn a_method_that_changes_a_collection_still_writes_it() {
        for body in ["V.set(k, x)", "V.delete(k)", "V.clear()", "V.other()"] {
            assert!(peek_writes(MAP, body), "{body}");
        }
        for body in ["V.set(k, x)", "V.delete(k)"] {
            assert!(peek_writes(WEAK_MAP, body), "{body}");
        }
        for body in ["V.add(x)", "V.delete(x)", "V.clear()"] {
            assert!(peek_writes(SET, body), "{body}");
        }
        for body in ["V.add(x)", "V.delete(x)"] {
            assert!(peek_writes(WEAK_SET, body), "{body}");
        }
        // A method one collection reads with is no read of another.
        assert!(peek_writes(WEAK_MAP, "V.forEach((value) => value === x)"));
        assert!(peek_writes(SET, "V.get(k) === x"));
        assert!(peek_writes(WEAK_SET, "V.keys() === x"));
        for body in [
            "V.push(x)",
            "V.pop()",
            "V.shift()",
            "V.unshift(x)",
            "V.splice(0, 1)",
            "V.sort()",
            "V.reverse()",
            "V.fill(0)",
            "V.copyWithin(0, 1)",
            "V.get(k) === x",
            // Writing through an element, and handing the array on.
            "{ V[0].count = 1; }",
            "{ V.length = 0; }",
            "register(V)",
            "V.concat(V).length",
        ] {
            assert!(peek_writes(ARRAY, body), "{body}");
        }
    }

    #[test]
    fn a_collection_whose_type_cannot_be_seen_keeps_the_whole_value_rule() {
        for binding in [
            "let V = new Map<string, any>();",
            "var V = new Map<string, any>();",
            "let V: any[] = [];",
            "let V: any[] = []; export const reset = () => { V = []; };",
            "const V = makeMap();",
            "const V = new Map<string, any>() || other;",
            "const V = Array.from(items);",
            "const V = new Array<any>();",
            "const V = new Map2<string, any>();",
            // A `Map` of the file's own, or one imported, may do anything.
            "class Map { has(k: any) { register(k); return true; } }\nconst V = new Map();",
            "function Map() {}\nconst V = new (Map as any)();",
            "import { Map } from './map';\nconst V = new Map();",
            // A method replaced on the value, or on what it inherits from.
            "const V = new Map<string, any>();\nexport const patch = () => { V.has = () => true; };",
            "const V = new Map<string, any>();\nexport const patch = () => { V['has'] = () => true; };",
            "const V = new Map<string, any>();\nexport const patch = () => Object.defineProperty(V, 'has', { value: () => true });",
            "const V = new Map<string, any>();\nexport const patch = () => Object.setPrototypeOf(V, other);",
            "const V = new Map<string, any>();\nexport const patch = () => Reflect.set(V, 'has', other);",
            "const V = new Map<string, any>();\nexport const patch = () => { const alias = V; alias.has = () => true; };",
            "const V = new Map<string, any>();\nexport const patch = () => { Map.prototype.has = () => true; };",
            "const V = new Map<string, any>();\nObject.defineProperty(Map.prototype, 'has', { value: () => true });",
        ] {
            assert!(peek_writes(binding, "V.has(k)"), "{binding}");
        }
        for binding in [
            "const V: any[] = [];\nexport const patch = () => { Array.prototype.includes = () => true; };",
            "const V: any[] = [];\nexport const patch = () => { V.includes = () => true; };",
            "const V: any[] = [];\nexport const patch = () => { V[key] = () => true; };",
        ] {
            assert!(peek_writes(binding, "V.includes(x)"), "{binding}");
        }
        // An element or the length written leaves the methods as they were.
        for binding in [
            "const V: any[] = [];\nexport const put = () => { V[0] = 1; };",
            "const V: any[] = [];\nexport const put = () => { V.length = 0; };",
        ] {
            assert!(!peek_writes(binding, "V.includes(x)"), "{binding}");
        }
        // Naming the constructor as a type, checking against it, or calling one of
        // its own functions leaves its prototype be.
        for (binding, body) in [
            ("const V: Map<string, number> = new Map();", "V.has(k)"),
            (
                "const V = new Map<string, any>();\nexport const isMap = (value: unknown) => value instanceof Map;",
                "V.has(k)",
            ),
            (
                "const V: any[] = [];\nexport const isList = (value: unknown) => Array.isArray(value);",
                "V.includes(x)",
            ),
        ] {
            assert!(!peek_writes(binding, body), "{binding}");
        }
        // Handed to other code, the value is still what it was made as, and the
        // declaration that hands it on is a writer of it, as it always was.
        let binding = "const V = new Map<string, any>();\nexport const share = () => register(V);";
        assert!(!peek_writes(binding, "V.has(k)"));
        let source = format!("{binding}\nexport const read = () => V.has(1);\n");
        assert!(reaches_tsx(&source, "read", "share"));
    }

    #[test]
    fn a_write_anywhere_down_a_member_chain_writes_a_whole_value() {
        for binding in [
            "const V = make();",
            "let V = make();\nexport const reset = () => { V = make(); };",
        ] {
            for body in [
                "{ V.a.b = 1; }",
                "{ V.a.b += 1; }",
                "{ V.a[k]++; }",
                "{ delete V.a.b; }",
                "V.items.push(1)",
                "V.a.b.c()",
            ] {
                assert!(peek_writes(binding, body), "{binding} {body}");
            }
            for body in ["V.items.length", "V.a.b", "V.a.b === x", "`${V.a.b}`"] {
                assert!(!peek_writes(binding, body), "{binding} {body}");
            }
        }
    }

    #[test]
    fn a_property_handed_to_a_component_is_written_however_the_binding_is_read() {
        // An object tracked property by property, and one read only for its
        // members: what a component is handed it may change.
        for (binding, read) in [
            (STATE, "state.list.length"),
            (
                "const state = { list: [] as number[], format: (n: number) => `${n}` };\n",
                "state.list.length",
            ),
        ] {
            for writer in [
                "export const Write = () => <Foo items={state.list} />;",
                "export const Write = () => <Foo>{state.list}</Foo>;",
                "export const Write = () => <div onClick={state.list} />;",
            ] {
                let source = format!("{binding}{writer}\nexport const read = () => {read};");
                assert!(reaches_tsx(&source, "read", "Write"), "{source}");
            }
            for writer in [
                "export const Write = () => <li>{state.list}</li>;",
                "export const Write = () => <li key={state.list} />;",
            ] {
                let source = format!("{binding}{writer}\nexport const read = () => {read};");
                assert!(!reaches_tsx(&source, "read", "Write"), "{source}");
            }
        }
        // The second is read only for its members: calling `format` reads that
        // member, and meets no write of `list`.
        let source = "const state = { list: [] as number[], format: (n: number) => `${n}` };
            export const Write = () => <Foo items={state.list} />;
            export const show = () => state.format(1);";
        assert!(!reaches_tsx(source, "show", "Write"));
        // A collection, handed an element of it.
        assert!(peek_writes(ARRAY, "<Row item={V[0]} />"));
        assert!(peek_writes(ARRAY, "<Row>{V[0]}</Row>"));
        assert!(!peek_writes(ARRAY, "<li>{V[0]}</li>"));
    }

    #[test]
    fn a_method_called_on_a_members_member_writes_that_member() {
        // `utils` is read only for its members, and a call on something read off
        // one of them hands that something, not `utils`, to the method.
        let source =
            "const utils = { list: { items: [] as number[] }, format: (n: number) => `${n}` };
            export const add = () => { utils.list.items.push(1); };
            export const clear = () => utils.list.reset();
            export const count = () => utils.list.items.length;
            export const show = () => utils.format(1);";
        assert!(reaches(source, "count", "add"));
        assert!(reaches(source, "count", "clear"));
        assert!(!reaches(source, "show", "add"));
        assert!(!reaches(source, "show", "clear"));
    }

    #[test]
    fn a_callback_that_writes_still_makes_its_declaration_a_writer() {
        for body in [
            // The collection written by name in the callback.
            "V.forEach((item) => V.push(item))",
            "V.forEach((item) => { V.splice(0, 1); })",
            // An element written, handed on, returned or called.
            "V.forEach((item) => { item.count = 1; })",
            "V.forEach((item) => item.reset())",
            "V.forEach((item) => register(item))",
            "V.some((item) => { return item; })",
            "V.forEach((item) => { later = () => item; })",
            "V.forEach((item, index, all) => all.push(item))",
            "V.reduce((acc, item) => { acc.push(item); return acc; }, []).length",
            "V.toSorted((a, b) => { a.count = b.count; return 0; })[0] === x",
            // Parameters this cannot follow, and a function whose `arguments` hold
            // what it is called with.
            "V.forEach((...items) => register(items))",
            "V.forEach((item = other) => item === x)",
            "V.forEach(([first]) => first === x)",
            "V.forEach(function (item) { if (item.count > 0) go(); })",
            "V.forEach()",
        ] {
            assert!(peek_writes(ARRAY, body), "{body}");
        }
        assert!(peek_writes(MAP, "V.forEach((value) => V.delete(value))"));
        assert!(peek_writes(
            SET,
            "V.forEach((value) => { value.count = 1; })"
        ));
    }

    #[test]
    fn a_callback_passed_by_name_still_writes_the_collection() {
        // `mutate` changes each element it is handed, and nothing about it names
        // the array, so only the call that hands it the elements can link it.
        let binding = "const V: any[] = [];\nconst mutate = (item: any) => { item.count = 1; };\nconst isReady = (item: any) => item.ready === true;";
        for body in [
            "V.forEach(mutate)",
            "V.some(isReady)",
            "V.map(isReady).length",
            "V.filter(callbacks.ready).length",
        ] {
            assert!(peek_writes(binding, body), "{body}");
        }
        // The reader reaches `mutate` through the call.
        let source = format!(
            "{binding}\nexport const peek = () => V.forEach(mutate);\nexport const read = () => V.includes(1);\n"
        );
        assert!(reaches_tsx(&source, "read", "peek"));
        assert!(reaches_tsx(&source, "peek", "mutate"));
    }

    #[test]
    fn a_local_const_alias_of_a_collection_is_followed_to_its_uses() {
        assert!(!peek_writes(
            MAP,
            "{ const alias = V; return alias.has(k); }"
        ));
        assert!(!peek_writes(
            MAP,
            "{ const alias = V; return alias.get(k) === x; }"
        ));
        assert!(peek_writes(MAP, "{ const alias = V; alias.set(k, x); }"));
        assert!(peek_writes(
            MAP,
            "{ const alias = V; return alias.get(k); }"
        ));
        assert!(peek_writes(MAP, "{ let alias = V; return alias.has(k); }"));
        // A write through an alias declared elsewhere is credited where it is made.
        let source = format!(
            "{MAP}\nconst alias = V;\nexport const peek = (k: any) => alias.set(k, 1);\nexport const read = () => V.has(1);\n"
        );
        assert!(reaches_tsx(&source, "read", "peek"));
    }

    /// Whether `reader` reaches `writer` in `source`, read as TSX.
    fn reaches_tsx(source: &str, reader: &str, writer: &str) -> bool {
        let (ModuleAnalysis::Fine(module), _) =
            analyse_source(Path::new("state.tsx"), source, &Reading::default()).unwrap()
        else {
            panic!("expected fine module: {source}")
        };
        let reader = module.decl_named(reader).expect(reader);
        let writer = module.decl_named(writer).expect(writer);
        module.decls[reader as usize].refs.contains(&writer)
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
