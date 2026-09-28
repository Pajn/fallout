//! What runs when the module is evaluated.
//!
//! `ModuleInit(f)` is the node every importer of `f` depends on. It collects the
//! declarations whose values are computed at import time, either because a top-level
//! statement uses them or because their own initialiser may have side effects.

use oxc_ast::ast::*;
use oxc_ast_visit::{Visit, walk};
use oxc_semantic::{Scoping, SymbolFlags, SymbolId};
use oxc_span::GetSpan;

use ahash::{AHashMap, AHashSet};

use super::cjs;
use super::decls::{DeclDraft, ImportBinding};
use super::local_pure::LocalPure;
use super::parse::{Ctx, span_of};
use super::{Decl, DeclId, ImportTarget};
use crate::pure::PureList;

/// Where an imported binding in this file comes from, for deciding whether a call on
/// it is one the project has declared pure.
///
/// Keyed by the binding itself rather than by its name, since a local of the same
/// name inside a function or a class body is somebody else's value.
pub(crate) struct Origins<'s, 'a> {
    scoping: &'s Scoping,
    by_symbol: AHashMap<SymbolId, (&'a str, &'a str)>,
}

impl<'a> Origins<'_, 'a> {
    /// The import `identifier` reads, as `(module specifier, exported name)`, or
    /// `None` when it reads anything else.
    fn of(&self, identifier: &IdentifierReference<'_>) -> Option<(&'a str, &'a str)> {
        let reference = self.scoping.get_reference(identifier.reference_id.get()?);
        self.by_symbol.get(&reference.symbol_id()?).copied()
    }
}

/// Each imported binding as `symbol -> (module specifier, exported name)`, with
/// `default` and `*` standing for the two unnamed import forms.
pub(crate) fn origins<'s, 'a>(
    ctx: &Ctx<'s>,
    imports: &'a [ImportBinding],
    sources: &'a [String],
) -> Origins<'s, 'a> {
    let scoping = ctx.semantic.scoping();
    let mut by_symbol = AHashMap::default();
    for binding in imports {
        let Some(source) = sources.get(binding.reference.source as usize) else {
            continue;
        };
        // The binding the import statement made, which is what a callee must
        // resolve to for an entry to speak for it.
        let Some(symbol) = scoping
            .get_root_binding(binding.local.as_str().into())
            .filter(|&symbol| scoping.symbol_flags(symbol).is_import())
        else {
            continue;
        };
        let exported = match &binding.reference.target {
            ImportTarget::Named(name) | ImportTarget::Member { export: name, .. } => name.as_str(),
            ImportTarget::Namespace => "*",
        };
        by_symbol.insert(symbol, (source.as_str(), exported));
    }
    Origins { scoping, by_symbol }
}

/// Declarations module initialisation depends on.
pub(crate) fn collect(
    ctx: &Ctx<'_>,
    program: &Program<'_>,
    drafts: &[DeclDraft],
    decls: &[Decl],
    origins: &Origins<'_, '_>,
    pure: &PureList,
    conditional: &AHashSet<usize>,
) -> (Vec<DeclId>, Vec<DeclId>) {
    let local = LocalPure::infer(ctx, program);
    let rules = Rules {
        origins,
        pure,
        local: &local,
        readable: readable_from(ctx),
    };
    let mut init: Vec<DeclId> = Vec::new();
    let mut maybe: Vec<DeclId> = Vec::new();

    for (index, statement) in program.body.iter().enumerate() {
        let declares = drafts.iter().any(|draft| draft.statement == index);

        if !declares {
            // A top-level statement that declares nothing runs for its effect. Every
            // declaration it names is part of initialisation.
            for decl in referenced_decls(ctx, statement, drafts) {
                push_unique(&mut init, decl);
            }
            continue;
        }

        // An initialiser that may have side effects runs at import time whether or
        // not anyone reads the binding. One this module can clear is not asked about
        // again below.
        if !statement_has_impure_initialiser(statement, &rules) {
            continue;
        }

        // A call that may be a factory's runs nothing else if its arguments do not:
        // whether the call itself does is the graph's to say, once it knows the
        // callee. See [`super::factories`].
        if conditional.contains(&index)
            && let Some((_, call)) = super::factories::call_of(statement)
            && !call.arguments.iter().any(|argument| {
                argument
                    .as_expression()
                    .is_none_or(|argument| argument.check_impurity(&rules))
            })
        {
            for (id, draft) in drafts.iter().enumerate() {
                if draft.statement == index {
                    push_unique(&mut maybe, id as DeclId);
                }
            }
            continue;
        }

        for (id, draft) in drafts.iter().enumerate() {
            if draft.statement == index {
                push_unique(&mut init, id as DeclId);
            }
        }
    }

    // Initialisation reaches whatever those declarations reach, including an object
    // one of them reads a property of.
    let mut queue: Vec<DeclId> = init.clone();
    while let Some(current) = queue.pop() {
        let entry = &decls[current as usize];
        let objects = entry.member_refs.iter().map(|(object, _)| object);
        for &next in entry.refs.iter().chain(objects) {
            if !init.contains(&next) {
                init.push(next);
                queue.push(next);
            }
        }
    }

    // A conditional declaration an effectful one reads is part of initialisation
    // either way, so it may be in both lists.
    init.sort_unstable();
    (init, maybe)
}

