//! The properties of a declaration bound to a plain object literal.
//!
//! `export const utils = { formatDate, formatPrice }` is a namespace written by
//! hand, and it is read like one: `utils.formatDate()` picks one property back out.
//! Where every property can be read on its own, each gets the dependencies of its
//! own value, so a reader of one does not reach the others — in another module or
//! in this one.
//!
//! Nothing here narrows unless the object is known to stay as written. A binding
//! that can be reassigned, written through, handed somewhere or read whole, and a
//! literal with a spread, a computed key, an accessor, a prototype or a `this`, all
//! leave the declaration with no members, which keeps it a single node.

use ahash::AHashMap;
use oxc_ast::AstKind;
use oxc_ast::ast::*;
use oxc_ast_visit::{Visit, walk};
use oxc_semantic::SymbolId;
use oxc_span::GetSpan;

use super::cjs;
use super::decls::{DeclDraft, ImportBinding};
use super::parse::{Ctx, is_require, span_of};
use super::refs::{Use, classify, narrowed, unwritten_member_read};
use super::{Decl, DeclId, ImportRef, Member, Span};

/// The declarations whose properties can be read apart, found before any reference
/// is linked so that a reader in this file can be linked to the property it reads.
#[derive(Default)]
pub(crate) struct Objects {
    list: Vec<Object>,
    /// The binding of each object that has one, for the readers in this file.
    pub by_symbol: AHashMap<SymbolId, DeclId>,
}

struct Object {
    /// Every declaration the statement introduces, which all hold this one object:
    /// `exports.a = exports.b = { ... }` is two names for it.
    decls: Vec<DeclId>,
    interior: Span,
    members: Vec<Member>,
}

/// Every object declaration whose properties can be read apart.
pub(crate) fn find<'a>(
    ctx: &Ctx<'_>,
    program: &'a Program<'a>,
    drafts: &[DeclDraft],
    cjs: &cjs::Table,
) -> Objects {
    let mut objects = Objects::default();
    for (index, statement) in program.body.iter().enumerate() {
        let Some((binding, object)) = object_literal_of(statement, index, cjs) else {
            continue;
        };
        let decls: Vec<DeclId> = drafts
            .iter()
            .enumerate()
            .filter(|(_, draft)| draft.statement == index)
            .map(|(id, _)| id as DeclId)
            .collect();
        if decls.is_empty() {
            continue;
        }
        // An anonymous object — a default export, a CommonJS export — has no name
        // this file could reach it by.
        let symbol = match binding {
            Some(binding) => {
                let Some(symbol) = binding.symbol_id.get() else {
                    continue;
                };
                if !only_read_for_members(ctx, symbol, span_of(statement.span())) {
                    continue;
                }
                Some(symbol)
            }
            None => None,
        };
        let Some(members) = members(ctx, object) else {
            continue;
        };
        if let Some(symbol) = symbol {
            objects.by_symbol.insert(symbol, decls[0]);
        }
        objects.list.push(Object {
            decls,
            interior: Span {
                start: object.span.start + 1,
                end: object.span.end.saturating_sub(1),
            },
            members,
        });
    }
    objects
}

