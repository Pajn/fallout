//! The properties of a declaration bound to a plain object literal.
//!
//! `export const utils = { formatDate, formatPrice }` is a namespace written by
//! hand, and it is read like one: `utils.formatDate()` picks one property back out.
//! Where every property can be read on its own, each gets the dependencies of its
//! own value, so a consumer that reads one does not reach the others.
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

use super::decls::{DeclDraft, ImportBinding};
use super::parse::{Ctx, is_require, span_of};
use super::refs::{narrowed, unwritten_member_read};
use super::{Decl, DeclId, ImportRef, Member, Span};

/// Fills in the members of every declaration whose properties can be read apart.
///
/// Runs once every other edge of the declaration is known, since an edge no single
/// property accounts for belongs to all of them.
pub(crate) fn attach(
    ctx: &Ctx<'_>,
    program: &Program<'_>,
    drafts: &[DeclDraft],
    imports: &[ImportBinding],
    decls: &mut [Decl],
) {
    let mut objects: Vec<(DeclId, Vec<Member>)> = Vec::new();
    for (index, statement) in program.body.iter().enumerate() {
        let Some((decl, object)) = object_declaration(ctx, statement, index, drafts) else {
            continue;
        };
        let Some(members) = members(ctx, object) else {
            continue;
        };
        decls[decl as usize].interior = Span {
            start: object.span.start + 1,
            end: object.span.end.saturating_sub(1),
        };
        objects.push((decl, members));
    }
    if objects.is_empty() {
        return;
    }

    link_members(ctx, drafts, imports, &mut objects);

    for (decl, mut members) in objects {
        let entry = &mut decls[decl as usize];
        // What the declaration depends on beyond its properties' values — a type
        // annotation, the shared-state rule — is not any one property's to drop.
        let extra_refs: Vec<DeclId> = entry
            .refs
            .iter()
            .copied()
            .filter(|target| !members.iter().any(|member| member.refs.contains(target)))
            .collect();
        let extra_imports: Vec<ImportRef> = entry
            .imports
            .iter()
            .filter(|import| !members.iter().any(|member| member.imports.contains(import)))
            .cloned()
            .collect();
        for member in &mut members {
            for &target in &extra_refs {
                push_unique(&mut member.refs, target);
            }
            for import in &extra_imports {
                if !member.imports.contains(import) {
                    member.imports.push(import.clone());
                }
            }
        }
        entry.members = members;
    }
}

/// `const x = { ... }` or `export const x = { ... }`, one binding, whose name is
/// only ever read for one property at a time.
fn object_declaration<'s, 'a>(
    ctx: &Ctx<'_>,
    statement: &'s Statement<'a>,
    index: usize,
    drafts: &[DeclDraft],
) -> Option<(DeclId, &'s ObjectExpression<'a>)> {
    let (id, object) = declared_object(statement)?;
    let decl = drafts
        .iter()
        .position(|draft| draft.statement == index && draft.name == id.name.as_str())?;
    let symbol = id.symbol_id.get()?;
    only_read_for_members(ctx, symbol, span_of(statement.span()))
        .then_some((decl as DeclId, object))
}

/// The binding and the object literal of `const x = { ... }`, exported or not.
pub(crate) fn declared_object<'s, 'a>(
    statement: &'s Statement<'a>,
) -> Option<(&'s BindingIdentifier<'a>, &'s ObjectExpression<'a>)> {
    let variable = match statement {
        Statement::VariableDeclaration(variable) => variable,
        Statement::ExportDeclaration(export) => match &export.declaration {
            Declaration::VariableDeclaration(variable) => variable,
            _ => return None,
        },
        _ => return None,
    };
    if variable.kind != VariableDeclarationKind::Const || variable.declarations.len() != 1 {
        return None;
    }
    let declarator = &variable.declarations[0];
    let BindingPattern::BindingIdentifier(id) = &declarator.id else {
        return None;
    };
    Some((id, object_literal(declarator.init.as_ref()?)?))
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
fn link_members(
    ctx: &Ctx<'_>,
    drafts: &[DeclDraft],
    imports: &[ImportBinding],
    objects: &mut [(DeclId, Vec<Member>)],
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

        for reference_id in scoping.get_resolved_reference_ids(symbol_id) {
            let node_id = scoping.get_reference(*reference_id).node_id();
            let at = span_of(nodes.get_node(node_id).kind().span()).start;
            for (owner, members) in objects.iter_mut() {
                let Some(member) = members.iter_mut().find(|member| member.span.contains(at))
                else {
                    continue;
                };
                if let Some(target) = target_decl
                    && target != *owner
                {
                    push_unique(&mut member.refs, target);
                }
                if let Some(binding) = target_import {
                    let reference = narrowed(nodes, node_id, &binding.reference);
                    if !member.imports.contains(&reference) {
                        member.imports.push(reference);
                    }
                }
            }
        }
    }
}

fn push_unique(list: &mut Vec<DeclId>, value: DeclId) {
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
        let module = module(source);
        let utils = module.decl_named("utils").expect("utils");
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
