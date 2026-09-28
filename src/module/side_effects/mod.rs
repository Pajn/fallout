//! What running code at module load can do that someone else could see.
//!
//! Module initialisation is every declaration whose value is computed at import
//! time for more than its own binding. [`super::init`] decides which statements
//! those are; this module answers the question it asks of each: can this
//! expression, or declaring this, run anything observable, written where it is?
//!
//! Several sources speak to that, and the answer is theirs together:
//!
//! - the project's pure list, honoured only where a callee is reached from the
//!   import an entry names ([`crate::pure`]);
//! - a `/* @__PURE__ */` annotation, which is the call site's own claim;
//! - the local-helper proof, which shows a call to a small function of this file,
//!   or to a global without side effects, runs nothing ([`local_pure`],
//!   [`globals`]);
//! - and the rules for what else runs on load: a write that reaches past the
//!   module, what a destructuring pattern computes as it binds, what defining a
//!   class runs, and a binding read before its declaration has run.
//!
//! Whether a known factory's call runs anything is not here. That needs the
//! callee's own file, so [`super::init`] defers it and the graph resolves it.

mod globals;
mod imports;
mod local_pure;

use ahash::AHashMap;
use oxc_ast::ast::*;
use oxc_ast_visit::{Visit, walk};
use oxc_semantic::{Scoping, SymbolFlags, SymbolId};
use oxc_span::GetSpan;

use super::cjs;
use super::decls::ImportBinding;
use super::parse::Ctx;
use crate::pure::PureList;
use imports::Imports;
use local_pure::{LocalPure, Proof};

/// What code run at module load in one file is judged against: where each import
/// comes from, what the project has declared pure, what the local-helper proof has
/// shown, and when each lexical binding of the file can first be read.
///
/// Built once per file, then asked about any expression or declaring statement in
/// it.
pub(super) struct SideEffects<'c, 'a> {
    scoping: &'c Scoping,
    /// The local-helper proof. It holds where each import comes from and what the
    /// project has declared pure, since it reads them too.
    local: LocalPure<'c, 'a>,
    readable: AHashMap<SymbolId, u32>,
}

impl<'c, 'a> SideEffects<'c, 'a> {
    pub(super) fn new(
        ctx: &'c Ctx<'a>,
        program: &Program<'a>,
        imports: &'c [ImportBinding],
        sources: &'c [String],
        pure: &'c PureList,
    ) -> Self {
        let scoping = ctx.semantic.scoping();
        let imports = Imports::new(scoping, imports, sources, pure);
        Self {
            scoping,
            local: LocalPure::infer(ctx, program, imports),
            readable: readable_from(ctx),
        }
    }

    /// Whether evaluating `expression` where it is written runs anything.
    ///
    /// A call, a construction or a tagged template does unless one of the sources
    /// clears it, and an `await` always does. So does a write to anything but a
    /// binding the module owns, and a read of a binding before its declaration has
    /// run. A function body inside it is not evaluated, and so says nothing.
    pub(super) fn runs(&self, expression: &Expression<'_>) -> bool {
        self.detect(|detector| detector.visit_expression(expression))
    }

    /// Whether declaring what `statement` declares runs anything: its initialisers,
    /// the defaults and computed keys of its patterns, what defining its class
    /// runs, or its enum members' values.
    ///
    /// `statement` is a top-level statement that declares something, so an
    /// expression statement is read as a CommonJS export.
    pub(super) fn declaring_runs(&self, statement: &Statement<'_>) -> bool {
        let declaration = match statement {
            Statement::ExportDeclaration(export) => Some(&export.declaration),
            Statement::ExportDefaultDeclaration(export) => {
                return self.default_runs(&export.declaration);
            }
            // Either the assignment that fills the table, whose initialiser is the
            // value it assigns, or a defined property, which stores what it is handed
            // without running any of it.
            Statement::ExpressionStatement(statement) => {
                return cjs::assigned_value(&statement.expression)
                    .is_some_and(|value| self.runs(value));
            }
            statement => statement.as_declaration(),
        };

        let variable = match declaration {
            Some(Declaration::VariableDeclaration(variable)) => variable,
            Some(Declaration::ClassDeclaration(class)) => {
                return self.detect(|detector| detector.visit_class(class));
            }
            // Each member's value is computed when the enum is, and a `const enum` is
            // read the same way because type erasure keeps it.
            Some(Declaration::TSEnumDeclaration(enumeration)) => {
                return enumeration
                    .body
                    .members
                    .iter()
                    .filter_map(|member| member.initializer.as_ref())
                    .any(|init| self.runs(init));
            }
            // A function declaration binds without running anything.
            _ => return false,
        };

        variable.declarations.iter().any(|declarator| {
            // `var _default = (exports.default = …)`, a compiler's `export default`.
            // Filling the table is the export, not an effect on anybody else, so what
            // decides whether this runs anything is the value alone.
            let init = declarator
                .init
                .as_ref()
                .is_some_and(|init| self.runs(cjs::assigned_value(init).unwrap_or(init)));
            // A pattern computes its defaults and its computed keys as it binds, so
            // those run with the initialiser. Taking the value apart is left to the
            // value, as reading a property is everywhere else.
            init || self.detect(|detector| detector.visit_binding_pattern(&declarator.id))
        })
    }