/// Fills in the members of every object [`find`] found.
///
/// Runs once every other edge of the declaration is known, since an edge no single
/// property accounts for belongs to all of them.
pub(crate) fn attach(
    ctx: &Ctx<'_>,
    drafts: &[DeclDraft],
    imports: &[ImportBinding],
    objects: Objects,
    decls: &mut [Decl],
) {
    let Objects {
        mut list,
        by_symbol,
    } = objects;
    if list.is_empty() {
        return;
    }

    link_members(ctx, drafts, imports, &by_symbol, &mut list);

    for object in list {
        let mut members = object.members;
        let first = &decls[object.decls[0] as usize];
        // What the declaration depends on beyond its properties' values — a type
        // annotation, the shared-state rule — is not any one property's to drop.
        let extra_refs: Vec<DeclId> = first
            .refs
            .iter()
            .copied()
            .filter(|target| {
                !object.decls.contains(target)
                    && !members.iter().any(|member| member.refs.contains(target))
            })
            .collect();
        let extra_members: Vec<(DeclId, String)> = first
            .member_refs
            .iter()
            .filter(|read| {
                !members
                    .iter()
                    .any(|member| member.member_refs.contains(read))
            })
            .cloned()
            .collect();
        let extra_imports: Vec<ImportRef> = first
            .imports
            .iter()
            .filter(|import| !members.iter().any(|member| member.imports.contains(import)))
            .cloned()
            .collect();
        for member in &mut members {
            for &target in &extra_refs {
                push_unique(&mut member.refs, target);
            }
            for read in &extra_members {
                push_unique(&mut member.member_refs, read.clone());
            }
            for import in &extra_imports {
                push_unique(&mut member.imports, import.clone());
            }
        }
        for &decl in &object.decls {
            decls[decl as usize].interior = object.interior;
            decls[decl as usize].members = members.clone();
        }
    }
}

/// The object literal a statement binds whole, and the binding it goes by if it
/// has one: `const x = { ... }` exported or not, `export default { ... }`, and
/// `exports.x = { ... }`.
pub(crate) fn object_literal_of<'a>(
    statement: &'a Statement<'a>,
    index: usize,
    cjs: &cjs::Table,
) -> Option<(Option<&'a BindingIdentifier<'a>>, &'a ObjectExpression<'a>)> {
    let variable = match statement {
        Statement::VariableDeclaration(variable) => variable,
        Statement::ExportDeclaration(export) => match &export.declaration {
            Declaration::VariableDeclaration(variable) => variable,
            _ => return None,
        },
        Statement::ExportDefaultDeclaration(export) => {
            return Some((None, object_literal(export.declaration.as_expression()?)?));
        }
        // Only an assignment standing on its own: `var _default = (exports.default
        // = { ... })` also binds the object to a name this file can reach it by.
        Statement::ExpressionStatement(expression) => {
            cjs.names_at(index).next()?;
            if !assigns_properties(&expression.expression) {
                return None;
            }
            return Some((
                None,
                object_literal(cjs::assigned_value(&expression.expression)?)?,
            ));
        }
        _ => return None,
    };
    if variable.kind != VariableDeclarationKind::Const || variable.declarations.len() != 1 {
        return None;
    }
    let declarator = &variable.declarations[0];
    let BindingPattern::BindingIdentifier(id) = &declarator.id else {
        return None;
    };
    Some((Some(id), object_literal(declarator.init.as_ref()?)?))
}

/// Whether every assignment in a chain writes one property of the export table.
/// `module.exports = { ... }` is the table itself, whose properties are exports of
/// their own rather than members of one.
fn assigns_properties(expression: &Expression<'_>) -> bool {
    let mut current = expression.get_inner_expression();
    loop {
        let Expression::AssignmentExpression(assignment) = current else {
            return true;
        };
        let AssignmentTarget::StaticMemberExpression(target) = &assignment.left else {
            return false;
        };
        if matches!(&target.object, Expression::Identifier(object) if object.name == "module")
            && target.property.name == "exports"
        {
            return false;
        }
        current = assignment.right.get_inner_expression();
    }
}

/// The object literal an initialiser is, through parentheses and `as const`.
fn object_literal<'s, 'a>(expression: &'s Expression<'a>) -> Option<&'s ObjectExpression<'a>> {
    match expression {
        Expression::ObjectExpression(object) => Some(object),
        Expression::ParenthesizedExpression(inner) => object_literal(&inner.expression),
        Expression::TSAsExpression(inner) => object_literal(&inner.expression),
        Expression::TSSatisfiesExpression(inner) => object_literal(&inner.expression),
        _ => None,
    }
}

