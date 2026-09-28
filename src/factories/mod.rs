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
//!
//! This module's interface is [`Graph::made_by`]. Behind it, [`rules`] is the table
//! and `resolve` follows a callee across files to the row it names. What a module
//! of the app records about its own calls is the parse-time half, in
//! `crate::module`, since one module cannot see past its own file.

mod resolve;
pub mod rules;

pub use rules::{RULES, Rule, rule};

use crate::graph::{FileId, Graph};
use crate::module::DeclId;

impl Graph {
    /// The rule of the factory `decl` of `file` is the result of calling, if the
    /// callee is one.
    ///
    /// An opaque file has no declarations to ask about, and so no rule.
    pub fn made_by(&self, file: FileId, decl: DeclId) -> Option<&'static Rule> {
        let fine = self.view(file).fine()?;
        let call = fine.module().decls.get(decl as usize)?.factory.as_ref()?;
        resolve::callee_rule(self, &fine, &call.callee, 0)
    }
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
            Arc::default(),
            Default::default(),
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