    /// `export default …`, which binds a declaration or evaluates an expression.
    fn default_runs(&self, declaration: &ExportDefaultDeclarationKind<'_>) -> bool {
        match declaration {
            ExportDefaultDeclarationKind::ClassDeclaration(class) => {
                self.detect(|detector| detector.visit_class(class))
            }
            // `export default function f() {}` binds without running anything.
            ExportDefaultDeclarationKind::FunctionDeclaration(_)
            | ExportDefaultDeclarationKind::TSInterfaceDeclaration(_) => false,
            expression => expression
                .as_expression()
                .is_some_and(|expression| self.runs(expression)),
        }
    }

    fn detect(&self, visit: impl FnOnce(&mut Detector<'_, 'c, 'a>)) -> bool {
        let mut detector = Detector::new(self);
        visit(&mut detector);
        detector.impure
    }
}

/// Each `let`, `const` and declared class of the file, and the offset at which its
/// declaration has run. Reading or writing one before then throws, and a throw on
/// load stops the module loading, and every importer with it.
///
/// That is the end of the whole declarator, as it is for the local-helper proof,
/// so a binding a pattern gives is taken as readable only once the pattern is
/// done; and the end of a class, whose own name its static members may still read
/// (see [`Detector::statics`]).
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

/// Walks what runs on load, and records whether any of it runs something.
///
/// A call or a construction the local-helper proof covers whole is not walked
/// again. Everything that proof accepts is an expression this walk finds nothing
/// in: it takes no write, `delete`, update, `await` or tagged template, no call or
/// construction that neither it nor the list nor an annotation clears, no function
/// or class, and no binding read before its declaration has run, since each proof
/// holds only from the end of the latest declaration it depends on and a call
/// written earlier is not proven.
struct Detector<'s, 'c, 'a> {
    effects: &'s SideEffects<'c, 'a>,
    impure: bool,
    /// The declared classes whose bodies are being read, innermost last. Inside
    /// its own body a class's name is a constant.
    classes: Vec<SymbolId>,
    /// The classes whose static fields and blocks are being read. Those run once
    /// the class's own name is bound, so they may read it, while its `extends`
    /// expression and computed keys run before and may not.
    statics: Vec<SymbolId>,
}

impl<'s, 'c, 'a> Detector<'s, 'c, 'a> {
    fn new(effects: &'s SideEffects<'c, 'a>) -> Self {
        Self {
            effects,
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
        let scoping = self.effects.scoping;
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
        self.effects
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
        let scoping = self.effects.scoping;
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
        annotated || self.effects.local.imports().listed(callee)
    }
}

impl<'a> Visit<'a> for Detector<'_, '_, '_> {
    fn visit_call_expression(&mut self, expr: &CallExpression<'a>) {
        let proof = self.effects.local.call(expr);
        if proof.is_none() && !self.callee_is_pure(expr.pure, &expr.callee) {
            self.impure = true;
        }
        // A pure callee says nothing about its arguments, which still run, unless
        // the local proof has already covered them.
        if proof != Some(Proof::Whole) {
            walk::walk_call_expression(self, expr);
        }
    }

