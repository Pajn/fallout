//! What happens to a value where it is used: whether it stays in the expression it
//! is read in, or leaves it for code that could change it.
//!
//! The shared-state rule asks this of every use of a mutable module-scope binding,
//! and of what a Zustand store or a collection the file makes hands out. This
//! module answers it behind one interface, [`uses`]: given a value and what it
//! holds, it climbs out through what leaves the value as it was — parentheses,
//! type-only wrappers, member chains, optional chaining — and judges the position
//! it lands in. What differs from one kind of binding to another is a [`Policy`],
//! not a walk of its own, so each position is judged in one place and two kinds of
//! binding can be told apart by reading their policies. That is the module's
//! depth: a caller passes a value, and the list of positions stays in the
//! implementation, where a change to one is made once and seen, in the table its
//! tests hold, for every kind of binding.
//!
//! A method called on the value is judged by a [`CallRule`], which knows what a
//! collection's read methods hand back and which calls a factory's rule declares
//! reads. What such a call hands out, and what a callback it takes is called with,
//! goes back through [`uses`].

use oxc_ast::AstKind;
use oxc_ast::ast::{
    Argument, BinaryOperator, BindingPattern, CallExpression, FormalParameters, JSXElementName,
    LogicalOperator, UnaryOperator, VariableDeclarationKind,
};
use oxc_semantic::{AstNodes, NodeId};
use oxc_span::{GetSpan, Span as OxcSpan};

use super::parse::Ctx;
use super::shared::{Callback, Collection};
use crate::factories::rules::{Reads, read_forms};

/// How far aliases of aliases are followed. Past it, what is handed out is taken
/// to escape, and an alias of a shared object to use the whole of it.
pub(crate) const ALIAS_DEPTH: usize = 4;

/// What happens to a value where it is used.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) enum Fate {
    /// It stays in the expression it is read in: compared, tested, coerced to a
    /// primitive, rendered where it stands, or dropped.
    InPlace,
    /// It is handed to code that could change it: passed on, returned, stored,
    /// spread or iterated.
    Escapes,
    /// It is changed where it stands: assigned, updated, deleted, or called as a
    /// method on what it was read off.
    Writes,
}

impl Fate {
    pub(crate) fn in_place(self) -> bool {
        self == Fate::InPlace
    }
}

/// What a value holds, which decides what may be done with it in place.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) enum Held {
    /// A shared binding's value, used bare: a reference to the binding itself.
    Whole,
    /// Something read off a shared binding's value: a property, or anything read
    /// off one. What it holds is part of the value, so a change made through it is
    /// a change to the value.
    Part,
    /// A Zustand store's state, whose properties may be actions.
    State,
    /// Something read off the state.
    StatePart,
    /// What a collection hands out, which may be an object it holds, as may
    /// anything read off one. A change made through it is one every other reader
    /// of the collection sees.
    Stored,
    /// A boolean, a number, a string or nothing, which holds nothing of the value
    /// it came from, however it is used.
    Primitive,
}

impl Held {
    /// What something read off a value holding this holds.
    fn read_off(self) -> Self {
        match self {
            Held::Whole | Held::Part => Held::Part,
            Held::State | Held::StatePart => Held::StatePart,
            other => other,
        }
    }
}

/// How the positions a value can land in are judged for one kind of binding.
///
/// A position whose reading every kind of binding shares is not here: an
/// assignment writes, a comparison reads, and a component handed the value may
/// change it, whatever the value is.
#[derive(Copy, Clone, Debug)]
pub(crate) struct Policy {
    /// Put through arithmetic, as in `v + 1` or `-v`, or into an untagged
    /// template, which coerces it to a primitive. Something read off the value is
    /// in place there whatever this says.
    arithmetic: Fate,
    /// Tested for truth, as `if (v)`, `v ? a : b` and `!v` do. Something read off
    /// the value is in place there whatever this says.
    test: Fate,
    /// Dropped, as an expression statement or `void v` does. Something read off
    /// the value is in place there whatever this says.
    discard: Fate,
    /// Where an arrow's expression body is what the caller does no more with than
    /// with what it hands out, the span of that arrow. Anywhere else a return
    /// hands the value on.
    returns: Option<OxcSpan>,
    /// How a binding declared to hold the value is followed.
    aliases: Aliases,
    /// A position the lists below do not know, for anything but a binding's value
    /// used bare, which escapes there. Temporary: the soundness PR takes every
    /// such position as an escape.
    legacy_unknown: Fate,
    /// Whether a binding's value used bare is judged by its direct parent alone,
    /// with no climb through wrappers: `(v).x` and `v!.x` escape, and `o[v]`, where
    /// it is the key, is judged as a member read off it. Temporary: the soundness
    /// PR climbs through wrappers from a bare value as from anything else.
    legacy_bare_parent: bool,
}

/// How a binding declared to hold a value, `const v = value`, is followed.
#[derive(Copy, Clone, Debug)]
pub(crate) enum Aliases {
    /// A local `const` that holds the value, or takes it apart one level, is
    /// followed to its own uses here, and must leave it in place for the
    /// declaration to. So is a `let` that nothing reassigns.
    Local,
    /// Followed by the caller's adapter, which credits each use to the
    /// declaration it is written in.
    Credited(Credit),
}