/// Every reference to the binding reads one property and writes nothing, or names
/// it in an `export { }` list. Any other use — passing it, spreading it, assigning
/// through it, reading it inside its own literal — lets code this analysis does not
/// see decide what the properties hold.
fn only_read_for_members(ctx: &Ctx<'_>, symbol: SymbolId, statement: Span) -> bool {
    let scoping = ctx.semantic.scoping();
    let nodes = ctx.semantic.nodes();
    scoping.get_resolved_reference_ids(symbol).iter().all(|id| {
        let reference = scoping.get_reference(*id);
        let node_id = reference.node_id();
        let at = span_of(nodes.get_node(node_id).kind().span());
        if reference.is_write() || statement.contains(at.start) {
            return false;
        }
        matches!(nodes.parent_kind(node_id), AstKind::ExportSpecifier(_))
            || unwritten_member_read(nodes, node_id).is_some()
    })
}

/// One member per property, or `None` if any property could be something other than
/// the value written for it.
fn members(ctx: &Ctx<'_>, object: &ObjectExpression<'_>) -> Option<Vec<Member>> {
    let mut members: Vec<Member> = Vec::new();
    for property in &object.properties {
        // A spread copies properties nobody here can list.
        let ObjectPropertyKind::ObjectProperty(property) = property else {
            return None;
        };
        // A getter runs on every read, and a setter on every write.
        if property.kind != PropertyKind::Init || property.computed {
            return None;
        }
        let name = property.key.static_name()?;
        // `__proto__: base` sets the prototype, which lends the object properties
        // it never lists. The shorthand is an ordinary property.
        if name == "__proto__" && !property.shorthand {
            return None;
        }
        // A later duplicate replaces the earlier one, value and effects both.
        if members.iter().any(|member| member.name == name) {
            return None;
        }
        if reaches_outside(&property.value) || calls_with_receiver(ctx, &property.value) {
            return None;
        }
        members.push(Member {
            name: name.to_string(),
            span: span_of(property.span),
            refs: Vec::new(),
            member_refs: Vec::new(),
            imports: Vec::new(),
        });
    }
    Some(members)
}

/// Whether a value depends on something its references do not show: the object
/// itself through `this` or `super`, or a module loaded where it is written, which
/// is attributed to the statement rather than to a reference.
fn reaches_outside(value: &Expression<'_>) -> bool {
    let mut finder = Outside::default();
    finder.visit_expression(value);
    finder.found
}

#[derive(Default)]
struct Outside {
    found: bool,
}

impl<'a> Visit<'a> for Outside {
    fn visit_this_expression(&mut self, _: &ThisExpression) {
        self.found = true;
    }

    fn visit_super(&mut self, _: &Super) {
        self.found = true;
    }

    fn visit_import_expression(&mut self, _: &ImportExpression<'a>) {
        self.found = true;
    }

    fn visit_call_expression(&mut self, call: &CallExpression<'a>) {
        if is_require(call) {
            self.found = true;
        }
        walk::walk_call_expression(self, call);
    }
}

/// A local function placed in the object by name, which reads the object as `this`
/// when called as `utils.fn()`. A function imported from elsewhere is not looked
/// into; like a helper that mutates itself, that is the assumption bundlers make.
fn calls_with_receiver(ctx: &Ctx<'_>, value: &Expression<'_>) -> bool {
    let Expression::Identifier(identifier) = value else {
        return false;
    };
    let scoping = ctx.semantic.scoping();
    let Some(symbol) = identifier
        .reference_id
        .get()
        .and_then(|id| scoping.get_reference(id).symbol_id())
    else {
        return false;
    };
    let nodes = ctx.semantic.nodes();
    let declaration = scoping.symbol_declaration(symbol);
    let function = match nodes.kind(declaration) {
        AstKind::Function(function) => Some(function),
        AstKind::VariableDeclarator(declarator) => match &declarator.init {
            Some(Expression::FunctionExpression(function)) => Some(function.as_ref()),
            _ => None,
        },
        _ => None,
    };
    function.is_some_and(|function| {
        let mut finder = Outside::default();
        finder.visit_formal_parameters(&function.params);
        if let Some(body) = &function.body {
            finder.visit_function_body(body);
        }
        finder.found
    })
}