/// Top-level declarations named anywhere inside `statement`.
fn referenced_decls(ctx: &Ctx<'_>, statement: &Statement<'_>, drafts: &[DeclDraft]) -> Vec<DeclId> {
    let scoping = ctx.semantic.scoping();
    let root = scoping.root_scope_id();
    let span = span_of(statement.span());

    let mut found = Vec::new();
    for symbol_id in scoping.symbol_ids() {
        if scoping.symbol_scope_id(symbol_id) != root {
            continue;
        }
        let name = scoping.symbol_name(symbol_id);
        let Some(decl) = drafts.iter().position(|d| d.name == name) else {
            continue;
        };

        for reference_id in scoping.get_resolved_reference_ids(symbol_id) {
            let node_id = scoping.get_reference(*reference_id).node_id();
            let at = span_of(ctx.semantic.nodes().get_node(node_id).kind().span());
            if span.contains(at.start) {
                push_unique(&mut found, decl as DeclId);
                break;
            }
        }
    }
    found
}

/// The conservative rule for this step: a call, a construction, an `await`, a tagged
/// template, or a write to anything but a binding the module owns is impure. Later
/// steps narrow this.
fn statement_has_impure_initialiser(statement: &Statement<'_>, rules: &Rules<'_, '_, '_>) -> bool {
    let declaration = match statement {
        Statement::ExportDeclaration(export) => Some(&export.declaration),
        Statement::ExportDefaultDeclaration(export) => {
            return export.declaration.check_impurity(rules);
        }
        // Only a statement that declares something reaches here, so an expression
        // statement is a CommonJS export: either the assignment that fills the table,
        // whose initialiser is the value it assigns, or a defined property, which
        // stores what it is handed without running any of it.
        Statement::ExpressionStatement(statement) => {
            return cjs::assigned_value(&statement.expression)
                .is_some_and(|value| value.check_impurity(rules));
        }
        statement => statement.as_declaration(),
    };

    let variable = match declaration {
        Some(Declaration::VariableDeclaration(variable)) => variable,
        Some(Declaration::ClassDeclaration(class)) => {
            return class.check_impurity(rules);
        }
        // Each member's value is computed when the enum is, and a `const enum` is
        // read the same way because type erasure keeps it.
        Some(Declaration::TSEnumDeclaration(enumeration)) => {
            return enumeration
                .body
                .members
                .iter()
                .filter_map(|member| member.initializer.as_ref())
                .any(|init| init.check_impurity(rules));
        }
        // A function declaration binds without running anything.
        _ => return false,
    };

    variable.declarations.iter().any(|declarator| {
        // `var _default = (exports.default = …)`, a compiler's `export default`.
        // Filling the table is the export, not an effect on anybody else, so what
        // decides whether this runs anything is the value alone.
        let init = declarator.init.as_ref().is_some_and(|init| {
            cjs::assigned_value(init)
                .unwrap_or(init)
                .check_impurity(rules)
        });
        // A pattern computes its defaults and its computed keys as it binds, so
        // those run with the initialiser. Taking the value apart is left to the
        // value, as reading a property is everywhere else.
        init || declarator.id.check_impurity(rules)
    })
}

/// What an initialiser is judged against: where each import comes from, what the
/// project has declared pure, what the local-helper proof has shown, and when each
/// lexical binding of the file can first be read.
struct Rules<'c, 'o, 'a> {
    origins: &'c Origins<'c, 'o>,
    pure: &'c PureList,
    local: &'c LocalPure<'c, 'a>,
    readable: AHashMap<SymbolId, u32>,
}