/// Follows the alias the declarator above `value` declares, for [`Aliases::Credited`]:
/// every use it is put to, each with where it is written, where `own` is where the
/// declaration itself is and `depth` how many aliases were followed to reach it.
///
/// This is the module's one seam. Which declaration a use is credited to is the
/// shared-state rule's bookkeeping, so `shared` passes an adapter in here rather
/// than this module reaching for it.
pub(crate) type Credit = for<'a> fn(&Ctx<'a>, NodeId, u32, usize) -> Vec<(u32, Fate)>;

impl Policy {
    /// What a Zustand store's `getState` or a collection's read method hands out,
    /// and what a listener or callback is called with: it may be used as a value
    /// where it stands, and a local `const` or `let` that holds it, or takes it
    /// apart one level, is followed to its own uses. Nothing else is known to
    /// leave it be.
    pub(crate) const HANDED_OUT: Policy = Policy {
        arithmetic: Fate::InPlace,
        test: Fate::InPlace,
        discard: Fate::InPlace,
        returns: None,
        aliases: Aliases::Local,
        legacy_unknown: Fate::Escapes,
        legacy_bare_parent: false,
    };

    /// A binding read as one value, such as a reassigned `let` or a call's result,
    /// and what is read off it. The value itself escapes wherever it is not
    /// checked, called, constructed, rendered where it stands or read from, so
    /// arithmetic on it, testing it and dropping it count as escapes. A local
    /// `const` or `let` that holds it, or takes it apart one level, is followed to
    /// its own uses. What is read off it is in place anywhere this does not know.
    pub(crate) const WHOLE: Policy = Policy {
        arithmetic: Fate::Escapes,
        test: Fate::Escapes,
        discard: Fate::Escapes,
        returns: None,
        aliases: Aliases::Local,
        legacy_unknown: Fate::InPlace,
        legacy_bare_parent: true,
    };

    /// A property read off an object shared property by property or read only for
    /// its members, or an element read off a collection the file makes. Anything
    /// this does not know leaves it in place. A binding declared to hold it is
    /// followed by `credit`.
    pub(crate) const fn parts(credit: Credit) -> Policy {
        Policy {
            arithmetic: Fate::InPlace,
            test: Fate::InPlace,
            discard: Fate::InPlace,
            returns: None,
            aliases: Aliases::Credited(credit),
            legacy_unknown: Fate::InPlace,
            legacy_bare_parent: false,
        }
    }

    /// This policy, with the expression body of the arrow at `arrow` in place.
    fn returning(self, arrow: Option<OxcSpan>) -> Policy {
        Policy {
            returns: arrow,
            ..self
        }
    }
}

/// Where a value lands once it has climbed out through what leaves it as it was.
struct Landing {
    /// The outermost node standing for the value, or for what was read off it.
    top: NodeId,
    /// That node's span, by which its parent tells which of its parts it is.
    span: OxcSpan,
    /// What that node holds.
    held: Held,
    /// How many member steps the climb took. Zero where the value itself lands.
    steps: usize,
}

/// Every use `value` is put to, each with where it is written and its fate, where
/// `value` holds `held`. `depth` is how many aliases were followed to reach it.
pub(crate) fn uses(
    ctx: &Ctx<'_>,
    value: NodeId,
    held: Held,
    policy: &Policy,
    depth: usize,
) -> Vec<(u32, Fate)> {
    let nodes = ctx.semantic.nodes();
    let at = nodes.get_node(value).kind().span().start;
    if held == Held::Primitive {
        return vec![(at, Fate::InPlace)];
    }
    let landing = climb(nodes, value, held, policy);
    if let Aliases::Credited(credit) = policy.aliases
        && let AstKind::VariableDeclarator(declarator) = nodes.parent_kind(landing.top)
        && declarator
            .init
            .as_ref()
            .is_some_and(|init| init.span() == landing.span)
    {
        return credit(ctx, landing.top, at, depth);
    }
    vec![(at, fate(ctx, &landing, policy, depth))]
}

/// Whether the member expression at `member` is called, or used as a tag, as it
/// stands: a method called on the value it was read off, which gets that value as
/// `this`.
pub(crate) fn called_as_method(nodes: &AstNodes<'_>, member: NodeId) -> bool {
    let mut current = member;
    loop {
        let (outer, span) = through_wrappers(nodes, current);
        match nodes.parent_kind(outer) {
            AstKind::ChainExpression(_) => current = nodes.parent_id(outer),
            AstKind::CallExpression(call) => return call.callee.span() == span,
            AstKind::TaggedTemplateExpression(tagged) => return tagged.tag.span() == span,
            _ => return false,
        }
    }
}

/// Whether every use `value` is put to leaves it in place.
pub(crate) fn stays(
    ctx: &Ctx<'_>,
    value: NodeId,
    held: Held,
    policy: &Policy,
    depth: usize,
) -> bool {
    uses(ctx, value, held, policy, depth)
        .iter()
        .all(|(_, fate)| fate.in_place())
}