/// Each member's references, read the way [`super::refs::link`] reads a
/// declaration's, but only inside the property.
///
/// Two properties of one object can also pass a value between them through a
/// binding of this file: one that writes it reaches every other that reads it, as
/// the shared-state rule has two declarations do.
fn link_members(
    ctx: &Ctx<'_>,
    drafts: &[DeclDraft],
    imports: &[ImportBinding],
    by_symbol: &AHashMap<SymbolId, DeclId>,
    list: &mut [Object],
) {
    let scoping = ctx.semantic.scoping();
    let nodes = ctx.semantic.nodes();
    let root = scoping.root_scope_id();

    let decl_by_name: AHashMap<&str, DeclId> = drafts
        .iter()
        .enumerate()
        .map(|(id, draft)| (draft.name.as_str(), id as DeclId))
        .collect();
    let import_by_name: AHashMap<&str, &ImportBinding> = imports
        .iter()
        .map(|binding| (binding.local.as_str(), binding))
        .collect();

    for symbol_id in scoping.symbol_ids() {
        if scoping.symbol_scope_id(symbol_id) != root {
            continue;
        }
        let name = scoping.symbol_name(symbol_id);
        let target_decl = decl_by_name.get(name).copied();
        let target_import = import_by_name.get(name).copied();
        if target_decl.is_none() && target_import.is_none() {
            continue;
        }
        let object = by_symbol.get(&symbol_id).copied();
        // Which properties of each object read and write this binding.
        let mut readers: Vec<(usize, usize)> = Vec::new();
        let mut writers: Vec<(usize, usize)> = Vec::new();

        for reference_id in scoping.get_resolved_reference_ids(symbol_id) {
            let node_id = scoping.get_reference(*reference_id).node_id();
            let at = span_of(nodes.get_node(node_id).kind().span()).start;
            for (index, owner) in list.iter_mut().enumerate() {
                let Some(position) = owner
                    .members
                    .iter()
                    .position(|member| member.span.contains(at))
                else {
                    continue;
                };
                let member = &mut owner.members[position];
                if let Some(target) = target_decl
                    && !owner.decls.contains(&target)
                {
                    match object.zip(unwritten_member_read(nodes, node_id)) {
                        Some((object, read)) => {
                            push_unique(&mut member.member_refs, (object, read))
                        }
                        None => push_unique(&mut member.refs, target),
                    }
                }
                if let Some(binding) = target_import {
                    push_unique(
                        &mut member.imports,
                        narrowed(nodes, node_id, &binding.reference),
                    );
                }
                readers.push((index, position));
                if classify(nodes, node_id) == Use::Mutate {
                    writers.push((index, position));
                }
            }
        }

        // An object read only for its members cannot be changed through them, and
        // a primitive `const` cannot be changed at all. An import is its own
        // module's business, as it is for the shared-state rule.
        let shared =
            target_decl.is_some_and(|decl| !drafts[decl as usize].immutable) && object.is_none();
        if !shared {
            continue;
        }
        for &(index, reader) in &readers {
            for &(writer_index, writer) in &writers {
                if writer_index == index && writer != reader {
                    let owner = &mut list[index];
                    let read = (owner.decls[0], owner.members[writer].name.clone());
                    push_unique(&mut owner.members[reader].member_refs, read);
                }
            }
        }
    }
}

fn push_unique<T: PartialEq>(list: &mut Vec<T>, value: T) {
    if !list.contains(&value) {
        list.push(value);
    }
}

#[cfg(test)]
mod tests {
    use crate::module::{ImportTarget, ModuleAnalysis, Reading, parse::analyse_source};
    use std::path::Path;

