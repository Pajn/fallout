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
//! calling it there runs nothing; see [`Creation::Calls`]. A middleware that only
//! wraps the creator, such as `create(immer(creator))`, is looked through; see
//! [`rules::WRAPPERS`].
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
        let wrappers_are_zustands = call
            .args
            .iter()
            .flat_map(|argument| &argument.wrappers)
            .all(|&source| resolve::app_file(self, &fine, source).is_none());
        Some(Made {
            rule,
            fine,
            decl,
            wrappers_are_zustands,
        })
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
    /// Whether every middleware an argument is proven quiet through is Zustand's,
    /// rather than a file of the app its import lands in.
    wrappers_are_zustands: bool,
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
    ///
    /// A Zustand middleware around such a function, as in `create(immer(creator))`,
    /// is looked through where it is Zustand's own: where its import lands in a
    /// file of the app, the function called is that file's.
    ///
    /// With `inline_requires`, reading an import evaluates the module it names, so
    /// an argument that may read one is not quiet either. Initialisation then
    /// reaches the whole declaration, and what it reads is followed as any read is.
    /// Calling a middleware reads its import, so a store wrapped in one is never
    /// made quietly there.
    pub fn creates_quietly(&self, inline_requires: bool) -> bool {
        match self.creation() {
            Creation::Frame => true,
            Creation::Calls(called) => {
                self.wrappers_are_zustands
                    && called.iter().all(|&index| {
                        self.call().args.get(index).is_some_and(|argument| {
                            argument.quiet_when_called
                                && !(inline_requires && argument.reads_imports)
                        })
                    })
            }
            Creation::Whole => false,
        }
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

    /// Whether calling `method` on the value, in the way its form says, reads it
    /// and changes none of it. See [`rules::Reads`].
    pub fn read_by(&self, method: &str) -> bool {
        self.rule.reads(method)
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
    use crate::graph::Node;
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
        assert!(quiet.creates_quietly(false));
        // Every use of a store can read all of it, so nothing is read apart.
        assert_eq!(quiet.member_names().count(), 0);
        assert!(quiet.member("getState").is_none());

        let loud = store(
            "import { create } from 'zustand';\nexport const t = create(() => ({ id: register() }));\n",
        )
        .expect("made by the factory");
        assert!(!loud.creates_quietly(false));

        // Where imports are deferred to first use, reading one evaluates its module,
        // whether the creator reads it or a helper it calls does. A creator that
        // reads none is quiet either way.
        assert!(quiet.creates_quietly(true));
        for source in [
            "import { create } from 'zustand';\nimport { base } from './tokens';\nexport const t = create(() => ({ base }));\n",
            "import { create } from 'zustand';\nimport { base } from './tokens';\nfunction initial() { return { base }; }\nexport const t = create(() => initial());\n",
        ] {
            let made = store(source).expect("made by the factory");
            assert!(made.creates_quietly(false), "{source}");
            assert!(!made.creates_quietly(true), "{source}");
        }

        // A thunk's payload creator is not called as the thunk is made, whatever
        // it runs.
        let thunk = store(
            "import { createAsyncThunk } from '@reduxjs/toolkit';\nexport const t = createAsyncThunk('a/b', () => register());\n",
        )
        .expect("made by the factory");
        assert!(thunk.creates_quietly(false));

        let other = "import { create } from 'another-library';\nexport const t = create(() => ({ count: 0 }));\n";
        assert!(store(other).is_none());
    }

    #[test]
    fn a_store_wrapped_in_a_quiet_middleware_is_created_quietly_where_imports_load_up_front() {
        let made = store(
            "import { create } from 'zustand';\nimport { immer } from 'zustand/middleware/immer';\nexport const t = create(immer((set) => ({ count: 0 })));\n",
        )
        .expect("made by the factory");
        assert!(made.creates_quietly(false));
        // The middleware is called as the store is made, as the factory is, so
        // initialisation reaches it with the frame.
        assert_eq!(made.frame().imports.len(), 2);
        // Calling it reads its import, which evaluates the module it names where
        // imports are deferred to first use.
        assert!(!made.creates_quietly(true));
    }

    #[test]
    fn a_middleware_from_a_file_of_the_app_is_not_zustands() {
        let files = [
            (
                "tsconfig.json",
                "{ \"compilerOptions\": { \"paths\": { \"zustand/middleware\": [\"./shims/middleware.ts\"] } } }\n",
            ),
            (
                "shims/middleware.ts",
                "export function combine(initial: object, f: () => object) { register(); return f; }\n",
            ),
            (
                "slice.ts",
                "import { create } from 'zustand';\nimport { combine } from 'zustand/middleware';\nexport const t = create(combine({ count: 0 }, () => ({})));\n",
            ),
        ];
        let made = made(&files, "t").expect("made by the factory");
        assert!(!made.creates_quietly(false));
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
            assert!(made.creates_quietly(false), "{source}");
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
                assert!(made.creates_quietly(false), "{source}");
            }
        }
        // The equality function is kept for the hook, and is not called as the store
        // is made.
        let source = format!(
            "import {{ createWithEqualityFn }} from 'zustand/traditional';\nexport const t = createWithEqualityFn({CREATOR}, (a, b) => register(a, b));\n"
        );
        assert!(store(&source).expect("made").creates_quietly(false));

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

    /// Whether a read of `useCounter` reaches `peek`, in a `slice.ts` that imports
    /// what `import` says, makes the store with `make`, and declares `peek` as
    /// `body`, among the given other files. Both a reader in the same file and a
    /// reader in another module, landing on the export, are asked, and must agree.
    fn peek_writes(files: &[(&str, &str)], import: &str, make: &str, body: &str) -> bool {
        peek_writes_in("slice.ts", files, import, make, body)
    }

    /// As [`peek_writes`], with the store's file named `name`, so that `body` may
    /// be JSX.
    fn peek_writes_in(
        name: &str,
        files: &[(&str, &str)],
        import: &str,
        make: &str,
        body: &str,
    ) -> bool {
        let source = format!(
            "{import}\nexport const useCounter = {make}(() => ({{ count: 0, inc: () => {{}} }}));\nexport const read = () => useCounter((state) => state.count);\nexport const peek = () => {body};\n"
        );
        let mut files = files.to_vec();
        files.push((name, &source));
        let (dir, graph) = graph_for(&files);
        let slice = file(&graph, &dir, name);
        let module = graph.view(slice).fine().unwrap();
        let decl = |name: &str| module.module().decl_named(name).expect(name);
        let peek = Node::Decl(slice, decl("peek"));
        let local = graph.edges(Node::Decl(slice, decl("read"))).contains(&peek);
        let exported = graph
            .edges(Node::Export(slice, graph.name_id("useCounter")))
            .contains(&peek);
        assert_eq!(local, exported, "{source}");
        local
    }

    const ZUSTAND: &str = "import { create } from 'zustand';";

    #[test]
    fn reading_a_stores_state_or_subscribing_to_it_does_not_write_it() {
        for body in [
            "useCounter.getState().count > 0",
            "useCounter.getInitialState().count > 0",
            "useCounter.getState()['count'] === 1",
            "(useCounter.getState() as { count: number }).count + 1",
            "useCounter.getState() === undefined",
            "`${useCounter.getState().count}`",
            "{ if (useCounter.getState().count > 1) { document.title = 'many'; } }",
            "{ const { count } = useCounter.getState(); return count > 0; }",
            "{ const count = useCounter.getState().count; return -count; }",
            "{ const state = useCounter.getState(); return state.count > 0; }",
            "useCounter.subscribe((state) => { document.title = `${state.count}`; })",
            "useCounter.subscribe(function (state, previous) { return state.count === previous.count; })",
            "useCounter.subscribe(({ count }) => { document.title = `${count}`; })",
            "useCounter.subscribe(() => {})",
            // The function `subscribe` returns unsubscribes, and changes no state.
            "{ const stop = useCounter.subscribe(() => {}); stop(); }",
        ] {
            assert!(!peek_writes(&[], ZUSTAND, "create", body), "{body}");
        }
    }

    #[test]
    fn state_rendered_by_an_element_is_read_and_handed_to_a_component_is_passed() {
        let writes = |body: &str| peek_writes_in("slice.tsx", &[], ZUSTAND, "create", body);
        for body in [
            "<li>{useCounter.getState().count}</li>",
            "<>{useCounter.getState().count}</>",
            "<li key={useCounter.getState().count}>x</li>",
            "<Row key={useCounter.getState().count} />",
            "{ const { count } = useCounter.getState(); return <li>{count}</li>; }",
            "useCounter.subscribe((state) => { render(<li>{state.count}</li>); })",
        ] {
            assert!(!writes(body), "{body}");
        }
        for body in [
            // A component is handed its children as a prop, and may call them.
            "<Foo>{useCounter.getState().inc}</Foo>",
            "<Foo>{useCounter.getState().count}</Foo>",
            "<Foo.Bar>{useCounter.getState().inc}</Foo.Bar>",
            "{ const { inc } = useCounter.getState(); return <Foo>{inc}</Foo>; }",
            "{ const state = useCounter.getState(); return <Foo>{state.inc}</Foo>; }",
            "useCounter.subscribe((state) => { render(<Foo>{state.inc}</Foo>); })",
            // Any prop but `key` is handed on, even to an element of the
            // platform's own, which may call it as a handler.
            "<Foo value={useCounter.getState().count} />",
            "<div title={useCounter.getState().count} />",
            "<div onClick={useCounter.getState().inc} />",
            // The state itself is never rendered in place.
            "<li>{useCounter.getState()}</li>",
        ] {
            assert!(writes(body), "{body}");
        }
    }

    #[test]
    fn every_zustand_store_is_read_by_its_read_methods_however_its_factory_is_reached() {
        let body = "useCounter.getState().count > 0";
        for (import, make) in [
            (
                "import { createStore } from 'zustand/vanilla';",
                "createStore",
            ),
            (
                "import { createWithEqualityFn } from 'zustand/traditional';",
                "createWithEqualityFn",
            ),
            ("import create from 'zustand';", "create"),
            ("import * as z from 'zustand';", "z.create"),
            (
                "import { create } from 'zustand';\nconst typed = create<{ count: number }>();",
                "typed",
            ),
        ] {
            assert!(!peek_writes(&[], import, make, body), "{import}");
        }
        // Through a module of the app that re-exports the factory.
        let files = [("zustand.ts", "export { create } from 'zustand';\n")];
        assert!(!peek_writes(
            &files,
            "import { create } from './zustand';",
            "create",
            body
        ));
    }

    #[test]
    fn acting_on_what_a_store_hands_out_still_writes_it() {
        for body in [
            // An action on the state calls `set`.
            "useCounter.getState().inc()",
            "{ const state = useCounter.getState(); state.inc(); }",
            "{ const { inc } = useCounter.getState(); inc(); }",
            "{ const inc = useCounter.getState().inc; inc(); }",
            "register(useCounter.getState())",
            "register(useCounter.getState().inc)",
            "useCounter.getState().count = 1",
            "{ const { inc } = useCounter.getState(); register(inc); }",
            // Anything taken from the state and returned may be an action, which the
            // caller is free to call.
            "useCounter.getState()",
            "useCounter.getState().inc",
            "useCounter.getState().count",
            "useCounter.getInitialState().count",
            "{ return useCounter.getState().inc; }",
            "{ const { inc } = useCounter.getState(); return inc; }",
            "{ const count = useCounter.getState().count; return count; }",
            "{ const state = useCounter.getState(); return state.inc; }",
            "({ inc: useCounter.getState().inc })",
            "[...useCounter.getState().list]",
            // Any other method, and a read method not called.
            "useCounter.setState({ count: 1 })",
            "useCounter.destroy()",
            "useCounter.other()",
            "register(useCounter.getState)",
            // A listener that writes, or that this cannot read.
            "useCounter.subscribe((state) => state.inc())",
            "useCounter.subscribe((state) => register(state))",
            "useCounter.subscribe(() => useCounter.setState({ count: 1 }))",
            "useCounter.subscribe(() => useCounter.getState().inc())",
            "useCounter.subscribe(register)",
            "useCounter.subscribe((state) => state.count, (count) => register(count))",
            "useCounter.subscribe((...args) => args)",
        ] {
            assert!(peek_writes(&[], ZUSTAND, "create", body), "{body}");
        }
    }

    #[test]
    fn a_declaration_that_reads_a_store_is_still_reached_by_what_names_it_and_still_reads_its_writers()
     {
        let source = "import { create } from 'zustand';
export const useCounter = create(() => ({ count: 0 }));
export const peek = () => useCounter.getState().count > 0;
export const show = () => peek() + useCounter.getState().count;
export const reset = () => useCounter.setState({ count: 0 });\n";
        let (dir, graph) = graph_for(&[("slice.ts", source)]);
        let slice = file(&graph, &dir, "slice.ts");
        let module = graph.view(slice).fine().unwrap();
        let decl = |name: &str| Node::Decl(slice, module.module().decl_named(name).expect(name));
        let show = graph.edges(decl("show"));
        assert!(show.contains(&decl("peek")));
        assert!(show.contains(&decl("reset")));
        assert!(graph.edges(decl("peek")).contains(&decl("reset")));
        let exported = graph.edges(Node::Export(slice, graph.name_id("useCounter")));
        assert!(exported.contains(&decl("reset")));
        assert!(!exported.contains(&decl("peek")));
        assert!(!exported.contains(&decl("show")));
    }

    #[test]
    fn the_same_methods_on_anything_but_a_zustand_store_still_write_it() {
        let body = "useCounter.getState().count > 0";
        // Another library's `create`.
        assert!(peek_writes(
            &[],
            "import { create } from 'another-library';",
            "create",
            body
        ));
        // The app's own `create`.
        let files = [(
            "make.ts",
            "export function create(f: () => object) { const state = f(); return { getState() { register(); return state; } }; }\n",
        )];
        assert!(peek_writes(
            &files,
            "import { create } from './make';",
            "create",
            body
        ));
        // A file of the app that Zustand's name resolves to.
        let files = [
            (
                "tsconfig.json",
                "{ \"compilerOptions\": { \"paths\": { \"zustand\": [\"./shims/zustand.ts\"] } } }\n",
            ),
            (
                "shims/zustand.ts",
                "export function create(f: () => object) { const state = f(); return { getState() { register(); return state; } }; }\n",
            ),
        ];
        assert!(peek_writes(&files, ZUSTAND, "create", body));
        // A local object with methods of the same names.
        let class = "class Counter { constructor(f: () => object) {} getState() { register(); return {}; } }";
        assert!(peek_writes(&[], class, "new Counter", body));
        // RTK's factory makes no store.
        assert!(peek_writes(
            &[],
            "import { createAsyncThunk } from '@reduxjs/toolkit';",
            "createAsyncThunk",
            body
        ));
    }
}
