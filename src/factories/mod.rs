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
//! Some factories call what they are given while they build. Zustand's
//! `create(createState)` calls `createState` for the store's initial state, so
//! creating a store runs whatever the creator's body runs. Its rule names that
//! argument, and initialisation reaches only the frame where the module proves that
//! calling it there runs nothing; see [`Creation::Calls`].
//!
//! A factory is matched however an app reaches it: imported directly, through a
//! namespace, through `withTypes<…>()`, which types it and returns it, and through
//! a module of the app's own that exports any of those. The last is the usual
//! shape — `export const createAsyncThunk = createAsyncThunkOriginal.withTypes<…>()`
//! in a store module, imported by every slice — and it is why this is decided on the
//! graph rather than in one module. A call such as `withTypes<…>()` returns the
//! factory only because RTK says so of its own factory, so each rule declares its
//! identity forms, and only the matched rule's are read through.
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

pub use rules::{Creation, Identity, RULES, Rule, rule};

use crate::graph::{FileId, Fine, Graph};
use crate::module::{Decl, DeclId, Deps, FactoryCall, ImportRef};

impl Graph {
    /// The value `decl` of `file` holds, where it is the result of calling a factory
    /// with a rule.
    ///
    /// An opaque file has no declarations to ask about, and so none is made by one.
    pub fn made_by(&self, file: FileId, decl: DeclId) -> Option<Made> {
        let fine = self.view(file).fine()?;
        let call = fine.module().decls.get(decl as usize)?.factory.as_ref()?;
        let rule = resolve::callee_rule(self, &fine, &call.callee, 0)?;
        Some(Made { rule, fine, decl })
    }
}

/// A declaration whose value a known factory made: `const t = createAsyncThunk(…)`,
/// however the app reached the factory.
///
/// It answers what the graph and the marks ask of such a value in terms of the
/// call as written, so that neither needs to know how a rule is laid out.
pub struct Made {
    rule: &'static Rule,
    fine: Fine,
    decl: DeclId,
}

impl Made {
    /// What creating the value reaches. See [`Creation`].
    pub fn creation(&self) -> Creation {
        self.rule.creation
    }

    /// Whether creating the value runs nothing but the factory, so that module
    /// initialisation reaches the frame and no argument.
    ///
    /// A factory that calls some of its arguments as it creates the value does so
    /// only where each of them is a function written out in place that the
    /// local-helper proof clears when called there. One the call does not pass
    /// would be called all the same, and calling nothing throws.
    pub fn creates_quietly(&self) -> bool {
        match self.creation() {
            Creation::Frame => true,
            Creation::Calls(called) => called.iter().all(|&index| {
                self.call()
                    .args
                    .get(index)
                    .is_some_and(|argument| argument.quiet_when_called)
            }),
            Creation::Whole => false,
        }
    }

    /// What each argument that creating the value calls depends on.
    pub fn called(&self) -> impl Iterator<Item = &Deps> {
        let called: &[usize] = match self.creation() {
            Creation::Calls(called) => called,
            Creation::Frame | Creation::Whole => &[],
        };
        let args = &self.call().args;
        called
            .iter()
            .filter_map(|&index| args.get(index))
            .map(|argument| &argument.deps)
    }

    /// What the declaration depends on outside every argument: the callee, and the
    /// call around the arguments.
    pub fn frame(&self) -> &Deps {
        &self.call().frame
    }

    /// What reading `name` off the value depends on, if the rule lists it: the
    /// arguments it names and the frame. Any other property depends on the whole
    /// declaration, which is the caller's to reach.
    pub fn member(&self, name: &str) -> Option<Deps> {
        let call = self.call();
        let args = self.rule.member(name)?;
        let mut deps = call.frame.clone();
        for &index in args {
            let Some(argument) = call.args.get(index) else {
                continue;
            };
            let argument = &argument.deps;
            deps.refs.extend(argument.refs.iter().copied());
            deps.member_refs
                .extend(argument.member_refs.iter().cloned());
            deps.imports.extend(argument.imports.iter().cloned());
        }
        Some(deps)
    }