    fn module(source: &str) -> Box<crate::module::FineModule> {
        let (ModuleAnalysis::Fine(module), _) =
            analyse_source(Path::new("utils.ts"), source, &Reading::default()).unwrap()
        else {
            panic!("expected fine module: {source}")
        };
        module
    }

    /// The members of `utils`, each with the declarations it references by name.
    fn members(source: &str) -> Vec<(String, Vec<String>)> {
        members_of(source, "utils")
    }

    fn members_of(source: &str, name: &str) -> Vec<(String, Vec<String>)> {
        let module = module(source);
        let utils = module.decl_named(name).expect(name);
        module.decls[utils as usize]
            .members
            .iter()
            .map(|member| {
                let refs = member
                    .refs
                    .iter()
                    .map(|&id| module.decls[id as usize].name.clone())
                    .collect();
                (member.name.clone(), refs)
            })
            .collect()
    }

    #[test]
    fn each_property_depends_on_its_own_value() {
        let source = "function a() {} function b() {}
            export const utils = { a, renamed: b, inline: () => a(), method() { return 1; }, 'quoted': 1 } as const;
            export const read = () => utils.a;";
        assert_eq!(
            members(source),
            [
                ("a".to_string(), vec!["a".to_string()]),
                ("renamed".to_string(), vec!["b".to_string()]),
                ("inline".to_string(), vec!["a".to_string()]),
                ("method".to_string(), vec![]),
                ("quoted".to_string(), vec![]),
            ]
        );
    }

    #[test]
    fn a_property_keeps_the_import_it_reads() {
        let source = "import { format } from './format';
            import * as dates from './dates';
            export const utils = { format, parse: dates.parse, zone: () => dates.config.zone };";
        let module = module(source);
        let utils = module.decl_named("utils").unwrap() as usize;
        let targets: Vec<Vec<ImportTarget>> = module.decls[utils]
            .members
            .iter()
            .map(|member| member.imports.iter().map(|i| i.target.clone()).collect())
            .collect();
        assert_eq!(
            targets,
            [
                vec![ImportTarget::Named("format".into())],
                vec![ImportTarget::Named("parse".into())],
                vec![ImportTarget::Member {
                    export: "config".into(),
                    member: "zone".into()
                }],
            ]
        );
    }

    #[test]
    fn anything_that_reaches_or_changes_the_object_keeps_it_whole() {
        for source in [
            // The binding can change, or is used as a whole.
            "let utils = { a: 1 };",
            "var utils = { a: 1 };",
            "const utils = { a: 1 }, other = 1;",
            "const utils = { a: 1 }; utils.a = 2;",
            "const utils = { a: 1 }; utils.a.b = 2;",
            "const utils = { a: 1 }; utils.a++;",
            "const utils = { a: 1 }; delete utils.a;",
            "const utils = { a: 1 }; Object.assign(utils, {});",
            "const utils = { a: 1 }; const alias = utils;",
            "const utils = { a: 1 }; export default utils;",
            "const utils = { a: 1 }; utils['a'];",
            "const utils = { a: 1 }; const { a } = utils;",
            "const utils = { a: () => utils.b, b: 1 };",
            // The literal holds something other than values under fixed keys.
            "const other = {}; const utils = { ...other };",
            "const key = 'a'; const utils = { [key]: 1 };",
            "const utils = { get a() { return 1; } };",
            "const utils = { set a(v) {} };",
            "const utils = { __proto__: null, a: 1 };",
            "const utils = { a: 1, a: 2 };",
            "const utils = { a() { return this.b; }, b: 1 };",
            "function a() { return this.b; } const utils = { a, b: 1 };",
            "const a = function () { return this.b; }; const utils = { a, b: 1 };",
            "const utils = { a: () => import('./x') };",
            "const utils = { a: () => require('./x') };",
            // Not an object literal at all.
            "const utils = make({ a: 1 });",
            "const utils = Object.freeze({ a: 1 });",
        ] {
            assert!(members(source).is_empty(), "{source}");
        }
    }