/// Each `let`, `const` and declared class of the file, and the offset at which its
/// declaration has run. Reading or writing one before then throws, and a throw on
/// load stops the module loading, and every importer with it.
///
/// That is the end of the whole declarator, as it is for the local-helper proof,
/// so a binding a pattern gives is taken as readable only once the pattern is
/// done; and the end of a class, whose own name its static members may still read
/// (see [`ImpureDetector::statics`]).
fn readable_from(ctx: &Ctx<'_>) -> AHashMap<SymbolId, u32> {
    let scoping = ctx.semantic.scoping();
    let nodes = ctx.semantic.nodes();
    scoping
        .symbol_ids()
        .filter(|&symbol| {
            let flags = scoping.symbol_flags(symbol);
            flags.intersects(SymbolFlags::BlockScopedVariable | SymbolFlags::Class)
                && !flags.is_ambient()
        })
        .map(|symbol| {
            let declaration = nodes.get_node(scoping.symbol_declaration(symbol));
            (symbol, declaration.kind().span().end)
        })
        .collect()
}

trait ImpurityCheck<'a> {
    fn check_impurity(&self, rules: &Rules<'_, '_, '_>) -> bool;
}

impl<'a> ImpurityCheck<'a> for Expression<'a> {
    fn check_impurity(&self, rules: &Rules<'_, '_, '_>) -> bool {
        let mut detector = ImpureDetector::new(rules);
        detector.visit_expression(self);
        detector.impure
    }
}

impl<'a> ImpurityCheck<'a> for BindingPattern<'a> {
    fn check_impurity(&self, rules: &Rules<'_, '_, '_>) -> bool {
        let mut detector = ImpureDetector::new(rules);
        detector.visit_binding_pattern(self);
        detector.impure
    }
}

impl<'a> ImpurityCheck<'a> for Class<'a> {
    fn check_impurity(&self, rules: &Rules<'_, '_, '_>) -> bool {
        let mut detector = ImpureDetector::new(rules);
        detector.visit_class(self);
        detector.impure
    }
}

impl<'a> ImpurityCheck<'a> for ExportDefaultDeclarationKind<'a> {
    fn check_impurity(&self, rules: &Rules<'_, '_, '_>) -> bool {
        match self {
            ExportDefaultDeclarationKind::ClassDeclaration(class) => class.check_impurity(rules),
            // `export default function f() {}` binds without running anything.
            ExportDefaultDeclarationKind::FunctionDeclaration(_)
            | ExportDefaultDeclarationKind::TSInterfaceDeclaration(_) => false,
            expression => {
                let mut detector = ImpureDetector::new(rules);
                if let Some(expression) = expression.as_expression() {
                    detector.visit_expression(expression);
                }
                detector.impure
            }
        }
    }
}

struct ImpureDetector<'c, 'o, 'a> {
    rules: &'c Rules<'c, 'o, 'a>,
    impure: bool,
    /// The declared classes whose bodies are being read, innermost last. Inside
    /// its own body a class's name is a constant.
    classes: Vec<SymbolId>,
    /// The classes whose static fields and blocks are being read. Those run once
    /// the class's own name is bound, so they may read it, while its `extends`
    /// expression and computed keys run before and may not.
    statics: Vec<SymbolId>,
}

impl<'c, 'o, 'a> ImpureDetector<'c, 'o, 'a> {
    fn new(rules: &'c Rules<'c, 'o, 'a>) -> Self {
        Self {
            rules,
            impure: false,
            classes: Vec::new(),
            statics: Vec::new(),
        }
    }

    /// Does this reference name a binding whose declaration has not run yet?
    ///
    /// Only what runs on load is read here, since a function body is skipped, so a
    /// reference written before its binding's declaration is one evaluated before
    /// it. The binding is found by symbol, so a local of the same name declared
    /// later does not count against an earlier binding, nor the other way round.
    fn too_early(&self, identifier: &IdentifierReference<'_>) -> bool {
        let scoping = self.rules.origins.scoping;
        let Some(reference) = identifier
            .reference_id
            .get()
            .map(|id| scoping.get_reference(id))
        else {
            return false;
        };
        let Some(symbol) = reference.symbol_id() else {
            return false;
        };
        // A type erases, and so never runs.
        if !reference.is_read() && !reference.is_write() {
            return false;
        }
        self.rules
            .readable
            .get(&symbol)
            .is_some_and(|&readable| identifier.span.start < readable)
            && !self.statics.contains(&symbol)
    }