/// Climbs from `value` out through wrappers, member reads off it, optional
/// chaining and the operators that yield an operand as they found it.
fn climb(nodes: &AstNodes<'_>, value: NodeId, mut held: Held, policy: &Policy) -> Landing {
    let mut current = value;
    let mut steps = 0;
    if held == Held::Whole && policy.legacy_bare_parent {
        match nodes.parent_kind(value) {
            AstKind::StaticMemberExpression(_) | AstKind::ComputedMemberExpression(_) => {
                current = nodes.parent_id(value);
                held = held.read_off();
                steps = 1;
            }
            _ => {
                return Landing {
                    top: value,
                    span: nodes.get_node(value).kind().span(),
                    held,
                    steps,
                };
            }
        }
    }
    loop {
        let (outer, span) = through_wrappers(nodes, current);
        match nodes.parent_kind(outer) {
            AstKind::StaticMemberExpression(next) if next.object.span() == span => {
                held = held.read_off();
                steps += 1;
            }
            AstKind::ComputedMemberExpression(next) if next.object.span() == span => {
                held = held.read_off();
                steps += 1;
            }
            AstKind::ChainExpression(_) => {}
            // What these yield is the operand as it was, so where it goes the
            // operand goes: both operands of `||` and `??`, the right of `&&`, the
            // branches of `?:` and the last expression of a comma.
            AstKind::LogicalExpression(logical)
                if logical.operator != LogicalOperator::And || logical.right.span() == span => {}
            AstKind::ConditionalExpression(conditional) if conditional.test.span() != span => {}
            AstKind::SequenceExpression(sequence)
                if sequence.expressions.last().map(GetSpan::span) == Some(span) => {}
            _ => {
                return Landing {
                    top: outer,
                    span,
                    held,
                    steps,
                };
            }
        }
        current = nodes.parent_id(outer);
    }
}