    #[test]
    fn an_export_list_is_not_a_use_of_the_object() {
        for source in [
            "const utils = { a: 1 }; export { utils };",
            "const utils = { a: 1 }; export { utils as helpers, utils as default };",
        ] {
            assert_eq!(members(source), [("a".to_string(), vec![])], "{source}");
        }
    }

    #[test]
    fn a_default_or_commonjs_export_of_a_literal_has_members() {
        for (source, name) in [
            ("function a() {} export default { a };", "default"),
            ("function a() {} exports.utils = { a };", "exports.utils"),
            (
                "function a() {} module.exports.utils = { a };",
                "exports.utils",
            ),
            (
                "function a() {} exports.utils = exports.alias = { a };",
                "exports.alias",
            ),
        ] {
            assert_eq!(
                members_of(source, name),
                [("a".to_string(), vec!["a".to_string()])],
                "{source}"
            );
        }
    }

    #[test]
    fn a_commonjs_table_is_not_an_object_of_one_export() {
        for (source, name) in [
            ("function a() {} module.exports = { a };", "exports.a"),
            (
                "function a() {} var _default = (exports.default = { a });",
                "exports.default",
            ),
        ] {
            assert!(members_of(source, name).is_empty(), "{source}");
        }
    }

    #[test]
    fn a_reader_in_the_same_file_reads_one_property() {
        let source = "function a() {} function b() {}
            const utils = { a, b };
            export const page = () => utils.a();";
        let module = module(source);
        let page = module.decl_named("page").unwrap() as usize;
        let utils = module.decl_named("utils").unwrap();
        assert!(module.decls[page].refs.is_empty());
        assert_eq!(module.decls[page].member_refs, [(utils, "a".to_string())]);
    }

    #[test]
    fn calling_properties_is_not_a_write_shared_between_readers() {
        let source = "const utils = { a: () => 1, b: () => 2 };
            export const first = () => utils.a();
            export const second = () => utils.b();";
        let module = module(source);
        for name in ["first", "second"] {
            let decl = module.decl_named(name).unwrap() as usize;
            assert!(module.decls[decl].refs.is_empty(), "{name}");
        }
    }

    #[test]
    fn a_property_writing_what_a_sibling_reads_reaches_it() {
        let source = "let count = 0;
            export const counter = { bump: () => { count += 1; }, read: () => count, fixed: 1 };";
        let module = module(source);
        let counter = module.decl_named("counter").unwrap();
        let reads: Vec<(String, Vec<(u32, String)>)> = module.decls[counter as usize]
            .members
            .iter()
            .map(|member| (member.name.clone(), member.member_refs.clone()))
            .collect();
        assert_eq!(
            reads,
            // Returning `count` hands it on, which the shared-state rule counts as
            // a write, so `bump` reaches `read` as well.
            [
                ("bump".to_string(), vec![(counter, "read".to_string())]),
                ("read".to_string(), vec![(counter, "bump".to_string())]),
                ("fixed".to_string(), vec![]),
            ]
        );
    }

    #[test]
    fn a_shorthand_proto_is_an_ordinary_property() {
        assert_eq!(
            members("const __proto__ = 1; const utils = { __proto__ };"),
            [("__proto__".to_string(), vec!["__proto__".to_string()])]
        );
    }

    #[test]
    fn an_edge_no_property_accounts_for_belongs_to_every_member() {
        // `bump` writes `count`, which `utils` reads, so the shared-state rule puts
        // `bump` behind `utils`: every member has to keep it.
        let source = "let count = 0; function bump() { count++; }
            export const utils = { read: () => count, fixed: 1 };";
        assert_eq!(
            members(source),
            [
                (
                    "read".to_string(),
                    vec!["count".to_string(), "bump".to_string()]
                ),
                ("fixed".to_string(), vec!["bump".to_string()]),
            ]
        );
    }
}
