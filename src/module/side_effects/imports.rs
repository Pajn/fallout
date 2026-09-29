//! Where each import of a file comes from, and which callees the project's pure
//! list speaks for there.
//!
//! An entry is a claim about someone else's function, so it is honoured only where
//! a callee is reached from the import the entry names. That is decided by the
//! binding a callee resolves to, not by its name, so a local of the same name inside
//! a function or a class body is not covered.

use ahash::AHashMap;
use oxc_ast::ast::*;
use oxc_semantic::{Scoping, SymbolId};

use crate::factories::rules::{Wrapper, wrapper};
use crate::module::decls::ImportBinding;
use crate::module::{ImportTarget, SourceId};
use crate::pure::PureList;

pub(super) struct Imports<'c> {
    scoping: &'c Scoping,
    /// Each imported binding as `symbol -> (module specifier, exported name)`, with
    /// `default` and `*` standing for the two unnamed import forms.
    by_symbol: AHashMap<SymbolId, (&'c str, &'c str)>,
    /// Where each imported binding's specifier sits in the module's sources.
    source_ids: AHashMap<SymbolId, SourceId>,
    pure: &'c PureList,
}

impl<'c> Imports<'c> {
    pub(super) fn new(
        scoping: &'c Scoping,
        imports: &'c [ImportBinding],
        sources: &'c [String],
        pure: &'c PureList,
    ) -> Self {
        let mut by_symbol = AHashMap::default();
        let mut source_ids = AHashMap::default();
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
                ImportTarget::Named(name) | ImportTarget::Member { export: name, .. } => {
                    name.as_str()
                }
                ImportTarget::Namespace => "*",
            };
            by_symbol.insert(symbol, (source.as_str(), exported));
            source_ids.insert(symbol, binding.reference.source);
        }
        Self {
            scoping,
            by_symbol,
            source_ids,
            pure,
        }
    }

    /// Does the project's list call `callee` free of side effects here?
    pub(super) fn listed(&self, callee: &Expression<'_>) -> bool {
        let Some((root, members)) = callee_path(callee) else {
            return false;
        };
        let Some((source, exported)) = self.of(root) else {
            return false;
        };

        let mut path = Vec::with_capacity(members.len() + 1);
        path.push(exported);
        path.extend(members);
        self.pure.contains(source, &path)
    }

    /// The Zustand middleware `callee` is, reached from its import directly or
    /// through a namespace, with the source of that import. Like an entry of the
    /// list, it is known by the binding the callee resolves to, not by its name.
    pub(super) fn wrapper(&self, callee: &Expression<'_>) -> Option<(&'static Wrapper, SourceId)> {
        let (root, members) = callee_path(callee)?;
        let (source, exported) = self.of(root)?;
        let export = match (exported, members.as_slice()) {
            ("*", [name]) => *name,
            (name, []) if name != "*" => name,
            _ => return None,
        };
        let symbol = self
            .scoping
            .get_reference(root.reference_id.get()?)
            .symbol_id()?;
        Some((wrapper(source, export)?, *self.source_ids.get(&symbol)?))
    }

    /// Does `identifier` read an import binding of the file?
    pub(super) fn is_import(&self, identifier: &IdentifierReference<'_>) -> bool {
        self.of(identifier).is_some()
    }

    /// The import `identifier` reads, as `(module specifier, exported name)`, or
    /// `None` when it reads anything else.
    fn of(&self, identifier: &IdentifierReference<'_>) -> Option<(&'c str, &'c str)> {
        let reference = self.scoping.get_reference(identifier.reference_id.get()?);
        self.by_symbol.get(&reference.symbol_id()?).copied()
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