/// What the position a value landed in does to it.
fn fate(ctx: &Ctx<'_>, landing: &Landing, policy: &Policy, depth: usize) -> Fate {
    let nodes = ctx.semantic.nodes();
    let Landing {
        top,
        span,
        held,
        steps,
    } = *landing;
    let parent = nodes.parent_id(top);
    // A binding's value used bare, as opposed to anything read off it or handed out.
    let bare = held == Held::Whole;
    // A knob of the policy speaks for the value itself. Anything read off it is a
    // part, which these positions leave in place, whether it is read off here or
    // held by an alias.
    let own = |fate: Fate| {
        if steps == 0 && bare {
            fate
        } else {
            Fate::InPlace
        }
    };
    match nodes.parent_kind(top) {
        // The state itself is never rendered in place, since it holds the actions.
        AstKind::JSXExpressionContainer(_) => {
            if held != Held::State && rendered_in_place(nodes, parent) {
                Fate::InPlace
            } else {
                Fate::Escapes
            }
        }
        AstKind::UnaryExpression(unary) => match unary.operator {
            // `typeof v` reads nothing that can be written back.
            UnaryOperator::Typeof => Fate::InPlace,
            UnaryOperator::Delete => Fate::Writes,
            UnaryOperator::LogicalNot => own(policy.test),
            UnaryOperator::Void => own(policy.discard),
            UnaryOperator::UnaryNegation | UnaryOperator::UnaryPlus | UnaryOperator::BitwiseNot => {
                own(policy.arithmetic)
            }
        },
        // `k in v`, `v instanceof C` and `v === o` check the value, on either side,
        // and yield a boolean that holds no reference to it. What they can run, a
        // proxy's `has` trap, `Symbol.hasInstance`, `valueOf` or `toString`, is
        // taken to run nothing, as it is for property reads and pure calls.
        AstKind::BinaryExpression(binary) => {
            if matches!(
                binary.operator,
                BinaryOperator::In
                    | BinaryOperator::Instanceof
                    | BinaryOperator::Equality
                    | BinaryOperator::Inequality
                    | BinaryOperator::StrictEquality
                    | BinaryOperator::StrictInequality
                    | BinaryOperator::LessThan
                    | BinaryOperator::LessEqualThan
                    | BinaryOperator::GreaterThan
                    | BinaryOperator::GreaterEqualThan
            ) {
                Fate::InPlace
            } else {
                own(policy.arithmetic)
            }
        }
        AstKind::IfStatement(test) if test.test.span() == span => own(policy.test),
        AstKind::ConditionalExpression(test) if test.test.span() == span => own(policy.test),
        AstKind::WhileStatement(test) if test.test.span() == span => own(policy.test),
        AstKind::DoWhileStatement(test) if test.test.span() == span => own(policy.test),
        AstKind::ForStatement(test)
            if test.test.as_ref().is_some_and(|test| test.span() == span) =>
        {
            own(policy.test)
        }
        // A `switch` compares its discriminant with each case's test by `===`,
        // which checks both as a comparison written out does.
        AstKind::SwitchStatement(switch) if switch.discriminant.span() == span => Fate::InPlace,
        AstKind::SwitchCase(case) if case.test.as_ref().is_some_and(|test| test.span() == span) => {
            Fate::InPlace
        }
        // A computed key is converted to a property key, which a `toString` it may
        // run is taken to leave as it was, as a comparison's is.
        AstKind::ComputedMemberExpression(member) if member.expression.span() == span => {
            Fate::InPlace
        }
        AstKind::ExpressionStatement(_) => own(policy.discard),
        // A template's values go to its tag, where it has one.
        AstKind::TemplateLiteral(_)
            if !matches!(
                nodes.parent_kind(parent),
                AstKind::TaggedTemplateExpression(_)
            ) =>
        {
            own(policy.arithmetic)
        }
        // An arrow's expression body is what it returns.
        AstKind::ArrowFunctionExpression(arrow) if policy.returns == Some(arrow.span) => {
            Fate::InPlace
        }
        // Anywhere else, what is returned is handed to the caller, which is free to
        // change it. A number read off a part may be anything else to a caller
        // that does not know what the part is.
        AstKind::ArrowFunctionExpression(_) | AstKind::ReturnStatement(_) => Fate::Escapes,
        // A literal holds what it is built from, and goes wherever it is taken.
        AstKind::ArrayExpression(_) | AstKind::ObjectProperty(_) => Fate::Escapes,
        // A credited alias never gets here: `uses` hands it to the adapter.
        AstKind::VariableDeclarator(declarator)
            if matches!(policy.aliases, Aliases::Local)
                && declarator
                    .init
                    .as_ref()
                    .is_some_and(|init| init.span() == span) =>
        {
            // A `let` is followed as a `const` is: a reassignment is a write of the
            // binding, which `bindings_stay` does not take as leaving it in place.
            let declaration = nodes.parent_id(parent);
            let local = matches!(
                nodes.kind(declaration),
                AstKind::VariableDeclaration(variable)
                    if matches!(
                        variable.kind,
                        VariableDeclarationKind::Const | VariableDeclarationKind::Let
                    )
            ) && !matches!(
                nodes.parent_kind(declaration),
                AstKind::ExportDeclaration(_)
            );
            if local && bindings_stay(ctx, &declarator.id, held, policy, depth) {
                Fate::InPlace
            } else {
                Fate::Escapes
            }
        }
        // Calling or constructing a binding does not rebind the name. A helper that
        // mutates itself is not covered, which is the assumption every bundler
        // makes. A method called on what the value was read off gets that as
        // `this`, and what was handed out may be an action.
        AstKind::CallExpression(call) if call.callee.span() == span => {
            if steps > 0 {
                Fate::Writes
            } else if bare {
                Fate::InPlace
            } else {
                Fate::Escapes
            }
        }
        AstKind::NewExpression(new) if bare && new.callee.span() == span => Fate::InPlace,
        // `<S />` and `<S>...</S>`, whose closing tag names it again. Rendering a
        // component passes props to the component, which is its own declaration.
        //
        // `<S.Provider value={...}>` is deliberately not here. Naming a member of a
        // binding as an element is a call on that member, and it is how a React
        // context is written to: the provider puts a value in, every consumer of the
        // same context reads it out. That is a channel between two declarations
        // however little the syntax looks like one.
        AstKind::JSXOpeningElement(_) | AstKind::JSXClosingElement(_) if bare => Fate::InPlace,
        // `#x in v` asks of a private field what `in` asks of a property.
        AstKind::PrivateInExpression(_) => Fate::InPlace,
        AstKind::TaggedTemplateExpression(tagged) if tagged.tag.span() == span => {
            if steps > 0 {
                Fate::Writes
            } else {
                Fate::Escapes
            }
        }
        AstKind::AssignmentExpression(assignment) => {
            if assignment.left.span() == span {
                Fate::Writes
            } else {
                Fate::Escapes
            }
        }
        AstKind::ForInStatement(statement) if statement.left.span() == span => Fate::Writes,
        AstKind::ForOfStatement(statement) if statement.left.span() == span => Fate::Writes,
        AstKind::UpdateExpression(_)
        | AstKind::AssignmentTargetPropertyIdentifier(_)
        | AstKind::AssignmentTargetPropertyProperty(_)
        | AstKind::ArrayAssignmentTarget(_)
        | AstKind::AssignmentTargetRest(_)
        | AstKind::AssignmentTargetWithDefault(_)
        | AstKind::JSXOpeningElement(_)
        | AstKind::JSXClosingElement(_) => Fate::Writes,
        AstKind::CallExpression(_)
        | AstKind::NewExpression(_)
        | AstKind::SpreadElement(_)
        | AstKind::ForInStatement(_)
        | AstKind::ForOfStatement(_) => Fate::Escapes,
        _ if bare => Fate::Escapes,
        _ => policy.legacy_unknown,
    }
}

/// Whether every binding a pattern declares is only put to uses that leave it in
/// place, where what the pattern takes apart holds `held`. Taking apart anything
/// past one level, or with a rest element or a default, is not followed, and nor
/// are aliases past [`ALIAS_DEPTH`].
fn bindings_stay(
    ctx: &Ctx<'_>,
    pattern: &BindingPattern<'_>,
    held: Held,
    policy: &Policy,
    depth: usize,
) -> bool {
    if depth >= ALIAS_DEPTH {
        return false;
    }
    match pattern {
        BindingPattern::BindingIdentifier(identifier) => {
            let Some(symbol) = identifier.symbol_id.get() else {
                return false;
            };
            let scoping = ctx.semantic.scoping();
            scoping.get_resolved_reference_ids(symbol).iter().all(|id| {
                let reference = scoping.get_reference(*id);
                !reference.is_write() && stays(ctx, reference.node_id(), held, policy, depth + 1)
            })
        }
        BindingPattern::ObjectPattern(object) => {
            object.rest.is_none()
                && object.properties.iter().all(|property| {
                    matches!(property.value, BindingPattern::BindingIdentifier(_))
                        && bindings_stay(ctx, &property.value, held.read_off(), policy, depth)
                })
        }
        _ => false,
    }
}