    /// Every property the rule lets a reader reach on its own.
    pub fn member_names(&self) -> impl Iterator<Item = &'static str> + use<> {
        self.rule.member_names()
    }

    /// The properties an edit to the byte range `start..end` of the declaration
    /// touches.
    ///
    /// A property depends on the arguments its rule names and on the call around
    /// them, and on nothing in the other arguments. An edit anywhere in the
    /// statement outside the parentheses touches every one. An argument the call
    /// does not pass is touched by an edit where it would be written, which is
    /// where removing it leaves its mark.
    pub fn touched_members(&self, start: u32, end: u32) -> impl Iterator<Item = &'static str> {
        let (decl, call) = (self.decl(), self.call());
        let framing = !call.interior.holds_within(decl.span, start, end);
        self.rule
            .members
            .iter()
            .filter(move |(_, args)| {
                framing
                    || args.iter().any(|&index| match call.args.get(index) {
                        Some(argument) => argument.span.intersects(start, end),
                        None => call.missing.intersects(start, end),
                    })
            })
            .map(|(name, _)| *name)
    }

    /// Whether the call reads an import `reads` picks out, in any argument or
    /// around them.
    pub fn reads(&self, reads: impl Fn(&[ImportRef]) -> bool) -> bool {
        let call = self.call();
        reads(&call.frame.imports)
            || call
                .args
                .iter()
                .any(|argument| reads(&argument.deps.imports))
    }

    fn decl(&self) -> &Decl {
        &self.fine.module().decls[self.decl as usize]
    }

    fn call(&self) -> &FactoryCall {
        self.decl()
            .factory
            .as_ref()
            .expect("only a factory call is made by a factory")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::tests::{file, graph_for};

    /// What made `name` in `slice.ts`, in a tree of the given files.
    fn made(files: &[(&str, &str)], name: &str) -> Option<Made> {
        let (dir, graph) = graph_for(files);
        let slice = file(&graph, &dir, "slice.ts");
        let analysed = graph.analysis(slice).unwrap();
        let module = analysed.analysis.as_fine().unwrap();
        let decl = module.decl_named(name).expect(name);
        graph.made_by(slice, decl)
    }

    /// Whether `t` in `slice.ts` is made by a factory with a rule, in a tree of the
    /// given files.
    fn made_by_rule(files: &[(&str, &str)]) -> bool {
        made(files, "t").is_some()
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
            // Typed twice over, through a namespace.
            vec![(
                "slice.ts",
                "import * as rtk from '@reduxjs/toolkit';\nconst createAsyncThunk = rtk.createAsyncThunk.withTypes<{ state: unknown }>().withTypes();\n"
                    .to_string()
                    + THUNK,
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
            // RTK's identity form is RTK's: `withTypes` of another library's
            // function returns whatever that library says.
            vec![(
                "slice.ts",
                "import { createAsyncThunk as base } from 'another-library';\nconst createAsyncThunk = base.withTypes<{ state: unknown }>();\n"
                    .to_string()
                    + THUNK,
            )],
            // A file of the app that the package's name resolves to is that file,
            // and its function of the same name is not the factory.
            vec![
                (
                    "tsconfig.json",
                    "{ \"compilerOptions\": { \"paths\": { \"@reduxjs/toolkit\": [\"./shims/toolkit.ts\"] } } }\n"
                        .to_string(),
                ),
                (
                    "shims/toolkit.ts",
                    "export function createAsyncThunk(type: string, run: () => unknown) { run(); return run; }\n"
                        .to_string(),
                ),
                (
                    "slice.ts",
                    format!("import {{ createAsyncThunk }} from '@reduxjs/toolkit';\n{THUNK}"),
                ),
            ],
        ] {
            let files: Vec<(&str, &str)> = files
                .iter()
                .map(|(name, body)| (*name, body.as_str()))
                .collect();
            assert!(!made_by_rule(&files), "{files:?}");
        }
    }

    const OPTIONS: &str = "import { createAsyncThunk } from '@reduxjs/toolkit';
import { load } from './api';
import { serialize } from './errors';
const TYPE = 'a/b';
export const t = createAsyncThunk(TYPE, async () => load(), { serializeError: serialize });\n";

    /// The thunk `t` of a slice written as `source`, and the source indices of its
    /// imports from `./api` and `./errors`.
    fn thunk(source: &str) -> (Made, u32, u32) {
        let made = made(&[("slice.ts", source)], "t").expect("made by the factory");
        let sources = &made.fine.module().sources;
        let index = |name: &str| sources.iter().position(|s| s == name).unwrap() as u32;
        let (api, errors) = (index("./api"), index("./errors"));
        (made, api, errors)
    }

    #[test]
    fn a_member_depends_on_the_arguments_its_rule_names_and_the_frame() {
        let (made, api, errors) = thunk(OPTIONS);
        let reads = |deps: &Deps, source: u32| deps.imports.iter().any(|i| i.source == source);
        let type_decl = made.fine.module().decl_named("TYPE").unwrap();

        // The callee is an import, which is the frame's, and every member's.
        assert_eq!(made.frame().imports.len(), 1);
        let fulfilled = made.member("fulfilled").expect("a member the rule lists");
        assert!(fulfilled.refs.contains(&type_decl));
        assert!(fulfilled.imports.contains(&made.frame().imports[0]));
        assert!(!reads(&fulfilled, api));
        assert!(!reads(&fulfilled, errors));
        let rejected = made.member("rejected").expect("a member the rule lists");
        assert!(rejected.refs.contains(&type_decl));
        assert!(!reads(&rejected, api));
        assert!(reads(&rejected, errors));
        assert!(made.member("unwrap").is_none());

        assert_eq!(
            made.member_names().collect::<Vec<_>>(),
            ["pending", "fulfilled", "rejected", "settled", "typePrefix"]
        );
        assert_eq!(made.creation(), Creation::Frame);
        assert!(made.reads(|imports| imports.iter().any(|i| i.source == api)));
    }

    #[test]
    fn an_edit_touches_the_members_of_the_arguments_it_lands_in() {
        let (made, _, _) = thunk(OPTIONS);
        let at = |needle: &str| OPTIONS.find(needle).unwrap() as u32;
        let touched = |start: u32, end: u32| made.touched_members(start, end).collect::<Vec<_>>();
        let every = ["pending", "fulfilled", "rejected", "settled", "typePrefix"];

        // The payload creator is no member's.
        let payload = at("async");
        assert!(touched(payload, payload + 5).is_empty());
        // The type is every member's, and so is the statement around the call.
        let type_arg = at("TYPE,");
        assert_eq!(touched(type_arg, type_arg + 4), every);
        let binding = at("export const t");
        assert_eq!(touched(binding, binding + 6), every);
        // The options are `rejected`'s alone.
        let options = at("serializeError");
        assert_eq!(touched(options, options + 5), ["rejected"]);

        // Between the last argument and the closing parenthesis is where the options
        // would be written, and an edit there adds or removes them.
        let bare = "import { createAsyncThunk } from '@reduxjs/toolkit';
import { load } from './api';
import { serialize } from './errors';
export const t = createAsyncThunk(
  'a/b',
  async () => load(serialize)
);\n";
        let (made, _, _) = thunk(bare);
        let after = (bare.find("load(serialize)").unwrap() + "load(serialize)".len()) as u32;
        assert_eq!(
            made.touched_members(after, after + 1).collect::<Vec<_>>(),
            ["rejected"]
        );
    }

    /// What made `t` in `slice.ts`, where `slice.ts` is `source`.
    fn store(source: &str) -> Option<Made> {
        made(&[("slice.ts", source)], "t")
    }

    #[test]
    fn a_store_is_created_quietly_only_where_its_creator_is_proven_to_run_nothing() {
        let quiet = store(
            "import { create } from 'zustand';\nexport const t = create((set) => ({ count: 0, inc: () => set({ count: 1 }) }));\n",
        )
        .expect("made by the factory");
        assert_eq!(quiet.creation(), Creation::Calls(&[0]));
        assert!(quiet.creates_quietly());
        // Every use of a store can read all of it, so nothing is read apart.
        assert_eq!(quiet.member_names().count(), 0);
        assert!(quiet.member("getState").is_none());

        let loud = store(
            "import { create } from 'zustand';\nexport const t = create(() => ({ id: register() }));\n",
        )
        .expect("made by the factory");
        assert!(!loud.creates_quietly());

        // A thunk's payload creator is not called as the thunk is made, whatever
        // it runs.
        let thunk = store(
            "import { createAsyncThunk } from '@reduxjs/toolkit';\nexport const t = createAsyncThunk('a/b', () => register());\n",
        )
        .expect("made by the factory");
        assert!(thunk.creates_quietly());

        let other = "import { create } from 'another-library';\nexport const t = create(() => ({ count: 0 }));\n";
        assert!(store(other).is_none());
    }

    const CREATOR: &str = "(set) => ({ count: 0 })";

    #[test]
    fn a_store_is_made_by_create_called_first_with_no_arguments() {
        for source in [
            format!(
                "import {{ create }} from 'zustand';\nexport const t = create<{{ count: number }}>()({CREATOR});\n"
            ),
            format!(
                "import * as z from 'zustand';\nexport const t = z.create<{{ count: number }}>()({CREATOR});\n"
            ),
            format!(
                "import {{ create }} from 'zustand';\nconst typed = create<{{ count: number }}>();\nexport const t = typed({CREATOR});\n"
            ),
        ] {
            let made = store(&source).unwrap_or_else(|| panic!("{source}"));
            assert!(made.creates_quietly(), "{source}");
        }
    }

    #[test]
    fn every_way_zustand_offers_to_make_a_store_is_a_factory() {
        for import in [
            "import { create } from 'zustand/react';",
            "import { createStore as create } from 'zustand/vanilla';",
            "import { createStore as create } from 'zustand';",
            "import { createWithEqualityFn as create } from 'zustand/traditional';",
            // Zustand 4's default exports.
            "import create from 'zustand';",
            "import create from 'zustand/vanilla';",
        ] {
            for call in [
                format!("export const t = create({CREATOR});"),
                format!("export const t = create<{{ count: number }}>()({CREATOR});"),
            ] {
                let source = format!("{import}\n{call}\n");
                let made = store(&source).unwrap_or_else(|| panic!("{source}"));
                assert_eq!(made.creation(), Creation::Calls(&[0]), "{source}");
                assert!(made.creates_quietly(), "{source}");
            }
        }
        // The equality function is kept for the hook, and is not called as the store
        // is made.
        let source = format!(
            "import {{ createWithEqualityFn }} from 'zustand/traditional';\nexport const t = createWithEqualityFn({CREATOR}, (a, b) => register(a, b));\n"
        );
        assert!(store(&source).expect("made").creates_quietly());

        // A default Zustand does not export is no factory.
        let source = format!(
            "import create from 'zustand/traditional';\nexport const t = create({CREATOR});\n"
        );
        assert!(store(&source).is_none());
    }

    #[test]
    fn only_a_rules_own_identity_forms_return_its_factory() {
        for source in [
            // Twice with no arguments makes a store with no creator.
            format!("import {{ create }} from 'zustand';\nexport const t = create()()({CREATOR});\n"),
            // `withTypes` is RTK's, and Zustand's `create` has none.
            format!("import {{ create }} from 'zustand';\nexport const t = create.withTypes<{{ count: number }}>()({CREATOR});\n"),
            format!("import {{ create }} from 'zustand';\nconst typed = create.withTypes<{{ count: number }}>();\nexport const t = typed({CREATOR});\n"),
            // A call on something read off the factory.
            format!("import {{ create }} from 'zustand';\nexport const t = create.other()({CREATOR});\n"),
            // RTK's factory called with no arguments is not RTK's factory.
            "import { createAsyncThunk } from '@reduxjs/toolkit';\nexport const t = createAsyncThunk()('a/b', async () => 1);\n".to_string(),
            // A function of this file called with no arguments is not an import.
            format!("import {{ create }} from 'zustand';\nconst make = () => create;\nexport const t = make()({CREATOR});\n"),
            // Nor is another name for the factory, which is not read so far: the
            // call is plain initialisation, as it was.
            format!("import {{ create as base }} from 'zustand';\nconst create = base;\nexport const t = create<{{ count: number }}>()({CREATOR});\n"),
        ] {
            assert!(store(&source).is_none(), "{source}");
        }
    }
}