    /// Does assigning `identifier` change nothing but a binding this module owns?
    ///
    /// That is a `let`, a `var`, a function or a class declared at the top of this
    /// file, which only this file can read. A name nothing declares, or one only a
    /// `declare` names, is a property of the global object, and assigning an import
    /// or a `const` throws, which stops the module loading. The binding is found by
    /// symbol, so a local of the same name is not mistaken for it.
    fn owns(&self, identifier: &IdentifierReference<'_>) -> bool {
        let scoping = self.rules.origins.scoping;
        let Some(symbol) = identifier
            .reference_id
            .get()
            .and_then(|reference| scoping.get_reference(reference).symbol_id())
        else {
            return false;
        };
        let flags = scoping.symbol_flags(symbol);
        scoping.symbol_scope_id(symbol) == scoping.root_scope_id()
            && flags.intersects(SymbolFlags::Variable | SymbolFlags::Function | SymbolFlags::Class)
            && !flags
                .intersects(SymbolFlags::ConstVariable | SymbolFlags::Import | SymbolFlags::Ambient)
            && !self.classes.contains(&symbol)
    }

    /// Is this callee free of side effects?
    ///
    /// `annotated` is the `/* @__PURE__ */` comment, which is the author of the call
    /// site speaking. The list is the project speaking about someone else's
    /// function, and is honoured only where the callee is reached from the import
    /// the entry names, so a local binding of the same name is not covered.
    fn callee_is_pure(&self, annotated: bool, callee: &Expression<'_>) -> bool {
        if annotated {
            return true;
        }
        let Some((root, members)) = callee_path(callee) else {
            return false;
        };
        let Some((source, exported)) = self.rules.origins.of(root) else {
            return false;
        };

        let mut path = Vec::with_capacity(members.len() + 1);
        path.push(exported);
        path.extend(members);
        self.rules.pure.contains(source, &path)
    }
}