/// Whether a function only puts what it is called with to uses that leave it in
/// place, where each argument holds `held`. A rest parameter or a default is not
/// followed. Where `returns` is the function's own span, what it returns is in
/// place too, for a callback whose caller does no more with what it returns than
/// with what it hands out.
fn params_stay(
    ctx: &Ctx<'_>,
    params: &FormalParameters<'_>,
    held: Held,
    returns: Option<OxcSpan>,
) -> bool {
    let policy = Policy::HANDED_OUT.returning(returns);
    params.rest.is_none()
        && params.items.iter().all(|param| {
            param.initializer.is_none() && bindings_stay(ctx, &param.pattern, held, &policy, 0)
        })
}

/// How a method called on a value is judged.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) enum CallRule {
    /// A collection the file makes, whose built-in read methods are known.
    Collection(Collection),
    /// A value some factory's rule may have made, whose rule declares which of its
    /// methods read it and how each is written; see [`read_forms`].
    Factory,
}

/// The method this reference is called with directly, `value.name(…)`, with what
/// calling it does to the value by `rule`, or `None` where it is no such call.
///
/// For a collection, a callback the method calls with what the collection holds
/// must be an arrow written out in place that does nothing with what it is given
/// but use it in place, as what the method hands back must be. What else the
/// callback's body does is judged where it is written, so one that writes the
/// collection through its own binding is a write there. A callback passed by name
/// is handed the elements to do with as it likes, and nothing links it to the
/// collection but this call, so it escapes. So does a `function`, whose
/// `arguments` also hold what it is called with. What hands back a primitive is a
/// read however that is used.
///
/// For a factory, the call must be written the way each form its rule declares for
/// the name says: `store.getState().count` and `store.subscribe((state) => …)` for
/// a Zustand store. Which factory made the value, if any, is not known here, so
/// this only picks out the calls the graph may treat as reads. Calling what the
/// state holds, `store.getState().inc()`, calls an action that sets the store, and
/// handing the state on lets other code do the same.
pub(crate) fn method_fate<'a>(
    ctx: &Ctx<'a>,
    reference: NodeId,
    rule: CallRule,
) -> Option<(&'a str, Fate)> {
    let (name, call_id, call) = method_call(ctx.semantic.nodes(), reference)?;
    let reads = match rule {
        CallRule::Collection(collection) => {
            let Some((hands, callback)) = collection.read_method(name) else {
                return Some((name, Fate::Writes));
            };
            let callback_reads = match (callback, call.arguments.first()) {
                (Callback::Never, _) | (Callback::Optional, None) => true,
                (
                    Callback::Required | Callback::Optional,
                    Some(Argument::ArrowFunctionExpression(arrow)),
                ) => params_stay(ctx, &arrow.params, Held::Stored, Some(arrow.span)),
                _ => false,
            };
            callback_reads && stays(ctx, call_id, hands, &Policy::HANDED_OUT, 0)
        }
        CallRule::Factory => {
            let mut forms = read_forms(name).peekable();
            if forms.peek().is_none() {
                return Some((name, Fate::Writes));
            }
            forms.all(|form| match form {
                Reads::State(_) => stays(ctx, call_id, Held::State, &Policy::HANDED_OUT, 0),
                Reads::Listener(_) => listener_stays(ctx, call),
            })
        }
    };
    Some((name, if reads { Fate::InPlace } else { Fate::Escapes }))
}

/// Whether a call is given a single listener, written out in place, that does
/// nothing with the state it is called with but use it in place. What else its
/// body does is judged where it is written, so a listener that sets the store
/// through the store's own binding is a write there.
fn listener_stays(ctx: &Ctx<'_>, call: &CallExpression<'_>) -> bool {
    let [listener] = call.arguments.as_slice() else {
        return false;
    };
    let params = match listener {
        Argument::ArrowFunctionExpression(function) => &function.params,
        Argument::FunctionExpression(function) => &function.params,
        _ => return false,
    };
    params_stay(ctx, params, Held::State, None)
}

