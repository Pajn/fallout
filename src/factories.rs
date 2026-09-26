//! Factories whose results are read by property.
//!
//! A Redux Toolkit app makes every async action with `createAsyncThunk(type,
//! payloadCreator)`, and its slices name them in their reducers:
//! `builder.addCase(refresh.fulfilled, …)`. Read as a plain call, that reaches the
//! whole declaration, payload creator and all, and so everything it calls — which
//! in a real app is most of what talks to the network. `refresh.fulfilled` depends
//! on none of it. It is an action creator made from the type string alone.
//!
//! A rule here says so about one factory: calling it only builds a value, so module
//! initialisation reaches the call's callee and the rest of the call around the
//! arguments but not the arguments themselves, and a property it lists depends
//! only on the arguments it lists. Reading any other property of the result, calling the
//! result, or using it as a whole depends on every argument.
//!
//! A factory is matched however an app reaches it: imported directly, through a
//! namespace, through `withTypes<…>()`, which types it and returns it, and through
//! a module of the app's own that exports any of those. The last is the usual
//! shape — `export const createAsyncThunk = createAsyncThunkOriginal.withTypes<…>()`
//! in a store module, imported by every slice — and it is why this is decided on the
//! graph rather than in one module.
//!
//! Like the built-in pure calls, this is a claim about someone else's function, and
//! only the library's own documented behaviour is claimed.

/// One factory, and what its result's properties depend on.
#[derive(Debug, PartialEq, Eq)]
pub struct Rule {
    /// The module specifiers it is imported from, written as an import writes them.
    pub sources: &'static [&'static str],
    /// The name those modules export it under.
    pub export: &'static str,
    /// Properties of its result read on their own, each with the arguments, by
    /// position, it depends on.
    pub members: &'static [(&'static str, &'static [usize])],
}

impl Rule {
    /// The arguments `name` depends on, if it is a property the rule lists.
    pub fn member(&self, name: &str) -> Option<&'static [usize]> {
        self.members
            .iter()
            .find(|(member, _)| *member == name)
            .map(|(_, args)| *args)
    }

    /// Every property the rule lists.
    pub fn member_names(&self) -> impl Iterator<Item = &'static str> {
        self.members.iter().map(|(name, _)| *name)
    }

    /// Whether `source#export` names this factory.
    pub fn names(&self, source: &str, export: &str) -> bool {
        self.export == export && self.sources.contains(&source)
    }
}

/// `createAsyncThunk(type, payloadCreator, options)` returns a thunk action creator
/// with `pending`, `fulfilled` and `rejected` action creators, a `settled` matcher,
/// and its `typePrefix`, built from `type`. `rejected` also serialises the error it
/// is given with `options.serializeError`, so it reads the options too. The payload
/// creator only runs when the thunk is dispatched, and the rest of the options with
/// it.
pub const RULES: &[Rule] = &[Rule {
    sources: &["@reduxjs/toolkit", "@reduxjs/toolkit/react"],
    export: "createAsyncThunk",
    members: &[
        ("pending", &[0]),
        ("fulfilled", &[0]),
        ("rejected", &[0, 2]),
        ("settled", &[0]),
        ("typePrefix", &[0]),
    ],
}];

/// The rule for `source#export`, if there is one.
pub fn rule(source: &str, export: &str) -> Option<&'static Rule> {
    RULES.iter().find(|rule| rule.names(source, export))
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::Arc;

    use crate::config::{Bundler, Configs};
    use crate::graph::Graph;
    use crate::module::Reading;

    /// Whether `t` in `slice.ts` is made by a factory with a rule, in a tree of the
    /// given files.
    fn made_by_rule(files: &[(&str, &str)]) -> bool {
        let dir = tempfile::tempdir().unwrap();
        let root = dunce::canonicalize(dir.path()).unwrap();
        for (name, body) in files {
            let path = root.join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, body).unwrap();
        }
        let graph = Graph::new(
            Reading {
                configs: Arc::new(Configs::new(&root)),
                ignore_types: true,
            },
            Bundler::default(),
            Arc::default(),
            root.clone(),
            Arc::default(),
        );
        let file = graph.file_id(&root.join(Path::new("slice.ts")));
        let analysed = graph.analysis(file).unwrap();
        let module = analysed.analysis.as_fine().unwrap();
        let decl = module.decl_named("t").expect("t");
        graph.made_by(file, decl).is_some()
    }

    const THUNK: &str = "export const t = createAsyncThunk('a/b', async () => load());\n";

    #[test]
    fn a_factory_is_found_however_the_app_reaches_it() {
        for files in [
            // Imported directly.
            vec![(
                "slice.ts",
                format!("import {{ createAsyncThunk }} from '@reduxjs/toolkit';\n{THUNK}"),
            )],
            // Through a namespace.
            vec![(
                "slice.ts",
                "import * as rtk from '@reduxjs/toolkit';\nexport const t = rtk.createAsyncThunk('a/b', async () => 1);\n"
                    .to_string(),
            )],
            // Typed with `withTypes`, in the same file.
            vec![(
                "slice.ts",
                format!(
                    "import {{ createAsyncThunk as base }} from '@reduxjs/toolkit';\nconst createAsyncThunk = base.withTypes<{{ state: unknown }}>();\n{THUNK}"
                ),
            )],
            // Typed in a store module, and re-exported by a barrel.
            vec![
                (
                    "store/redux.ts",
                    "import { createAsyncThunk as base } from '@reduxjs/toolkit';\nexport const createAsyncThunk = base.withTypes<{ state: unknown }>();\n"
                        .to_string(),
                ),
                (
                    "store/index.ts",
                    "export { createAsyncThunk } from './redux';\n".to_string(),
                ),
                (
                    "slice.ts",
                    format!("import {{ createAsyncThunk }} from './store';\n{THUNK}"),
                ),
            ],
            // Through a namespace of the store module.
            vec![
                (
                    "store.ts",
                    "import * as rtk from '@reduxjs/toolkit';\nexport const createAsyncThunk = rtk.createAsyncThunk.withTypes<{ state: unknown }>();\n"
                        .to_string(),
                ),
                (
                    "slice.ts",
                    "import * as store from './store';\nexport const t = store.createAsyncThunk('a/b', async () => 1);\n"
                        .to_string(),
                ),
            ],
        ] {
            let files: Vec<(&str, &str)> = files
                .iter()
                .map(|(name, body)| (*name, body.as_str()))
                .collect();
            assert!(made_by_rule(&files), "{files:?}");
        }
    }

    #[test]
    fn a_function_of_the_same_name_is_not_the_factory() {
        for files in [
            vec![(
                "slice.ts",
                format!("import {{ createAsyncThunk }} from 'another-library';\n{THUNK}"),
            )],
            vec![
                (
                    "store.ts",
                    "export function createAsyncThunk(type: string, run: () => unknown) { return run; }\n"
                        .to_string(),
                ),
                (
                    "slice.ts",
                    format!("import {{ createAsyncThunk }} from './store';\n{THUNK}"),
                ),
            ],
            // A call on the factory's result is not the factory.
            vec![(
                "slice.ts",
                "import { createAsyncThunk as base } from '@reduxjs/toolkit';\nconst createAsyncThunk = base('x', async () => 1);\nexport const t = createAsyncThunk('a/b', async () => 1);\n"
                    .to_string(),
            )],
        ] {
            let files: Vec<(&str, &str)> = files
                .iter()
                .map(|(name, body)| (*name, body.as_str()))
                .collect();
            assert!(!made_by_rule(&files), "{files:?}");
        }
    }
}