    fn visit_new_expression(&mut self, expr: &NewExpression<'a>) {
        let proof = self.effects.local.construct(expr);
        if proof.is_none() && !self.callee_is_pure(expr.pure, &expr.callee) {
            self.impure = true;
        }
        if proof != Some(Proof::Whole) {
            walk::walk_new_expression(self, expr);
        }
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

#[cfg(test)]
mod tests {
    use oxc_allocator::Allocator;
    use oxc_parser::Parser;
    use oxc_semantic::SemanticBuilder;
    use oxc_span::SourceType;

    use super::SideEffects;
    use crate::config::Configs;
    use crate::module::decls::collect_imports;
    use crate::module::parse::{Ctx, collect_sources};
    use crate::module::{FineModule, ModuleAnalysis, Reading, parse::analyse_source};
    use crate::pure::{PureCall, PureList};
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

    /// Whether declaring what any statement of `source` declares runs anything,
    /// asked of [`SideEffects`] directly with `pure` as the project's list. Each
    /// source is written so that only the statement under test could.
    fn declaring_runs(source: &str, pure: &PureList) -> bool {
        let allocator = Allocator::default();
        let parsed = Parser::new(&allocator, source, SourceType::ts()).parse();
        assert!(
            parsed.diagnostics.is_empty(),
            "{source}: {:?}",
            parsed.diagnostics
        );
        let built = SemanticBuilder::new()
            .with_build_nodes(true)
            .build(&parsed.program);
        let ctx = Ctx {
            semantic: &built.semantic,
            statements: Vec::new(),
        };
        let sources = collect_sources(&parsed.program);
        let imports = collect_imports(&parsed.program, &sources).expect("readable imports");
        let effects = SideEffects::new(&ctx, &parsed.program, &imports, &sources, pure);
        parsed
            .program
            .body
            .iter()
            .any(|statement| effects.declaring_runs(statement))
    }

    #[test]
    fn a_pure_listed_call_in_a_class_static_initialiser_is_cleared_by_the_list() {
        for source in [
            "import { memo } from 'react'; export class W { static View = memo(1); }",
            "import * as React from 'react'; export class W { static { React.memo(1); } }",
            "import { memo } from 'react'; export const W = class { static View = memo(1); };",
        ] {
            assert!(!declaring_runs(source, &PureList::builtin()), "{source}");
            assert!(declaring_runs(source, &PureList::default()), "{source}");
        }
    }

    #[test]
    fn a_pure_listed_call_still_runs_what_its_arguments_run() {
        let list = PureList::builtin();
        // A local helper's call in the argument is proven on its own.
        assert!(!declaring_runs(
            "import { memo } from 'react'; const make = (x) => ({ x });
            export class W { static View = memo(make(1)); }",
            &list
        ));
        for source in [
            "import { memo } from 'react'; export class W { static View = memo(register()); }",
            "import { memo, obj } from 'react'; export const v = memo((obj.n = 1));",
            // An argument read before its declaration has run throws.
            "import { memo } from 'react'; export const v = memo(later); const later = 1;",
            // A helper called with an argument the proof cannot read is not proven.
            "import { memo, obj } from 'react'; const make = (x) => ({ x });
            export const v = memo(make(obj.x));",
        ] {
            assert!(declaring_runs(source, &list), "{source}");
        }
    }

    #[test]
    fn a_pure_annotation_clears_its_call_and_leaves_its_arguments_to_the_others() {
        let none = PureList::default();
        assert!(!declaring_runs(
            "const make = (x) => ({ x }); export const v = /* @__PURE__ */ register(make(1));",
            &none
        ));
        assert!(!declaring_runs(
            "import { memo } from 'react'; export const v = /* @__PURE__ */ register(memo(1));",
            &PureList::builtin()
        ));
        for source in [
            "import { memo } from 'react'; export const v = /* @__PURE__ */ register(memo(1));",
            "export const v = /* @__PURE__ */ register(other());",
        ] {
            assert!(declaring_runs(source, &none), "{source}");
        }
    }

    /// The project's list with one entry of its own, `./memo#memo`.
    fn memo_listed() -> PureList {
        PureList::of(false, PureCall::parse("./memo#memo").into_iter().collect())
    }

    #[test]
    fn a_helper_calling_a_pure_listed_import_is_proven() {
        for source in [
            "import { memo } from './memo'; function make(v) { return memo(v); }
            export const C = make(1);",
            "import { memo } from './memo'; const make = (v) => memo({ v });
            export const C = make('a');",
            "import { memo } from './memo'; function make(v) { const m = memo([v]); return m; }
            export const C = make(1);",
            // The argument is proven as any other, and a helper call is one it proves.
            "import { memo } from './memo'; const wrap = (v) => ({ v });
            function make(v) { return memo(wrap(v)); } export const C = make(1);",
            // An import is bound before anything in the module runs.
            "function make(v) { return memo(v); } export const C = make(1);
            import { memo } from './memo';",
            // A call in the module body is proven whole when its argument is one.
            "import { memo } from './memo'; function make(x) { return { x }; }
            export const C = make(memo(1));",
        ] {
            assert!(!declaring_runs(source, &memo_listed()), "{source}");
            assert!(declaring_runs(source, &PureList::default()), "{source}");
        }
        // The entry's whole path is matched, members and all.
        let source = "import * as React from 'react'; function make(v) { return React.memo(v); }
            export const C = make(1);";
        assert!(!declaring_runs(source, &PureList::builtin()), "{source}");
        assert!(declaring_runs(source, &PureList::default()), "{source}");
    }

    #[test]
    fn a_helper_calling_a_pure_listed_name_that_is_not_the_import_is_not_proven() {
        for source in [
            // A parameter or a local named like the import is somebody else's value.
            "import { memo } from './memo'; function make(memo) { return memo(1); }
            export const C = make(1);",
            "import { memo } from './memo'; function make() { const memo = 1; return memo(1); }
            export const C = make();",
            "import { memo } from './memo'; const make = (memo) => memo(1);
            export const C = make(1);",
            // The list clears the call, not what its arguments run.
            "import { memo } from './memo'; function make() { return memo(register()); }
            export const C = make();",
            "import { memo, obj } from './memo'; function make() { return memo(obj.x); }
            export const C = make();",
            // An entry speaks only for the import it names.
            "import { memo } from './other'; function make(v) { return memo(v); }
            export const C = make(1);",
            "import memo from './memo'; function make(v) { return memo(v); }
            export const C = make(1);",
            // A call to something else of the same module is not the listed one.
            "import { memo, other } from './memo'; function make(v) { return other(v); }
            export const C = make(1);",
        ] {
            assert!(declaring_runs(source, &memo_listed()), "{source}");
        }
        // React's entries do not speak for a `memo` of the project's own.
        let source = "import { memo } from './memo'; function make(v) { return memo(v); }
            export const C = make(1);";
        assert!(declaring_runs(source, &PureList::builtin()), "{source}");
    }

    #[test]
    fn a_helper_calling_a_pure_listed_import_is_proven_only_below_the_claim() {
        let dir = tempfile::tempdir().expect("temp dir");
        let package = dir.path().join("packages/ui");
        std::fs::create_dir_all(&package).expect("a directory");
        std::fs::write(package.join("fallout.toml"), "pure = [\"./memo#memo\"]\n")
            .expect("writing the config");
        let reading = Reading {
            configs: std::sync::Arc::new(Configs::new(dir.path())),
            ignore_types: true,
        };
        let source = "import { memo } from './memo'; function make(v) { return memo(v); }
            export const C = make(1);";
        let init = |path: &Path| {
            let (ModuleAnalysis::Fine(module), _) = analyse_source(path, source, &reading).unwrap()
            else {
                panic!("expected fine module: {source}")
            };
            module
                .init_decls
                .iter()
                .map(|&id| module.decls[id as usize].name.clone())
                .collect::<Vec<_>>()
        };
        assert!(init(&package.join("card.ts")).is_empty());
        assert!(init(&dir.path().join("apps/web/panel.ts")).contains(&"C".to_string()));
    }

    #[test]
    fn a_pure_annotation_in_a_helper_clears_its_call_and_not_its_arguments() {
        let none = PureList::default();
        for source in [
            "import { build } from './build'; function make(v) { return /* @__PURE__ */ build(v); }
            export const C = make(1);",
            "import * as b from './build'; const make = (v) => /* @__PURE__ */ b.build({ v });
            export const C = make(1);",
            "function make(v) { /* @__PURE__ */ track(v); return v; } export const C = make(1);",
            "function make(f) { return /* @__PURE__ */ f(1); } export const C = make(1);",
        ] {
            assert!(!declaring_runs(source, &none), "{source}");
        }
        for source in [
            "import { build } from './build';
            function make() { return /* @__PURE__ */ build(register()); } export const C = make();",
            "import { build, obj } from './build';
            function make() { return /* @__PURE__ */ build(obj.x); } export const C = make();",
            // Without the annotation the call is unknown.
            "import { build } from './build'; function make(v) { return build(v); }
            export const C = make(1);",
            // Calling a `const` before its declaration has run throws, annotated or not.
            "function make(v) { return /* @__PURE__ */ build(v); } export const C = make(1);
            const build = (v) => register(v);",
        ] {
            assert!(declaring_runs(source, &none), "{source}");
        }
    }

    #[test]
    fn what_a_frozen_literal_or_a_collection_holds_is_judged_by_every_source() {
        for source in [
            "import { memo } from 'react'; export const v = Object.freeze({ View: memo(1) });",
            "import { memo } from 'react'; export const v = new Set([memo(1)]);",
            "import { memo } from 'react'; export const v = new Map([['a', memo(1)]]);",
        ] {
            assert!(!declaring_runs(source, &PureList::builtin()), "{source}");
            assert!(declaring_runs(source, &PureList::default()), "{source}");
        }
    }
}