/// The method this reference is called with directly, `value.name(…)`, with the
/// call's node and the call.
fn method_call<'a>(
    nodes: &AstNodes<'a>,
    node_id: NodeId,
) -> Option<(&'a str, NodeId, &'a CallExpression<'a>)> {
    let (object, span) = through_wrappers(nodes, node_id);
    let AstKind::StaticMemberExpression(member) = nodes.parent_kind(object) else {
        return None;
    };
    if member.object.span() != span {
        return None;
    }
    let (callee, span) = through_wrappers(nodes, nodes.parent_id(object));
    let AstKind::CallExpression(call) = nodes.parent_kind(callee) else {
        return None;
    };
    if call.callee.span() != span {
        return None;
    }
    Some((member.property.name.as_str(), nodes.parent_id(callee), call))
}

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
    use oxc_span::{GetSpan, SourceType};

    use super::{Fate, Held, Policy};
    use crate::module::parse::Ctx;
    use crate::module::shared::{self, Collection, Mode};

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
        "while (@) go();"                            => [E,  I,  I,  I,  I,  I,  I,  I ];
        "do go(); while (@);"                        => [E,  I,  I,  I,  I,  I,  I,  I ];
        "for (; @; ) go();"                          => [E,  I,  I,  I,  I,  I,  I,  I ];
        "switch (@) {}"                              => [I,  I,  I,  I,  I,  I,  I,  I ];
        "switch (o) { case @: }"                     => [I,  I,  I,  I,  I,  I,  I,  I ];
        "class K { #x; m() { if (#x in @) go(); } }" => [I,  I,  I,  I,  I,  I,  I,  I ];
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
        "o[@];"                                      => [I,  I,  I,  I,  I,  I,  I,  I ];
        "o[@] = 1;"                                  => [W,  I,  I,  I,  I,  I,  I,  I ];
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
        "return @;"                                  => [E,  E,  E,  E,  E,  E,  E,  E ];
        "return @.x;"                                => [E,  E,  E,  E,  E,  E,  E,  E ];
        "return @.length;"                           => [E,  E,  E,  E,  E,  E,  E,  E ];
        "return @.x === 1;"                          => [I,  I,  I,  I,  I,  I,  I,  I ];
        "const g = () => @;"                         => [E,  E,  E,  E,  E,  E,  E,  E ];
        "const g = () => @.x;"                       => [E,  E,  E,  E,  E,  E,  E,  E ];
        "throw @;"                                   => [E,  U,  U,  U,  U,  E,  E,  E ];
        "await @;"                                   => [E,  U,  U,  U,  U,  E,  E,  E ];
        "yield @;"                                   => [E,  U,  U,  U,  U,  E,  E,  E ];
        // Passed through an operator that yields one of its operands.
        "f(o && @);"                                 => [E,  E,  E,  E,  E,  E,  E,  E ];
        "f(o || @);"                                 => [E,  E,  E,  E,  E,  E,  E,  E ];
        "f(o ?? @);"                                 => [E,  E,  E,  E,  E,  E,  E,  E ];
        "f(c ? @ : d);"                              => [E,  E,  E,  E,  E,  E,  E,  E ];
        "f((0, @));"                                 => [E,  E,  E,  E,  E,  E,  E,  E ];
        "return c ? @ : d;"                          => [E,  E,  E,  E,  E,  E,  E,  E ];
        "return o && @;"                             => [E,  E,  E,  E,  E,  E,  E,  E ];
        "(o || @) === 1;"                            => [E,  I,  I,  I,  I,  I,  I,  I ];
        "return <li>{o && @}</li>;"                  => [E,  I,  I,  I,  I,  I,  E,  I ];
        "return <Foo>{o && @}</Foo>;"                => [E,  E,  E,  E,  E,  E,  E,  E ];
        "return <Foo>{c ? @ : d}</Foo>;"             => [E,  E,  E,  E,  E,  E,  E,  E ];
        "return <Foo>{o ?? @}</Foo>;"                => [E,  E,  E,  E,  E,  E,  E,  E ];
        // Held in a literal.
        "f({ a: @ });"                               => [E,  E,  E,  E,  E,  E,  E,  E ];
        "f([@]);"                                    => [E,  E,  E,  E,  E,  E,  E,  E ];
        "return { a: @ };"                           => [E,  E,  E,  E,  E,  E,  E,  E ];
        "return [@];"                                => [E,  E,  E,  E,  E,  E,  E,  E ];
        // Held in a binding, or a class field or default of one.
        "const v = @; f(v);"                         => [E,  E,  E,  E,  E,  E,  E,  E ];
        "const v = @; return v;"                     => [E,  E,  E,  E,  E,  E,  E,  E ];
        "const v = @; v.x;"                          => [I,  I,  I,  I,  I,  I,  I,  I ];
        "const v = @; v.x = 1;"                      => [E,  E,  E,  E,  E,  E,  E,  E ];
        "const v = @; v.x();"                        => [E,  E,  E,  E,  E,  E,  E,  E ];
        "const { x } = @; f(x);"                     => [E,  E,  E,  E,  E,  E,  E,  E ];
        "const { x } = @; x + 1;"                    => [I,  I,  E,  E,  E,  I,  I,  I ];
        "const { x } = @; x.y = 1;"                  => [E,  E,  E,  E,  E,  E,  E,  E ];
        "let v = @; v.x;"                            => [I,  I,  I,  I,  I,  I,  I,  I ];
        "let v = @; v = o; v.x;"                     => [E,  E,  E,  E,  E,  E,  E,  E ];
        "export const v = @;"                        => [E,  E,  E,  E,  E,  E,  E,  E ];
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

    /// What the state a store hands out, standing in for `S` in `body`, meets.
    fn state_fate(body: &str) -> Fate {
        let source = format!("let S: any;\nfunction a() {{\n{body}\n}}\n");
        let allocator = Allocator::default();
        let parsed = Parser::new(&allocator, &source, SourceType::tsx()).parse();
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
        let [(_, fate)] = super::uses(&ctx, node, Held::State, &Policy::HANDED_OUT, 0)[..] else {
            panic!("expected one use: {source}");
        };
        fate
    }

    #[test]
    fn an_operand_an_operator_yields_goes_where_the_operator_puts_it() {
        for body in [
            "(o || S) === 1;",
            "(o ?? S) === 1;",
            "(S ?? o) === 1;",
            "(o && S) === 1;",
            "(c ? S : d) === 1;",
            "(0, S) === 1;",
        ] {
            assert_eq!(state_fate(body), Fate::InPlace, "{body}");
        }
        for body in [
            "f(o || S);",
            "f(S ?? o);",
            "f(o && S);",
            "f(c ? S : d);",
            "f((0, S));",
            "return c ? o : (0, S);",
        ] {
            assert_eq!(state_fate(body), Fate::Escapes, "{body}");
        }
        // The left of `&&` is yielded only where it is falsy, and a comma yields its
        // last expression alone, so neither is followed.
        for body in ["(S && o) === 1;", "(S, 0) === 1;"] {
            assert_eq!(state_fate(body), Fate::Escapes, "{body}");
        }
    }

    /// How a reference touches a binding read as one value, for the tests below.
    #[derive(Copy, Clone, Debug, PartialEq, Eq)]
    enum Use {
        /// Cannot change what the binding holds.
        Read,
        /// Could, or we cannot tell.
        Mutate,
    }

    /// How every reference to `S` in `body` is read, in source order.
    fn uses_of_s(body: &str) -> Vec<Use> {
        let source = format!("const S = make();\n{body}\n");
        let allocator = Allocator::default();
        let parsed = Parser::new(&allocator, &source, SourceType::tsx()).parse();
        let semantic = SemanticBuilder::new()
            .with_build_nodes(true)
            .build(&parsed.program)
            .semantic;
        let ctx = Ctx {
            semantic: &semantic,
            statements: Vec::new(),
        };
        let scoping = semantic.scoping();

        let mut found: Vec<(u32, Use)> = Vec::new();
        for symbol_id in scoping.symbol_ids() {
            if scoping.symbol_name(symbol_id) != "S" {
                continue;
            }
            for reference_id in scoping.get_resolved_reference_ids(symbol_id) {
                let node_id = scoping.get_reference(*reference_id).node_id();
                let start = semantic.nodes().get_node(node_id).kind().span().start;
                let how = if super::stays(&ctx, node_id, Held::Whole, &Policy::WHOLE, 0) {
                    Use::Read
                } else {
                    Use::Mutate
                };
                found.push((start, how));
            }
        }
        found.sort_by_key(|(start, _)| *start);
        found.into_iter().map(|(_, how)| how).collect()
    }

    #[test]
    fn using_a_binding_without_touching_it_is_a_read() {
        assert_eq!(uses_of_s("export const a = () => S(1);"), [Use::Read]);
        assert_eq!(uses_of_s("export const a = () => new S(2);"), [Use::Read]);
        assert_eq!(uses_of_s("export const a = () => typeof S;"), [Use::Read]);
        assert_eq!(uses_of_s("export const a = () => <S />;"), [Use::Read]);
        // A closing tag names the component a second time.
        assert_eq!(
            uses_of_s("export const a = () => <S>x</S>;"),
            [Use::Read, Use::Read]
        );
    }

    #[test]
    fn an_element_naming_a_member_is_a_call_on_it() {
        // `<Ctx.Provider value={v}>` writes v into the context every consumer of
        // `Ctx` reads. Compound components such as `<Menu.Item />` only read, and
        // are widened along with it.
        assert_eq!(
            uses_of_s("export const a = () => <S.Item />;"),
            [Use::Mutate]
        );
        assert_eq!(
            uses_of_s("export const a = () => <S.Provider value={1}>x</S.Provider>;"),
            [Use::Mutate, Use::Mutate]
        );
    }

    #[test]
    fn a_property_read_that_stays_in_the_expression_is_a_read() {
        assert_eq!(uses_of_s("export const a = () => { S.x; };"), [Use::Read]);
        assert_eq!(
            uses_of_s("export const a = () => { S[\"y\"]; };"),
            [Use::Read]
        );
        assert_eq!(uses_of_s("export const a = () => S.x + 1;"), [Use::Read]);
    }

    #[test]
    fn checking_a_value_is_a_read() {
        for check in [
            "k in S",
            "S in o",
            "S instanceof C",
            "v instanceof S",
            "S == o",
            "o != S",
            "S === o",
            "o !== S",
            "S < 1",
            "1 <= S",
            "S > 1",
            "1 >= S",
        ] {
            let body = format!("export const a = () => {{ if ({check}) go(); }};");
            assert_eq!(uses_of_s(&body), [Use::Read], "{check}");
        }
        // With both operands the binding, each is read.
        assert_eq!(
            uses_of_s("export const a = () => { if (S === S) go(); };"),
            [Use::Read, Use::Read]
        );
        // `#x in S` asks the same of a private field.
        assert_eq!(
            uses_of_s("class K { #x; static t = () => #x in S; }"),
            [Use::Read]
        );
    }

    #[test]
    fn arithmetic_on_a_value_is_still_counted_as_a_write() {
        // Only the checks above are taken out of the catch-all. Arithmetic can run
        // `valueOf` no more than a comparison can, but it stays in it.
        assert_eq!(uses_of_s("export const a = () => S + 1;"), [Use::Mutate]);
        assert_eq!(uses_of_s("export const a = () => S | 1;"), [Use::Mutate]);
        // A check does not make passing the value on any less of a hand-off.
        assert_eq!(
            uses_of_s("export const a = () => { if (k in S) other(S); };"),
            [Use::Read, Use::Mutate]
        );
    }

    #[test]
    fn writing_through_a_binding_is_a_mutation() {
        assert_eq!(
            uses_of_s("export const a = () => { S.x = 1 };"),
            [Use::Mutate]
        );
        assert_eq!(uses_of_s("export const a = () => S++;"), [Use::Mutate]);
        assert_eq!(
            uses_of_s("export const a = () => { delete S.x };"),
            [Use::Mutate]
        );
    }

    #[test]
    fn writing_or_calling_anywhere_down_a_member_chain_is_a_mutation() {
        for body in [
            "{ S.a.b = 1; }",
            "{ S.a.b += 1; }",
            "{ S.a[k]++; }",
            "{ delete S.a.b; }",
            "{ S.a.b.c = 1; }",
            "{ [S.a.b] = o; }",
            "{ ({ x: S.a.b } = o); }",
            "{ for (S.a.b of o); }",
            "{ (S.a as any).b = 1; }",
            "{ (S.a).b = 1; }",
            // A method called on what the value holds can change it.
            "S.items.push(1)",
            "S.a.b.c()",
            "S.a?.b.c()",
            "(S.a).b()",
            "S.a.b`x`",
            // Handed to other code, however deep it was read.
            "other(S.a.b)",
            "[...S.a.b]",
        ] {
            let source = format!("export const a = () => {body};");
            assert_eq!(uses_of_s(&source), [Use::Mutate], "{body}");
        }
    }

    #[test]
    fn handing_the_value_to_a_component_is_a_mutation() {
        for body in [
            "<Foo value={S.items} />",
            "<Foo>{S.items}</Foo>",
            "<Foo value={S} />",
            "<Foo>{S}</Foo>",
            "<Foo.Bar>{S.a.b}</Foo.Bar>",
            // An element of the platform's own calls a handler it is handed.
            "<div onClick={S.handler} />",
            "<div title={S.name} />",
        ] {
            let source = format!("export const a = () => {body};");
            assert_eq!(uses_of_s(&source), [Use::Mutate], "{body}");
        }
    }

    #[test]
    fn rendering_the_value_in_place_is_a_read() {
        for body in [
            "<li>{S.count}</li>",
            "<li key={S.id} />",
            "<Foo key={S.id} />",
            "<>{S.a.b}</>",
            "<li>{S}</li>",
        ] {
            let source = format!("export const a = () => {body};");
            assert_eq!(uses_of_s(&source), [Use::Read], "{body}");
        }
    }

    #[test]
    fn a_deep_property_read_that_stays_in_the_expression_is_a_read() {
        for body in [
            "S.a.b",
            "S.items.length",
            "S.a.b + 1",
            "S?.a.b",
            "S.a[k]",
            "`${S.a.b}`",
            "(S.a as any).b",
            "{ if (S.a.b === 1) go(); }",
        ] {
            let source = format!("export const a = () => {{ {body}; }};");
            assert_eq!(uses_of_s(&source), [Use::Read], "{body}");
        }
    }

    #[test]
    fn handing_the_value_to_other_code_is_a_mutation() {
        // The callee is the reference in `S(1)`; here `S` is an argument, and the
        // function it lands in can do anything with it.
        assert_eq!(uses_of_s("export const a = () => other(S);"), [Use::Mutate]);
        // A method call is the ordinary way to mutate: `.push`, `.set`, `.add`.
        assert_eq!(
            uses_of_s("export const a = () => S.push(1);"),
            [Use::Mutate]
        );
        // A property read can escape the same way the binding itself can.
        assert_eq!(
            uses_of_s("export const a = () => other(S.x);"),
            [Use::Mutate]
        );
        assert_eq!(uses_of_s("export const a = () => [...S];"), [Use::Mutate]);
        // Returning the binding, or anything read off it, hands it to the caller.
        assert_eq!(uses_of_s("export const a = () => S;"), [Use::Mutate]);
        assert_eq!(uses_of_s("export const a = () => S.x;"), [Use::Mutate]);
        assert_eq!(
            uses_of_s("export const a = () => { return S.items.length; };"),
            [Use::Mutate]
        );
    }

    #[test]
    fn each_reference_is_read_on_its_own() {
        assert_eq!(
            uses_of_s("export const a = () => { other(S); return S.y === 1 };"),
            [Use::Mutate, Use::Read]
        );
    }

    #[test]
    fn a_property_read_beside_an_assignment_is_not_told_apart() {
        // `S.y` here only reads. Working out which side of the assignment a member
        // expression sits on would narrow this, and narrowing is the direction that
        // can be wrong, so both sides count as writes.
        assert_eq!(
            uses_of_s("export const a = () => { S.x = S.y };"),
            [Use::Mutate, Use::Mutate]
        );
    }
}