impl<'a, 'c, 'o> Visit<'a> for ImpureDetector<'c, 'o, '_> {
    fn visit_call_expression(&mut self, expr: &CallExpression<'a>) {
        if !self.callee_is_pure(expr.pure, &expr.callee)
            && !self.rules.local.call(expr)
            && !self.rules.local.freezes(expr)
        {
            self.impure = true;
        }
        // A pure callee says nothing about its arguments, which still run.
        walk::walk_call_expression(self, expr);
    }

    fn visit_new_expression(&mut self, expr: &NewExpression<'a>) {
        if !self.callee_is_pure(expr.pure, &expr.callee) && !self.rules.local.constructs(expr) {
            self.impure = true;
        }
        walk::walk_new_expression(self, expr);
    }

    fn visit_await_expression(&mut self, expr: &AwaitExpression<'a>) {
        self.impure = true;
        walk::walk_await_expression(self, expr);
    }

    fn visit_tagged_template_expression(&mut self, expr: &TaggedTemplateExpression<'a>) {
        // A tagged template has no annotation field to read, so only the list can
        // clear it.
        if !self.callee_is_pure(false, &expr.tag) {
            self.impure = true;
        }
        walk::walk_tagged_template_expression(self, expr);
    }

    fn visit_assignment_expression(&mut self, expr: &AssignmentExpression<'a>) {
        let owned = match &expr.left {
            AssignmentTarget::AssignmentTargetIdentifier(identifier) => self.owns(identifier),
            _ => false,
        };
        if !owned {
            self.impure = true;
        }
        walk::walk_assignment_expression(self, expr);
    }

    // An update writes what it reads, and its target is as likely to be someone
    // else's as an assignment's is, so it counts wherever it points.
    fn visit_update_expression(&mut self, expr: &UpdateExpression<'a>) {
        self.impure = true;
        walk::walk_update_expression(self, expr);
    }

    // Deleting a property changes an object that anyone holding it can read.
    fn visit_unary_expression(&mut self, expr: &UnaryExpression<'a>) {
        if expr.operator == UnaryOperator::Delete {
            self.impure = true;
        }
        walk::walk_unary_expression(self, expr);
    }

    // A `let`, a `const` or a class read before its declaration has run throws, which
    // stops the module loading.
    fn visit_identifier_reference(&mut self, identifier: &IdentifierReference<'a>) {
        if self.too_early(identifier) {
            self.impure = true;
        }
    }

    // A function body does not run until it is called, so what it contains says
    // nothing about the initialiser's purity.
    fn visit_function(&mut self, _function: &Function<'a>, _flags: oxc_semantic::ScopeFlags) {}

    fn visit_arrow_function_expression(&mut self, _expr: &ArrowFunctionExpression<'a>) {}

    // Defining a class evaluates its heritage, every computed key, and its static
    // fields and blocks in order, so those run wherever the class is written. An
    // instance field waits for a construction and a method for a call, as a
    // function body does.
    fn visit_class(&mut self, class: &Class<'a>) {
        let symbol = class.id.as_ref().and_then(|id| id.symbol_id.get());
        self.classes.extend(symbol);
        if let Some(heritage) = &class.heritage {
            self.visit_expression(&heritage.expression);
        }
        for element in &class.body.body {
            let (key, computed, value, is_static) = match element {
                ClassElement::StaticBlock(block) => {
                    self.statics.extend(symbol);
                    self.visit_static_block(block);
                    if symbol.is_some() {
                        self.statics.pop();
                    }
                    continue;
                }
                ClassElement::MethodDefinition(method) => {
                    (&method.key, method.computed, None, false)
                }
                ClassElement::PropertyDefinition(property) => (
                    &property.key,
                    property.computed,
                    property.value.as_ref(),
                    property.r#static,
                ),
                ClassElement::AccessorProperty(property) => (
                    &property.key,
                    property.computed,
                    property.value.as_ref(),
                    property.r#static,
                ),
                ClassElement::TSIndexSignature(_) => continue,
            };
            if computed {
                self.visit_property_key(key);
            }
            if is_static && let Some(value) = value {
                self.statics.extend(symbol);
                self.visit_expression(value);
                if symbol.is_some() {
                    self.statics.pop();
                }
            }
        }
        if symbol.is_some() {
            self.classes.pop();
        }
    }

    // A static block is a body of statements, which can do more than an expression
    // can: throw, loop over an iterator, branch. So only a declaration or an
    // expression statement is judged by what it contains, and any other statement
    // counts as running something.
    fn visit_static_block(&mut self, block: &StaticBlock<'a>) {
        for statement in &block.body {
            let judged = match statement {
                Statement::VariableDeclaration(variable) => matches!(
                    variable.kind,
                    VariableDeclarationKind::Var
                        | VariableDeclarationKind::Let
                        | VariableDeclarationKind::Const
                ),
                Statement::ExpressionStatement(_)
                | Statement::FunctionDeclaration(_)
                | Statement::ClassDeclaration(_)
                | Statement::EmptyStatement(_) => true,
                _ => false,
            };
            if !judged {
                self.impure = true;
            }
            self.visit_statement(statement);
        }
    }
}

/// Splits `A.b.c` into its root identifier and the members read from it. `None` when
/// the callee is anything else, such as a call on a call or a computed member.
fn callee_path<'e, 'a>(
    callee: &'e Expression<'a>,
) -> Option<(&'e IdentifierReference<'a>, Vec<&'e str>)> {
    match callee {
        Expression::Identifier(ident) => Some((ident, Vec::new())),
        Expression::StaticMemberExpression(member) => {
            let (root, mut path) = callee_path(&member.object)?;
            path.push(member.property.name.as_str());
            Some((root, path))
        }
        _ => None,
    }
}

fn push_unique(list: &mut Vec<DeclId>, value: DeclId) {
    if !list.contains(&value) {
        list.push(value);
    }
}

#[cfg(test)]
mod tests {
    use crate::module::{FineModule, ModuleAnalysis, Reading, parse::analyse_source};
    use std::path::Path;

    fn module(source: &str) -> Box<FineModule> {
        let (ModuleAnalysis::Fine(module), _) =
            analyse_source(Path::new("lib.ts"), source, &Reading::default()).unwrap()
        else {
            panic!("expected fine module: {source}")
        };
        module
    }

    /// The declarations module initialisation reaches, by name.
    fn init(source: &str) -> Vec<String> {
        let module = module(source);
        module
            .init_decls
            .iter()
            .map(|&id| module.decls[id as usize].name.clone())
            .collect()
    }

    #[test]
    fn what_runs_when_a_class_is_defined_is_initialisation() {
        for (source, name) in [
            ("export class Widget { static id = register(1); }", "Widget"),
            ("class Widget { static { register(1); } }", "Widget"),
            ("export class Widget extends mixin(1) {}", "Widget"),
            ("export class Widget { [key()]() {} }", "Widget"),
            // The key of an instance field is computed with the class, even though
            // its value waits for a construction.
            ("export class Widget { [key()] = 1; }", "Widget"),
            (
                "export class Widget { static accessor id = register(1); }",
                "Widget",
            ),
            (
                "export default class { static id = register(1); }",
                "default",
            ),
            (
                "export default class Widget { static { register(1); } }",
                "default",
            ),
            (
                "export const Widget = class { static id = register(1); };",
                "Widget",
            ),
            ("const Widget = class extends mixin(1) {};", "Widget"),
            // A class declared inside a static block is defined when the block runs.
            (
                "export class Widget { static { class Inner { static id = register(1); } } }",
                "Widget",
            ),
        ] {
            assert!(init(source).contains(&name.to_string()), "{source}");
        }
    }

    #[test]
    fn a_static_block_that_may_stop_the_module_loading_is_initialisation() {
        for source in [
            // `new Error("…")` is pure, but throwing it stops the module loading.
            "export class Widget { static { throw new Error('no'); } }",
            // Iterating runs whatever the iterator does.
            "const list = [1]; export class Widget { static { for (const item of list) {} } }",
            "export class Widget { static { if (flag) { register(1); } } }",
        ] {
            assert!(init(source).contains(&"Widget".to_string()), "{source}");
        }
    }

    #[test]
    fn what_waits_for_a_construction_or_a_call_is_not_initialisation() {
        for source in [
            "export class Widget { id = register(1); }",
            "export class Widget { accessor id = register(1); }",
            "export class Widget { run() { register(1); } }",
            "export class Widget { static run() { register(1); } }",
            "export class Widget { static make = () => register(1); }",
            "export class Widget { constructor() { register(1); } }",
            "export const Widget = class { id = register(1); };",
            "export default class { run() { register(1); } }",
            "import { Base } from './base'; export class Widget extends Base {}",
            "export class Widget { static id = 1; static { const local = [Widget.id]; } }",
            "export class Widget { ['literal'] = register(1); }",
        ] {
            assert!(init(source).is_empty(), "{source}");
        }
    }

    #[test]
    fn a_static_member_built_by_a_pure_call_is_not_initialisation() {
        for source in [
            "import { memo } from 'react'; export class Widget { static View = memo(1); }",
            "import * as React from 'react'; export class Widget { static View = React.memo(1); }",
            "import { memo } from 'react'; export class Widget { static { memo(1); } }",
            "function make(x) { return { x }; } export class Widget { static id = make(1); }",
            "export class Widget { static id = /* @__PURE__ */ register(1); }",
            "import { memo } from 'react'; export const Widget = class { static View = memo(1); };",
        ] {
            assert!(init(source).is_empty(), "{source}");
        }
    }

    #[test]
    fn an_enum_member_computed_by_a_call_is_initialisation() {
        for source in [
            "export enum Size { Small = register(1) }",
            "enum Size { Small = 1, Large = register(2) }",
            // Type erasure keeps a `const enum`, as a compiler reading one file at a
            // time does, so it is read like any other.
            "export const enum Size { Small = register(1) }",
        ] {
            assert!(init(source).contains(&"Size".to_string()), "{source}");
        }
        for source in [
            "export enum Size { Small, Large }",
            "export enum Size { Small = 1, Large = Small << 1, Label = 'l' }",
            "import { memo } from 'react'; export enum Size { Small = memo(1) }",
        ] {
            assert!(init(source).is_empty(), "{source}");
        }
    }

    #[test]
    fn a_local_that_shadows_a_pure_import_is_not_the_import() {
        for source in [
            "import { memo } from 'react';
            export const Widget = class { static { const memo = () => register(1); memo(); } };",
            "import { memo } from 'react';
            export class Widget { static { function memo() { register(1); } memo(); } }",
            "import { memo } from 'react';
            export class Widget { static id = ((memo) => memo(1))(register); }",
            "import * as React from 'react';
            export class Widget { static { const React = { memo: () => register(1) }; React.memo(1); } }",
        ] {
            assert!(init(source).contains(&"Widget".to_string()), "{source}");
        }
    }

    #[test]
    fn a_pure_annotation_speaks_for_its_call_whatever_the_callee_is() {
        let source = "import { memo } from 'react';
            export class Widget { static { const memo = () => register(1); /* @__PURE__ */ memo(); } }";
        assert!(init(source).is_empty());
    }

    #[test]
    fn a_shadowed_pure_import_in_a_factory_argument_makes_the_call_initialisation() {
        let shadowed = module(
            "import { createAsyncThunk } from '@reduxjs/toolkit';
            import { memo } from 'react';
            export const t = createAsyncThunk('a/b', class {
                static { const memo = () => register(1); memo(); }
            });\n",
        );
        let t = shadowed.decl_named("t").unwrap();
        assert!(shadowed.init_decls.contains(&t));
        assert!(!shadowed.conditional_init.contains(&t));

        let imported = module(
            "import { createAsyncThunk } from '@reduxjs/toolkit';
            import { memo } from 'react';
            export const t = createAsyncThunk('a/b', class { static { memo(1); } });\n",
        );
        let t = imported.decl_named("t").unwrap();
        assert!(!imported.init_decls.contains(&t));
        assert!(imported.conditional_init.contains(&t));
    }

    /// Asserts that initialisation reaches `name` in each source.
    fn each_reaches(cases: &[(&str, &str)]) {
        for (source, name) in cases {
            assert!(
                module(source).decl_named(name).is_some(),
                "no {name}: {source}"
            );
        }
        let missed: Vec<&str> = cases
            .iter()
            .filter(|(source, name)| !init(source).contains(&name.to_string()))
            .map(|(source, _)| *source)
            .collect();
        assert!(
            missed.is_empty(),
            "not initialisation:\n{}",
            missed.join("\n")
        );
    }

    /// Asserts that initialisation reaches nothing in each source.
    fn each_reaches_nothing(sources: &[&str]) {
        let reached: Vec<String> = sources
            .iter()
            .filter_map(|source| {
                let names = init(source);
                (!names.is_empty()).then(|| format!("{names:?}: {source}"))
            })
            .collect();
        assert!(
            reached.is_empty(),
            "initialisation:\n{}",
            reached.join("\n")
        );
    }

    #[test]
    fn a_write_that_reaches_past_the_module_is_initialisation() {
        each_reaches(&[
            // A write to an object someone else owns.
            (
                "import { obj } from './obj'; export const x = (obj.n++, 1);",
                "x",
            ),
            (
                "import { obj } from './obj'; export const x = (++obj.n, 1);",
                "x",
            ),
            (
                "import { obj } from './obj'; export const x = (delete obj.n, 1);",
                "x",
            ),
            ("export const x = (delete globalThis.flag, 1);", "x"),
            // A bare name nothing declares is a property of the global object.
            ("export const x = (implicitGlobal = 1);", "x"),
            ("export const x = (implicitGlobal += 1);", "x"),
            ("export const x = (implicitGlobal ||= 1);", "x"),
            ("export const x = (implicitGlobal++, 1);", "x"),
            // So is one only an ambient declaration names.
            (
                "declare let ambient: number; export const x = (ambient = 1);",
                "x",
            ),
            // Assigning an import or a `const` throws, which stops the module loading.
            (
                "import { reg } from './reg'; export const x = (reg = 1, 1);",
                "x",
            ),
            ("const c = 1; export const x = (c = 2, 1);", "x"),
            // So does assigning a class's own name inside its body, where it is bound
            // as a constant.
            ("export class W { static { W = null; } }", "W"),
            // A local of the same name is a different binding.
            (
                "let n = 0; export class W { static { const n = 1; n = 2; } }",
                "W",
            ),
            // The same writes in the other places module initialisation reads.
            (
                "import { obj } from './obj'; export default (obj.n++, 1);",
                "default",
            ),
            (
                "import { obj } from './obj'; export class W { static { obj.n++; } }",
                "W",
            ),
            (
                "import { obj } from './obj'; export class W { static v = (obj.n++, 1); }",
                "W",
            ),
            (
                "import { obj } from './obj'; export class W { static { delete obj.n; } }",
                "W",
            ),
            ("export class W { static { implicitGlobal = 1; } }", "W"),
            (
                "const { obj } = require('./obj'); exports.x = (obj.n++, 1);",
                "exports.x",
            ),
            ("exports.x = (implicitGlobal = 1);", "exports.x"),
        ]);
    }

    #[test]
    fn a_write_to_a_binding_the_module_owns_is_not_initialisation() {
        each_reaches_nothing(&[
            "let n = 0; export const x = (n = 1);",
            "let n = 0; export const x = (n += 1);",
            "var n; export const x = (n ||= 1);",
            "function f() {} export const x = (f = null);",
            "class K {} export const x = (K = null);",
            // A write in a function body waits for a call.
            "export const f = () => { let n = 0; n++; n = 2; implicitGlobal = 1; };",
            "export function f(o) { delete o.n; }",
            "export class W { run() { W = null; } }",
        ]);
    }

    #[test]
    fn what_a_destructuring_pattern_runs_is_initialisation() {
        each_reaches(&[
            (
                "import { obj, reg } from './obj'; export const { a = reg(1) } = obj;",
                "a",
            ),
            (
                "import { reg } from './obj'; const src = {}; export const { a = reg(1) } = src;",
                "a",
            ),
            (
                "import { reg } from './obj'; export const [a = reg(1)] = [];",
                "a",
            ),
            (
                "import { key } from './obj'; export const { [key()]: a } = {};",
                "a",
            ),
            (
                "import { reg } from './obj'; export const { a: { b = reg(1) } } = { a: {} };",
                "b",
            ),
            (
                "import { reg } from './obj'; export const [, ...[c = reg(1)]] = [];",
                "c",
            ),
        ]);
    }

    #[test]
    fn a_destructuring_pattern_that_runs_nothing_is_not_initialisation() {
        each_reaches_nothing(&[
            "const src = {}; export const { a = 1, b = [src] } = src;",
            "export const [a = 'x', { b } = {}] = [];",
            "export const { ['literal']: a } = {};",
            "import { memo } from 'react'; export const { a = memo(1) } = {};",
            "export const { a = () => register(1) } = {};",
        ]);
    }

    #[test]
    fn a_read_before_the_declaration_is_initialisation() {
        each_reaches(&[
            ("export const v = later; const later = 1;", "v"),
            ("export const v = [Later]; class Later {}", "v"),
            ("export const v = { later }; let later = 1;", "v"),
            ("export const v = typeof later; const later = 1;", "v"),
            // A binding read in its own initialiser has not been declared yet either.
            ("export const v = [v];", "v"),
            // Writing one early throws too.
            ("export const x = (n = 1); let n = 0;", "x"),
            // A class's `extends` expression and computed keys run before its name is
            // bound, and a class bound to a `const` is not bound until the class is
            // done.
            ("export class W { [W.key] = 1; }", "W"),
            ("export const W = class { static v = [W]; };", "W"),
            // A local of the same name declared later is a different binding.
            (
                "const later = 1; export class W { static { const v = [later]; const later = 2; } }",
                "W",
            ),
            // The same read in the other places module initialisation reads.
            ("export default [later]; const later = 1;", "default"),
            (
                "export class W { static v = [later]; } const later = 1;",
                "W",
            ),
            (
                "export class W { static { const v = [later]; } } let later = 1;",
                "W",
            ),
            ("export const { a = later } = {}; const later = 1;", "a"),
            ("exports.x = [later]; const later = 1;", "exports.x"),
        ]);
    }

    #[test]
    fn a_read_after_the_declaration_or_in_a_body_that_waits_is_not_initialisation() {
        each_reaches_nothing(&[
            "const earlier = 1; export const v = [earlier];",
            "class Earlier {} export const v = [Earlier];",
            // A `var` and a function are there from the start.
            "export const v = [later]; var later = 1;",
            "export const v = [later]; function later() {}",
            // A body that waits for a call runs after the whole module has.
            "export const read = () => later; const later = 1;",
            "export function read() { return later; } const later = 1;",
            "export class W { run() { return later; } } const later = 1;",
            "export const W = class { run() { return W; } };",
            // A static member runs once the class's own name is bound.
            "export class W { static id = 1; static v = [W.id]; static { const v = [W]; } }",
            // A name used as a type is erased.
            "export const v: typeof later | undefined = undefined; const later = 1;",
        ]);
    }
}
