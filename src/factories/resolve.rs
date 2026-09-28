//! Following a callee across files to the rule it names.
//!
//! A module records a call's callee as it is written there: an import, or a
//! declaration of its own, read through properties. Which factory that is can only
//! be told on the graph, by following declarations that are other names for it and
//! the exports of the modules it is imported from, until the path lands on a
//! package the table knows. There the path must be the factory itself, once the
//! identity forms that factory's rule declares are taken off its end.

use crate::graph::{FileId, Fine, Graph, Node};
use crate::module::{Callee, ExportTarget, Step};
use crate::resolve::is_installed;

use super::rules::{Rule, rule};

/// The rule of the factory a callee written in a fine file names, followed
/// through declarations that are other names for it and through the exports of
/// the modules it is imported from.
pub(super) fn callee_rule(
    graph: &Graph,
    fine: &Fine,
    callee: &Callee,
    depth: usize,
) -> Option<&'static Rule> {
    // A cycle of re-exports names nothing.
    if depth > 32 {
        return None;
    }
    let module = fine.module();
    match callee {
        Callee::Local { decl, path } => {
            let derived = module.decls.get(*decl as usize)?.derived.as_ref()?;
            callee_rule(graph, fine, &extended(derived, path), depth + 1)
        }
        Callee::Import { source, name, path } => {
            // Where the specifier lands in the project's own source, that is the
            // module it names, whatever it is spelled as: a `paths` entry can
            // map a package's name onto a shim.
            if let Some(target) = graph.target_of(fine.analysed(), *source) {
                let target_path = graph.path(target);
                if crate::module::is_source_file(&target_path) && !is_installed(&target_path) {
                    return export_rule(graph, target, name, path, depth + 1);
                }
            }
            let specifier = module.sources.get(*source as usize)?;
            let rule = rule(specifier, name)?;
            rule.strip_identity(path).is_empty().then_some(rule)
        }
    }
}

/// The rule of the factory `file` exports as `name`, read through `path`. An
/// opaque file exports nothing that can be named, and so no rule.
fn export_rule(
    graph: &Graph,
    file: FileId,
    name: &str,
    path: &[Step],
    depth: usize,
) -> Option<&'static Rule> {
    let fine = graph.view(file).fine()?;
    let module = fine.module();
    // `import * as store from "./store"; store.createAsyncThunk(…)`.
    let (name, path) = match (name, path) {
        ("*", [Step::Prop(first), rest @ ..]) => (first.as_str(), rest),
        ("*", _) => return None,
        (name, path) => (name, path),
    };
    let Some(export) = module.export_named(name) else {
        // Through `export *`, one module at a time, and only where one star
        // could provide the name: a factory is known by where it comes from.
        return match graph.star_providers(&fine, name).as_slice() {
            [Node::Export(next, _)] => export_rule(graph, *next, name, path, depth + 1),
            _ => None,
        };
    };
    match &export.target {
        ExportTarget::Local(decl) => {
            let derived = module.decls.get(*decl as usize)?.derived.as_ref()?;
            callee_rule(graph, &fine, &extended(derived, path), depth + 1)
        }
        ExportTarget::Reexport { source, name } => callee_rule(
            graph,
            &fine,
            &Callee::Import {
                source: *source,
                name: name.clone(),
                path: path.to_vec(),
            },
            depth + 1,
        ),
        ExportTarget::ReexportAll { source } => {
            let [Step::Prop(first), rest @ ..] = path else {
                return None;
            };
            callee_rule(
                graph,
                &fine,
                &Callee::Import {
                    source: *source,
                    name: first.clone(),
                    path: rest.to_vec(),
                },
                depth + 1,
            )
        }
    }
}

/// `callee` read further, through `path`.
fn extended(callee: &Callee, path: &[Step]) -> Callee {
    match callee {
        Callee::Import {
            source,
            name,
            path: base,
        } => Callee::Import {
            source: *source,
            name: name.clone(),
            path: base.iter().chain(path).cloned().collect(),
        },
        Callee::Local { decl, path: base } => Callee::Local {
            decl: *decl,
            path: base.iter().chain(path).cloned().collect(),
        },
    }
}
