# fallout

Decide whether a set of changed files can reach a page, by walking the JavaScript and
TypeScript import graph.

Given one or more *anchors* (say, the page component behind an end-to-end test) and the
files a commit touched, `fallout` answers one question: is this anchor affected? It exits
`0` when it is and `1` when it is not, so a CI job can use it to skip test suites that the
change provably cannot influence.

> **Status: placeholder release.** The name is reserved and the tool works, but the
> interface is not yet stable. Pin an exact version.

## Install

```sh
npm install --save-dev fallout-cli
```

The package carries a prebuilt binary per platform and installs only the one the
machine needs: Linux and Windows on x64, Linux and macOS on arm64. Elsewhere, and
to build from source:

```sh
cargo install fallout
```

The command is `fallout` either way.

## Usage

```sh
fallout --anchor src/pages/CheckoutPage.tsx --changed src/components/Button.tsx

git diff -U3 main... | fallout --anchor src/pages/CheckoutPage.tsx --diff -
```

| Flag | Description |
| --- | --- |
| `-a, --anchor <PATH>` | Target component. Repeatable; the anchor set is affected if *any* anchor is. |
| `-c, --changed <PATH>` | A changed file, as produced by `git diff --name-only`: a relative path is relative to `--root`. Repeatable. |
| `-d, --diff <PATH>` | A unified diff describing the change; `-` reads standard input. |
| `-b, --base <REV>` | Git revision to compare each changed file against. See below. |
| `--include-types` | Read every file as written, so a type-only change still counts. See below. |
| `-r, --root <PATH>` | Root directory to resolve from. Defaults to the current directory. |
| `-o, --only <DIRECTION>` | Search only `downstream` or `upstream` instead of both. |
| `-g, --granularity <LEVEL>` | `file` (default) or `symbol`. See below. |
| `-e, --explain` | Print the chain of imports that produced the verdict. |
| `--unresolved` | List the specifiers this run reached and could not place on disk. See below. |
| `--json` | Answer each anchor on its own, in one run, as JSON. See below. |

`--diff` and `--changed` may be combined; their file sets are unioned. A diff is read
as text rather than by shelling out, so the tool needs no git checkout at runtime;
`--base` is the one flag that does ask git, and only for the files already named.
Files the change deletes are dropped: they have no after version to reach.

Exit codes: `0` — affected, run the tests. `1` — not affected. `2` — no answer: the
arguments were invalid, an anchor is not there, a diff, a `fallout.toml` or a
tsconfig could not be read, or git finds no commit by the `--base` revision from the root. Errors are
written to stderr. With `--json` the answers are in the output,
so the exit code is `0` for any answer and `2` for none.

### One answer per anchor

`--json` answers every anchor on its own rather than the set as a whole, in one run.
Anchors whose apps share a bundler share one graph, so a module is read and resolved
once however many anchors reach it.

```sh
git diff -U3 main... | fallout --diff - --granularity symbol --json \
  --anchor src/pages/CheckoutPage.tsx --anchor src/pages/SettingsPage.tsx
```

```json
{"anchors": [
  {"anchor": "src/pages/CheckoutPage.tsx", "affected": true, "direction": "downstream",
   "changed": "src/components/Button.tsx", "path": ["File(src/pages/CheckoutPage.tsx)", "…"],
   "granularity": "symbol", "unresolved": []},
  {"anchor": "src/pages/SettingsPage.tsx", "affected": false, "granularity": "symbol",
   "unresolved": [{"specifier": "~/theme", "kind": "alias", "in_repo": true,
                   "from": ["src/pages/SettingsPage.tsx"]}]}
]}
```

Each answer lists the specifiers its own search reached and could not place on disk
(see [Unresolved imports](#unresolved-imports)). For an anchor found not affected that
is every place a missing edge could have hidden a change from it, so a caller can treat
an in-repo gap as a reason to run the anchor anyway. Each specifier is classed:

- `path` — a relative or absolute path to a file that is not there;
- `alias` — a name the project maps to its own files: a `fallout.toml` alias, a
  `package.json` `#import`, or a `tsconfig.json` `paths` entry or `baseUrl`;
- `package` — a package that is installed or is one of the repository's own, where
  nothing it offers matches the bundler's [`[resolve]`](#package-entry-points)
  settings, or which the workspace has not linked;
- `missing-package` — a package that is not installed.

`in_repo` is true for the first three. In a stylesheet a bare name is a sibling file
first, so it is a `path`, and a `~` names a package. A specifier several files write
is as in-repo as the most in-repo of them says, and only what the anchor's own
bundler could not place is listed.

### Direction

By default `fallout` searches both directions and stops at the first hit.

- `downstream` — the anchor imports the changed file, directly or transitively.
- `upstream` — the changed file imports the anchor, directly or transitively.

### Granularity

`--granularity file`, the default, treats any change anywhere in a file as a change to
the whole file. `--granularity symbol` attributes a change to the individual
declarations, exports and module initialisation it touches, so an edit to a function
nobody imports stops marking its whole file.

```
src/utils/helpers.ts
  export const formatDate = ...   # the page imports this
  export const formatPrice = ...  # edited
```

A page importing only `formatDate` is affected at `file` granularity and not at
`symbol`.

Narrowing never costs a true positive. Two declarations sharing a module-scope binding
that is not a `const` bound to a primitive literal stay connected when one of them
could change what the binding holds, because that is how an edit to one travels to the
other without either naming it. Calling it, constructing with it, rendering it as
`<S />`, asking `typeof`, and reading a property in place cannot change it, so
declarations that only do those stay apart. Nor can checking it with `in`,
`instanceof` or a comparison (`===`, `!=`, `<`, `>=` and the rest), on either side:
that reads the value and does not write it. What a check can run, a proxy trap,
`Symbol.hasInstance`, `valueOf` or `toString`, is taken to run nothing, as it is
below. Everything else — passing it to a function, returning it, writing through it,
spreading it, arithmetic on it, or naming a member of it as an element, which is how
a React context is written — counts as a write. So does calling a method on it,
except a method that reads a [Zustand store](#zustand) or a collection whose type the
file shows. What a property holds is part of the value, so a write or a method call
anywhere down a member chain writes the value too: `state.a.b = 1`, `state.a[k]++`,
`delete state.a.b` and `state.items.push(x)` write `state` as `state.a = 1` does,
while `state.items.length` still reads it in place.

A collection's type is shown only by a module-scope `const` that nothing reassigns,
initialised in the same file with `new Map(…)`, `new Set(…)`, `new WeakMap(…)` or
`new WeakSet(…)` of the globals, not a class of the file or an import of the same
name, or with an array literal such as `[]` or `[a, b]`. Called directly on it, or on
a local `const` alias of it, these methods read it and change none of it:

- `Map` and `WeakMap`: `get` and `has`, and on a `Map` also `forEach`, `keys`,
  `values` and `entries`. `size` is a property, read in place like any other.
- `Set` and `WeakSet`: `has`, and on a `Set` also `forEach`, `keys`, `values` and
  `entries`.
- Arrays: `at`, `concat`, `entries`, `every`, `filter`, `find`, `findIndex`,
  `findLast`, `findLastIndex`, `flat`, `flatMap`, `forEach`, `includes`, `indexOf`,
  `join`, `keys`, `lastIndexOf`, `map`, `reduce`, `reduceRight`, `slice`, `some`,
  `toReversed`, `toSorted`, `toSpliced`, `values`, `with` and `toString`.

A method that returns a boolean, a number or a string, such as `has`, `includes`,
`indexOf`, `some` or `join`, is a read however its result is used. One whose result
can be, or hold, an object the collection holds — `get`, `find`, `at`, `filter`,
`map`, `slice`, `reduce`, an iterator from `keys`, `values` or `entries`, and the
rest — is a read only where that result is used in place: compared, put through
arithmetic or into an untagged template, tested, rendered as the child of an element
such as `<li>` or of a fragment, used as a `key`, or read through a local `const` or
destructuring used the same way. Returning it, passing it on, storing it, spreading
it, iterating it with `for…of`, handing it to a component, or calling a method on it
writes the collection, since the code it reaches could change what the collection
holds. A callback, as `forEach`, `map` and `find` take, must be an arrow written out
in place that does nothing with what it is handed but read it the same way; one
passed by name, or a `function`, writes the collection, as nothing else links it to
the elements it can change. What a callback's body does to the collection through its
own name counts where it is written, so `list.forEach((x) => list.push(x))` still
writes. Every other method writes, `set`, `delete`, `clear`, `add`, `push`, `pop`,
`shift`, `unshift`, `splice`, `sort`, `reverse`, `fill` and `copyWithin` among them.
So does every method of a `let` or `var`, of a value whose type the file does not
show, and of one whose methods could be replaced: anything written through it but an
array's index or `length`, handing it to `Object` or `Reflect`, as
`Object.defineProperty` does, or using its constructor other than with `new`,
`instanceof` or a call of the constructor's own functions such as `Array.isArray`,
since that could reach its `prototype`. A write
through an element, as in `list[0].count = 1`, writes the collection. The built-in
methods are taken to be the standard ones, and a collection handed to other code is
taken to keep them.

A binding initialised with a plain object literal, frozen or not, which nothing
reassigns and which has no getter, setter or prototype-setting `__proto__: value`, is
shared property by property instead. A write to `state.theme` reaches the declarations
that use `state.theme`, and not one that only reads `state.volume`. A write is found
however deep the chain goes, so `state.items.push(x)` writes `items`. A `const` alias
of the object or of one property is followed to its own uses, and a write through it
counts as a write in the declaration where it is written. Anything that cannot be
pinned to one property uses the whole object and meets every property: calling a
method on it, passing it anywhere, a computed key, `__proto__`, an exported or
reassignable alias, destructuring it, and aliases nested more than four deep. A write
through one of those aliases still counts in the declaration where it is written.

A read from another module of a value its own file exports and writes reaches the
declarations of that file that write it. Exporting a value is no use of it, so given
`export const cache = new Map()` and `export function reset() { cache.set("a", 1) }`,
nothing else would take a page that reads `cache` to an edit of `reset`. This holds
however the page reaches the name: imported directly, from an `export { }` list under
any name, through a barrel, or off a namespace. Of an object read only for its members,
a page reading `utils.items` reaches the writers of `items` and of the whole object, and
not one that writes only another property. Loading the file does not reach the
writers, so a page that imports only an unrelated export of that file stays apart.

A declaration whose initialiser may run something — a call, a `new`, an `await`, a
tagged template, a write — belongs to module initialisation, so importing anything
from that file reaches it. It reaches that declaration and what it reads, not the rest
of the file. A write is an assignment to a member, any `++` or `--`, any `delete`, and
an assignment to a name that is not a `let`, `var`, function or class declared at the
top of the same file: a name nothing declares is a property of the global object, and
assigning an import or a `const` throws. A destructuring pattern's defaults and
computed keys run with the initialiser, so the call in `const { a = register() } =
options` counts. So does naming a `let`, a `const` or a class before its declaration
has run, as `export const v = [limit]` above `const limit = 10` does, since that
throws. Naming one in a function body does not count, since the body runs only when
it is called, and a call on load is judged like any other. For a class, what counts
is what runs when the class is defined: its `extends` expression, its computed keys,
and its static fields and blocks, but not instance fields or method bodies. An enum's
member initialisers count too. A bare `import "./theme.css"` is a side effect of
loading the module and reaches every importer, while `import logo from "./logo.png"`
reaches only the declarations using `logo`. A top-level statement that declares
nothing and reads an import, such as `document.title = String(base)`, links the
file's initialisation to that export.

Some things an initialiser does are taken to run nothing, although they can. Reading a
property, destructuring, spreading, iterating, `in` and `instanceof` are assumed not
to run a getter, an iterator or a proxy. That is a deliberate trade-off: nearly every
initialiser does one of them, and counting each would put nearly every declaration in
module initialisation, leaving `symbol` little narrower than `file`. So a getter or a
proxy with an effect on load can be missed at `--granularity symbol`. It is never
missed at `file`.

A `require("./x")` is an ordinary dependency of the declaration that contains it. It
yields the whole export object, but static member reads such as
`require("./x").name`, destructuring such as `const { name } = require("./x")`, and
bindings used only for known members select those exports. Escapes, computed keys,
reassignment, writes through the object, and shadowed `require` calls keep every
export. Unused module bindings and empty patterns also retain the whole dependency
so evaluation is preserved, including with `inline-requires`. A `require` outside
every declaration runs on evaluation, like a bare import. CommonJS producers using
`this` stay coarse because a method's receiver may expose sibling exports.

A module object read for a single name depends on that name alone. `ns.fetchUser`
from an `import * as ns` targets that one export rather than the whole table, and so
do `<ns.Thing />`, `const m = await import("./g"); m.x`, `(await import("./g")).x`,
`import("./g").then(m => m.x)`, and `const { x } = await import("./g")`. Handing the
module object anywhere else — passing it on, reading `ns[key]` — keeps the whole
table, and a binding that does both keeps the whole table.

An object literal is read the same way. Given
`export const utils = { formatDate, formatPrice }`, a page calling
`utils.formatDate()` depends on `formatDate` and not on `formatPrice`. That holds
however the property is reached: off `import { utils }`, off `ns.utils` from a
namespace, off `require("./u").utils` or a module object bound from `require` or
`import()`, off a binding destructured from one, or from another declaration in the
same file. The object can be a `const` bound directly to the literal (optionally
through `as const`, `satisfies` or `Object.freeze`), an `export default { … }`, or a
CommonJS `exports.utils = { … }`. An edit inside one property's value marks that property
alone, with or without a base revision.

This only applies while nothing can change what a property holds. In the file that
declares it, the object must be used only by reading a property or by exporting it,
whether as `export { utils }`, `export { utils as default }` or
`export default utils`. A property must be a plain value or method under a fixed key.
A spread, a computed key, a getter or setter, a `__proto__: value` that sets the
prototype, a duplicate key, and a `require` or `import()` inside it all keep the
object whole. So does a write through it, passing it on, reading `utils[key]`, or
destructuring it. The shorthand `{ __proto__ }` is an ordinary property. A consumer
that writes through a property, uses the object as a whole, or re-exports a binding
destructured from it depends on every property. Two properties that share a binding of
their file, where one of them could write it, still reach each other, and so do two
declarations of the file where one hands a property to other code.

Calling a property as `utils.fn()` hands `fn` the object as `this`, through which it
can reach every other property. A property whose value may use it — a method or a
local function that reads `this`, a function imported from elsewhere, a call's
result, anything not visibly a literal, an arrow, a class or a local function that
never reads `this` — depends on the whole object. Calling any other property reads
that property and leaves the object as it was.

### Pure calls

A declaration whose initialiser runs something belongs to module initialisation, so
importing anything from its file reaches it. What that costs is the declaration and
what it reads, not the rest of the file: an importer is affected by an edit to the
declaration or to something feeding it, which is what could change what running it
does, and not by an edit anywhere else in the module. In React-shaped code that
catches nearly every top-level declaration, because nearly every one of them is a
call, and so everything each of them reads.

Six things take a call back out of initialisation:

- a `/* @__PURE__ */` annotation, the author of the call site saying it has no side
  effects;
- React's own factories — `memo`, `forwardRef`, `createContext`, `lazy` — which are
  built in;
- `Object.freeze` of an object or array literal written in place, which is how enums
  and constant tables are usually written, provided `Object` is the global;
- the language's own functions that have no side effects: `new Map()` and `new Set()`
  (empty, or filled from an array literal), `Math.*`, `Number.is*`, `parseInt`,
  `String()`, `Array.isArray`, `Object.is`, `Date.now()`, `new Error("…")`,
  `console.log()` and a few more, taken from oxc's side-effect analysis. A side effect
  here means a change other code in the app could read back. So a result read from the
  clock or a random source is fine, since nothing requires the call to return the same
  thing every time, and so is writing to the console, which the app never reads.
  `toString` and `valueOf` are taken to have none either, as getters, iterators and
  proxies are (see [Granularity](#granularity)). What such a call must not do
  is throw, since it runs in the module body: an argument converted to a number must
  not be a BigInt, an object literal converted must not name its own `toString`,
  `valueOf` or `__proto__`, since converting it would then run whatever those hold,
  an array literal converted has each of its elements converted in turn, functions
  that throw on some literals — `decodeURI`, `String.fromCodePoint`, `new Array(n)`
  — are not included, and a `console` call given
  several arguments must start with a literal holding no `%`, since a format string
  can convert what follows in ways that throw. `Symbol.for`, which adds to the global
  symbol registry, is not included either;
- entries in the project's `fallout.toml`;
- a proof for a small local helper.

Local inference covers top-level function declarations whose binding is never
reassigned or redeclared, and `const` bindings of arrows and function expressions,
with simple parameters. A body is a run of `const` locals, `if` statements that
return, statements such as `console.log(value);` that are one of the expressions
below, and a final return. It accepts literals (including negative numbers and
templates with no interpolation), reads of parameters and locals, reads of top-level
`const` primitives and of functions, direct reads of imported bindings, plain array and
object construction, conditionals, logical operators, strict equality, `Object.freeze`
of a literal, the global functions above, and calls to other proven helpers. For
example, `const make = (value) => ({ value })` makes `const item = make("item")`
independent of unrelated exports. Consumers of `item` still depend on `make` and any
helpers it calls.

Creating a function — an arrow, a function expression, or a method, getter or setter
in an object literal, async and generator ones included — is taken to run nothing,
since its body, its defaults and every read in it wait until it is called. So
`(name) => ({ name, rename: (next) => save(next) })` is proven whatever `rename`
does, as a store's creator returning its actions is, and so is a function that reads
a `const` declared further down. Calling one is judged as any call is, and a function
a helper returned or one written in place is not a proven helper, so `make().rename()`
and `(() => register())()` stay in initialisation, and so does a call that reads a
`const` before it exists. A computed key still runs where the object is created, and
is not proven. A class expression is not a plain value, since defining one runs its
heritage and static members (see [Granularity](#granularity)).

A helper may also call what the module body may: a callee named by a `pure` entry and
a call annotated `/* @__PURE__ */`. An entry is matched as it is everywhere else, by
the import binding the callee is reached from and only where the entry applies to the
file (see [Where a claim applies](#where-a-claim-applies)), so a parameter or a local
called `memo` is not covered. Both are claims rather than proofs, and inside the proof
they are trusted not to throw, as bundlers trust them when they drop such a call. The
claim covers the whole call, reaching its callee included, so `/* @__PURE__ */
base.method()` is trusted as it is in the module body. They clear the call and nothing
more: its arguments must still be proven, so `memo(value)` qualifies and
`memo(register())` does not.

A helper may read an imported binding directly, as the module body may, so given
`import { base } from "./tokens"`, `(n) => ({ base, n })` is proven. The binding is
matched by what it resolves to, so a parameter or a local named `base` is not the
import. Reading a member through an import, as `theme.colors.primary` or `ns.value`
does, is not proven inside a helper, and neither is calling an import unless a claim
above clears it. An imported value is not known to be a primitive, so it is not
proven where it would be converted, as in `String(base)`. A read during an import
cycle that would throw, because the imported module has not run yet, goes unseen, as
it does in the module body (see [Known gaps](#known-gaps)).

The proof also checks argument evaluation. Literals, top-level `const` primitives,
direct reads of imports, functions created in place and calls to proven helpers
qualify; other variable arguments remain conservative. A throw is not a side effect, but a throw in the module body stops
the module loading, and every importer with it. A helper is only ever proven on behalf
of a call written in the module body, which runs its body there, so nothing the call
evaluates may throw, in the helper or out of it, beyond the calls a claim above is
trusted for. A value converted to a number must not be a BigInt, and a parameter may
not be converted at all, since it could be one, or a symbol. Functions
that throw on some arguments — `decodeURI("%")`, `String.fromCodePoint(-1)`,
`new Array(-1)`, a `WeakMap` or `WeakSet` given a primitive key — are not proven. And
order matters: a `const` does not exist until its declaration runs, and calling or
reading one earlier throws, so a call written before a `const` helper, or before a
`const` it or a helper reads, stays in initialisation. That puts the call and what it
reads in front of every importer, which is what could change whether it throws, and
nothing else in the module. Function declarations are hoisted and can be called from
anywhere. Reads of other captured values, property reads (which may invoke getters),
coercing arithmetic, interpolated templates, writes, loops, unknown calls, recursion,
defaults, destructuring, spread, `this`, `arguments`, async and generator helpers,
and default-exported declarations keep the existing broad behaviour. No annotation or
configuration is needed for an inferred helper. Use `--base` to detect an effect
removed from a helper: line-only analysis sees its current body, not the previous side
effect.

```toml
# fallout.toml
pure = [
  "react-native#StyleSheet.create",
  "app/graphql#graphql",
]

# Drop the built-in React entries.
builtin-pure = false
```

The same file carries `inline-requires`, under [Inline requires](#inline-requires),
`[aliases]`, under [Import aliases](#import-aliases), and `[style.aliases]`, under
[Stylesheets](#stylesheets). Where it sits decides who it
speaks for — see [Where a claim applies](#where-a-claim-applies).

An entry is written as the import source, `#`, and the path taken from the binding
that import introduces. The first segment is the name the target exports, with
`default` and `*` for the two unnamed forms, so `React.memo` is `react#default.memo`.
An entry is honoured only where the callee is reached from the import it names: a
local function called `memo` is not covered by React's entry. That includes a local
declared inside a function or a class body with the same name as the import: the
entry applies where the callee is the import binding itself, not a local that shadows
it.

An entry is a claim about someone else's function. It says nothing about the
arguments, which still run, and nothing about the exports of the file it sits in,
which are reached by name however the declaration was built.

### Known factories

#### Redux Toolkit

A Redux Toolkit app makes its async actions with `createAsyncThunk`, and its slices
name them in their reducers:

```ts
export const refreshPlan = createAsyncThunk("session/refreshPlan", async (id: string) =>
  (await fetchPlan(id)).plan,
);

export const sessionSlice = createSlice({
  // …
  extraReducers: (builder) => {
    builder.addCase(refreshPlan.fulfilled, (state, action) => {
      state.plan = action.payload;
    });
  },
});
```

Read as a plain call, `refreshPlan.fulfilled` reaches the whole declaration: the
payload creator, and everything it calls. But `fulfilled` is an action creator made
from the type string alone. So are `pending`, `settled` and `typePrefix`, and so is
`rejected`, except that it also serialises the error with the options' `serializeError`,
the third argument. None reads the payload creator, and creating the thunk calls
nothing but the factory, so module initialisation reaches the call's callee and not
its arguments. A reducer, and every case or page built from it, is reached by an edit
to the type string and not by one to the payload creator. Calling the thunk, as
`dispatch(refreshPlan(id))` does, still reaches all of it. An argument that itself
runs something when evaluated, such as a type string built by a call, makes creating
the thunk ordinary initialisation.

The factory is matched however an app reaches it: imported from `@reduxjs/toolkit`
directly or through a namespace, typed with `createAsyncThunk.withTypes<…>()`, and
through modules of the app's own that export or re-export any of those. The usual
shape is a store module that exports the typed factory, imported by every slice.
A function merely called `createAsyncThunk` is not matched, and neither is one in a
file of the app that `@reduxjs/toolkit` resolves to.

As with an object literal, this holds only while nothing can change what a property
of the thunk holds: in its own file, its binding must only be read for a property,
called, or exported. An edit to lines of the payload creator's own marks the
thunk's members neither as a line range nor against a base revision, and an edit to
the options marks `rejected`. Adding or removing the options counts as an edit to
them wherever the edit lands after the last argument. An edit to the type string, to
the call around the arguments, or to the binding marks all of them, and so does one
to a line the payload creator shares with any of those.

#### Zustand

A Zustand store is usually declared beside other exports of its module:

```ts
export const COUNTER_TITLE = "Counter";

export const useCounter = create<CounterState>()((set) => ({
  count: 0,
  increment: () => set((state) => ({ count: state.count + 1 })),
}));
```

Read as a plain call, making the store is module initialisation, so a page that
imports only `COUNTER_TITLE` is reached by every edit to the store. But making a
store is not free of effects either: Zustand calls the creator there and then, with
`set`, `get` and the store's `api`, for the initial state. So the store counts as
initialisation unless calling its creator, once and where the store is made, is
proven to run nothing and not to throw. The proof is the one for [local
helpers](#pure-calls), with the creator's parameters standing for values nobody
knows: the creator may hand them on or hold them in the functions it returns, as
actions do, but calling one or reading through one is not proven, and what the
creator reads must already be declared where the store is made. Where the proof
holds, initialisation reaches only the call around the creator, and an edit inside
the creator reaches only what uses the store. With [inline
requires](#inline-requires), reading an import evaluates the module it names, so a
creator that may read one, directly or through a function of its file, is not proven
either, and its store stays initialisation.

Recognised are `create` from `zustand` and `zustand/react`, `createStore` from
`zustand` and `zustand/vanilla`, `createWithEqualityFn` from `zustand/traditional`,
whose equality function is kept for the hook rather than called, and Zustand 4's
default exports of `zustand` and `zustand/vanilla`. Each is matched in the curried form
`create<State>()(creator)` too, and however the app reaches it, as for
`createAsyncThunk`. An app's own function that wraps `create`, and a file of the app
that `zustand` resolves to, are not matched.

Using the store is not split by state key. Calling the hook, `getState()`,
`setState()` and passing the store around each reach the whole declaration, as they
would for any other value, whichever keys they read. Only making the store is
narrowed. A store written through `setState` by a declaration of its own file, such as
`export const reset = () => useCounter.setState({ count: 0 })`, links every page that
uses the store to that declaration, as [shared state](#granularity) does for any
exported value.

Under that rule, a method call on a value writes it. A store's own methods are told
apart: `getState()`, `getInitialState()` and `subscribe(listener)` read the store and
change none of it, as Zustand 4 and 5 write them, so a declaration that only reads
the store through them is no writer of it, and neither pages that use the store nor
other declarations that read it are linked to it. That holds only where what is
done with what they hand out reads it too: a property of the state used in place as
a value, compared, put through arithmetic or into a string, tested, rendered as the
child of an element such as `<li>` or of a fragment, or used as a `key`, the state
destructured into a local `const` whose bindings are used the same way, or a
listener written out in place, given alone, that does the same with the state it is
called with. Nothing taken from the state may leave the expression it is read in,
since which properties are actions cannot be told: returning one, as in
`() => useCounter.getState().count`, passing it on, storing it or spreading it
writes the store, as `() => useCounter.getState().inc` would hand out an action for
the caller to call. Passing it to a component, as a child or as a prop, passes it
on too, since the component may call it:
`<Confirm>{useCounter.getState().inc}</Confirm>` writes the store, and so does any
prop but `key`, even one of an element such as `<button onClick={…}>`, which calls
what it is handed. So do `setState()`, an action called on what `getState()`
returns, as in `useCounter.getState().inc()`, the state handed to other code, a
listener that is not written out in place, and every other method, Zustand 4's
`destroy()` among them. What a listener's body does to
the store through the store's own name is its declaration's, so a listener that
calls `setState()` still makes one a writer. The methods are recognised on a store
that one of the factories above is found to have made, however the app reaches the
factory; the same names on any other value, a store made by an app's own `create`
among them, are method calls like any other.

Three middlewares are recognised as wrappers, and count as quiet when what they wrap
is: `immer` from `zustand/middleware/immer`, and `subscribeWithSelector` and
`combine` from `zustand/middleware`, in Zustand 4 and 5 alike. Calling one only
builds the creator it returns, so evaluating `immer(creator)` runs what its
arguments run and nothing more, and calling what it returns calls the creator it
was given, after replacing `setState` or `subscribe` on the store's own `api`.
Wrappers nest, as in `create<State>()(immer(subscribeWithSelector(creator)))`, and
initialisation reaches each wrapper with the call around the creator. `combine`
also merges its initial state with what its creator returns, reading every property
of both, so each must be an object literal written out with no getter, spread or
computed key, and the initial state must run nothing as it is evaluated. A wrapper
is matched by the import it is, directly or through a namespace, and not by its
name, and one whose import lands in a file of the app is not matched. Calling a
wrapper reads its import, so with [inline requires](#inline-requires) a store
wrapped in one stays initialisation.

`persist` and `devtools` are not recognised, since `persist` reads storage and
`devtools` connects to the browser extension as the store is made. A store with
either anywhere in its chain, or with a wrapper of the app's own, stays
initialisation. So does one assembled from slices with
`(...a) => ({ ...createSlice(...a) })`.

Without `--base`, an edit that takes an effect out of a creator is not seen as a
change to initialisation, since the creator the change leaves behind runs nothing
and a line range cannot tell what it ran before. Against a base revision it is
seen.

### Where a claim applies

A `fallout.toml` is read from `--root`, and from any directory beneath it. A file in
`d` speaks for the files under `d`, and each file is answered by the chain from its
own directory up to the root.

A monorepo is why. Its apps are bundled by different tools and its packages give the
same name different meanings, so one file at the root cannot state what is true of all
of them: a single `inline-requires` would be a false claim about every app that does
not inline, and a single `[aliases]` table hands every app's names to every other
app's directories.

```
fallout.toml               pure = [...]            # everywhere
apps/mobile/fallout.toml   inline-requires = true  # this app's bundler
                           [resolve]               # and how it enters packages
apps/web/fallout.toml      [aliases]               # what this app's config maps
```

What happens where several files apply follows from the direction each setting is
wrong in:

| setting | several apply |
|---------|---------------|
| `[aliases]`, `[style.aliases]` | accumulate, nearest first — a name gets every directory claimed for it, tried in order |
| `pure` | accumulate, and an entry only ever applies below the file that wrote it |
| `builtin-pure`, `inline-requires`, each `[resolve]` key | one answer, so the nearest wins |

Aliases and pure entries accumulate rather than override because a name that resolves
to nothing loses an edge: the chain must never offer fewer candidates than the root
alone.

Which file does the asking is not the same for every setting, because they are not
claims about the same thing. An alias and a pure entry belong to the file doing the
importing — `packages/ui/card.scss` means one thing by `settings` whoever bundles it —
so they are read from the chain above that file. `inline-requires` is not a property
of a file at all but of the bundler, and the bundler is picked by the app being asked
about: the same shared module is inlined when a mobile bundler pulls it in and is not
when a web bundler does. So it is read from the chain above the **anchor**, and each
anchor is answered with its own app's setting.

Files are read as the run reaches what they speak for. A malformed file in a subtree
the run never enters is not reported, and could not have changed the answer.

### `sideEffects`

The `sideEffects` field of the nearest `package.json` is read the way bundlers read
it, and trusted the same way: it is a claim by the author that evaluating a module
runs nothing observable, and a wrong claim already breaks the build it ships in.

A module covered by the claim keeps its exports — those are reached by name, which is
data flow rather than a side effect of loading — but stops reaching importers through
module initialisation. So an edit to a top-level `const client = createClient()` no
longer reaches every page that imports something else from that file.

`false` covers the whole package. A list of globs names the files that *do* have side
effects, matched against the path relative to the package root, with a pattern naming
no directory matching at any depth (`"*.css"` covers a stylesheet anywhere). Absent,
`true`, or a pattern that cannot be read leaves a file analysed normally, since the
alternative is dropping an edge on a guess. Workspace packages reached through
`node_modules` symlinks are covered along with the app itself.

### Inline requires

By default, importing a module evaluates it. That is what the language says and what
most bundlers emit, so `ModuleInit(f)` reaches `ModuleInit(g)` for every module `f`
imports — and, transitively, for everything `g` imports. In an app where the entry
point pulls in a global store, that chain reaches most of the tree from most of the
tree, which is file-level reachability wearing a symbol-shaped node.

Some bundlers do not emit that. Metro's `inlineRequires`, and the equivalent under
other names, moves each `require` down to the first use of the binding it introduces,
so importing a module does nothing until something reads one of its names. A project
built that way says so:

```toml
# apps/mobile/fallout.toml
inline-requires = true
```

Written beside the app it describes, not at the root, unless every app in the tree is
bundled the same way. It is read from the chain above the anchor, so it is the page
being asked about that decides — see [Where a claim
applies](#where-a-claim-applies). Anchors in apps that disagree are each answered with
their own app's setting, on a graph of their own.

Then importing no longer evaluates, and reaching a name does. `ModuleInit(g)` hangs
off `Export(g, name)` instead of off `ModuleInit(f)`, which is the same work
attributed to whoever actually causes it: a declaration that uses `g`'s export
reaches `g`'s top-level statements, and a module that imports `g` and never touches
it reaches nothing.

Module-level code that reads an import loads that module at first use, which is as
the reading module is evaluated. So a declaration whose initialiser reads an import as
it runs at load, as `export const small = base` does, or a call there to a local
helper that reads one, counts as module initialisation, and so does what a statement
that declares nothing reads. A read in a function body waits for a call, and a
type-only import loads nothing. Against a base revision, a declaration that stops
reading an import, or goes, is a change to initialisation too.

Two things are unchanged. A bare `import "./setup"` introduces no binding, so there
is nothing to defer and it still runs when the importing module is evaluated. And a
namespace reference — `import * as ns`, `require(...)` — reaches the module itself
whether or not it exports anything, since a module that exports nothing has no name
to hang its evaluation on.

The setting is a claim about the build, and a wrong one under-reports: it would put
every top-level side effect behind a name nobody reads. It is off unless the project
turns it on, it applies only to anchors whose app claims it, and it only affects
`--granularity symbol`, since a whole-file verdict has no separate node for module
initialisation.

### CommonJS

A file that writes its exports the CommonJS way still has an export table; it is
spelled as assignment, and the spellings that say so plainly are read as one:

```js
exports.parse = (text) => { ... }
module.exports.format = (value) => { ... }
module.exports = { parse, format }
exports.parse = exports.read = (text) => { ... }
```

Each name becomes an export like any other, so a consumer asking for one arrives at
that name rather than at the file. The declaration behind it is the assignment
itself, held under the name `exports.parse` so that it cannot be mistaken for a local
binding the file happens to call `parse`.

Most of the CommonJS anybody actually reads is an ES module a compiler rewrote, and
those have a house style of their own, all of which is read too:

```js
Object.defineProperty(exports, "__esModule", { value: true });
exports.format = exports.parse = void 0;          // the names to come
var parse_1 = require("./parse");
Object.defineProperty(exports, "parse", { enumerable: true, get: function () { return parse_1.parse; } });
function format(value) { ... }
exports.format = format;
var _default = (exports.default = { parse, format });
```

The line of `void 0` is the compiler settling the shape of the table before filling
it in. It holds no value worth following, so it contributes no name of its own — and
every name it promises has to turn up in the table, or the file is one we have not
understood and coarsens. A defined property is a re-export, and defining an accessor
stores the function rather than calling it, so a descriptor that runs nothing on the
way past is read and one that does not is left alone. `var _default = (exports.default
= ...)` is `export default`: the assignment fills the table wherever it sits, and
filling the table is the export rather than a side effect of loading, so a page
importing the name beside it hears nothing about a change to the default.

A table has to be unambiguous to be read. Assigning `module.exports` as a whole
alongside individual properties, assigning it twice, or assigning the same name twice
all coarsen the file, because which assignment survives is a question about the order
statements run in rather than about the names. So does assigning the whole table
anything but an object literal — `module.exports = Widget` re-exports whatever
`Widget` turns out to hold — and so does a spread or a computed key inside one.

None of this holds unless the names involved are the runtime's — `module` and
`exports`, and the `Object` whose `defineProperty` a re-export is read through. A file
that declares or imports any of them means something of its own by the name, and an
assignment is then a write to somebody else's object rather than an export, so the
file coarsens like anything else the analyser cannot describe.

Coarsening here is by omission rather than by rule: every mention of `module` or
`exports` the table did not account for is left standing as a pattern the analyser
cannot describe, and takes the file down the same path as the rest of them. That
includes the table named on its own, without a property — handed to `Object.assign`,
to a compiler's `__exportStar`, to anyone. Reading half a table would be worse than
reading none, because a page importing the half that was read would be told its
import stands on one statement while the line below is free to replace it.

Anything the analyser cannot describe falls back to one opaque node for the whole
file, which is the `file` behaviour: an export table too tangled to read, a computed
`require()` or `import()` specifier, `eval`, `with`, TypeScript namespaces,
decorators, and any file that fails to parse. Giving up always means "treat this as
one unit", never "not affected".

Only the downstream search narrows. Upstream stays at file granularity, because a
change to a sibling component cannot reach a page through references even though the
two render together.

### Comparing against a base revision

A diff describes lines. It cannot say whether the file means anything different
afterwards, so a reworded comment and a rewritten function look alike. `--base` names
a revision to read the earlier version of each changed file from — `origin/main`,
`HEAD~1`, whatever the branch is measured against — and the two versions are then
compared as syntax rather than as text:

```sh
git diff -U3 origin/main... | fallout --anchor src/pages/CheckoutPage.tsx \
  --diff - --base origin/main --granularity symbol
```

A revision git cannot find from the root is no answer, exit code `2`: read as one
holding no files, it would have every changed file taken as one the change added.

Comments and formatting are not part of the comparison, so a change made only of
those marks nothing at all. This is the one case where a file the diff names is
reported as affecting nothing, and it holds at `file` granularity too.

A statement is matched to its counterpart by what it introduces rather than by where
it sits, which makes the remaining cases exact:

- A statement that only moved marks module initialisation, because the order the
  module computes things in is the only thing that changed about it.
- A statement that really differs marks what it declares, exports or imports, with no
  guessing at the statements around it. For an object literal that differs only in
  its properties' values, that means only the properties that changed. Any other
  change to it, including a property added, removed or reordered, marks every
  property.
- An export the base had and this version does not marks that *name*. A removal is
  invisible from inside the file — everything left behind reads as it did — so the
  name has to carry the mark itself, and it reaches the consumers that still ask for
  it whether by name, through a namespace, or through an `export *`. Nothing else
  hears about it, so deleting an export nobody imports costs nothing. A rename is
  this same case, which is what makes one detectable.
- Anything removed that the module used to *do* on evaluation — an import, a bare
  statement, a declaration whose initialiser may have run something — marks module
  initialisation. Removing an export whose value was computed marks both.
- A declaration that wrote a shared value (see [Granularity](#granularity)) and has
  been removed, or edited so that it no longer writes it, marks the value's readers:
  the declarations of its file that read what it wrote, the export of the value, and,
  of an object read only for its members, the members that write could reach. Nothing
  in the current version links those readers to it any more. The value's own
  declaration is not marked, since loading the file can reach it, and a page that
  imports only an unrelated name from the file stays apart.
- The whole file is marked only when the names that went cannot be listed: an
  `export * from` that was itself removed, a base version the analyser cannot
  describe, or a changed `"use client"`.

Without `--base`, the diff's line ranges are laid over the statements they fall in,
and a range landing between two statements marks both of them along with module
initialisation. A removed export cannot be seen that way at all — nothing in the
file after the change mentions it — so a consumer that was not updated alongside it
is reported only when there is a base revision to compare against. That fallback
also covers a file either version of which does not parse, and one git has no
earlier version of.

### Types

A type cannot change what a page renders. It can break the build, but a broken build
breaks every page at once and needs no answer about reachability — reachability is
no help with it. So a change made only of types is not a change this tool reports,
and every file is read with its type-only syntax erased:

```sh
git diff -U3 origin/main... | fallout --anchor src/pages/CheckoutPage.tsx \
  --diff - --base origin/main
```

`--include-types` turns that off and reads each file as it was written, for a run
that wants a type change to count.

The erasure happens once, on the source, before anything reads it, and everything
else follows from that rather than from a rule of its own. An `interface` is no
longer a statement, so nothing declares it and nothing depends on it. An
`import type` is no longer an import, so it is no longer an edge — which matters
more than it sounds, because a file whose imports are all type-only stops reaching
anything at all, and a barrel passing a type through stops carrying the module
behind it. Two versions of a file that differ only in their annotations become the
same text, so `--base` finds nothing between them. And a line the erasure empties is
a line no change to it can mark, which is the one thing a diff can prove on its own,
so this narrows at `file` granularity too.

What goes is what the language erases: annotations, type parameters and arguments,
`as`, `satisfies` and `!`, `implements`, interfaces, type aliases, anything
`declare`d, an overload signature, and a type-only name inside an import or export
that carries values too. What stays is everything that exists while the program runs,
however type-like it reads: an enum, a namespace, `import x = require(...)`, a
parameter property, an accessibility modifier.

A type does not always come away cleanly. A name in a list is held there by a comma,
`implements` needs something to name, `x!: T` carries its mark in front of the
annotation, and nothing may come between an arrow function's parameters and its `=>`
but spaces — so a return type written across lines is the one thing left where it
stands. Whatever is left behind is reparsed, and if it is no longer the language the
file is read as it was written, so a shape the eraser gets wrong costs precision
rather than an answer.

Erasing only ever takes syntax away, so it can only ever take verdicts away with it.
Every fixture in the suite is run both ways and held to that: whatever a read as
written reports, the default reports that or less, and never more.

### Explaining a verdict

```
$ fallout --anchor src/pages/VersionPage.tsx --diff pr.diff --explain -g symbol
Impact detected on target anchor via: "src/state/client.ts"
Path (downstream, symbol granularity):
  File(src/pages/VersionPage.tsx)
  ModuleInit(src/pages/VersionPage.tsx)
  ModuleInit(src/state/client.ts)
  Decl(src/state/client.ts, client)
```

Reading that: the page imports only `VERSION` from `client.ts`, but `client`'s
initialiser is a call, so it runs whenever the module is loaded.

The chain always reads in import order — each file imports the next — so a downstream
path starts at the anchor and an upstream path ends at it.

### Unresolved imports

An import specifier that resolves to nothing is an edge the graph does not have, and
it is the quietest way this tool can be wrong: the edge is dropped, the search carries
on, and out comes a confident "not affected" with no sign that anything went missing.

```
$ fallout --anchor apps/web/app/index.tsx --diff pr.diff --unresolved
No reachability impact detected

Resolved to nothing: 2 specifier(s), written in 3 file(s).
  #app/assets/cover.png
    apps/web/app/components/cover.tsx
    apps/web/app/components/hero.tsx
  ~sass/settings-and-mixins
    apps/web/app/styles/page.scss
```

A name the bundler answers and nothing else does is declared, under [Import
aliases](#import-aliases); that is what this report is for finding. Beyond that
nothing is done automatically, because an unresolved specifier is not by itself a
fault: a package nobody installed on this machine looks exactly like a broken
import, and a virtual module the bundler invents has no file to find. The flag reports
and does not judge — the verdict and the exit code are the same with it and without.

Two kinds are left out, because they are answers rather than failures: Node builtins
(`fs`, `node:fs/promises`) and Sass modules (`@use "sass:math"`). Neither names a file
and neither ever will.

The report covers what the run **reached**. A search that stops at the first change it
finds has not looked at the rest of the graph and does not report on it, so the widest
report comes from a run that finds nothing.

### Package entry points

Which file a package specifier names depends on the bundler: the `exports` conditions
it matches, and the `package.json` fields it reads for a package without `exports`.
Metro reads `react-native` before `main`, a web bundler reads `browser`. An app says
which it uses:

```toml
# apps/mobile/fallout.toml
[resolve]
conditions = ["react-native", "import", "require"]
main-fields = ["react-native", "main"]
```

Without it, the conditions are `import` and `require`, so a package whose `exports`
offers nothing else still resolves, and the only field is `main`. A `default` entry
always matches. Where a package's `exports` offers several conditions the app matches,
the package's own order decides which, as it does in Node, not the order written here.
`main-fields` reads a field that names a file; the object form of `browser`, which
replaces files within the package, is not read. Any other key under `[resolve]` is an
error rather than a setting that silently does nothing. Like `inline-requires`,
`[resolve]` is read from the chain above the anchor, the nearest file wins for each
key, and anchors in apps that disagree are each answered on a graph of their own.

## Changed dependencies

A dependency's code is not in the repository, so a change to it never appears in
a diff. What appears is the lockfile: one entry per resolved package, each pinned
to a hash. When that hash moves, the code behind every import of that package
moved with it, and nothing in the source tree records that it did.

`pnpm-lock.yaml` and `package-lock.json` are read. A changed entry gives its
package a node, and any file importing it reaches that node:

```
$ fallout --anchor apps/mobile/app/index.tsx --diff bump.diff \
          --granularity symbol --explain
Impact detected on target anchor via: "node_modules/@sentry/react-native"
Path (downstream, symbol granularity):
  File(apps/mobile/app/index.tsx)
  Decl(apps/mobile/app/index.tsx, default)
  File(node_modules/@sentry/react-native)
```

Nothing needs to be installed. The node stands for the package rather than for
any file of it, so the lockfile alone decides — which is fitting, because the
lockfile alone is what says the package changed.

Only a package's own record is read: its entry under `packages`, which carries
the version in its key and the hash in its body, and its entry under
`patchedDependencies`, which carries the hash of a patch laid on top. For npm,
the `node_modules/` keys, which carry the same two things.

Everything else in a lockfile is about relationships rather than about a
package, and is passed over. `importers`, `catalogs`, `overrides` and npm's root
entry record ranges that were *asked for*, and a range that moves without moving
a resolution installs the same bytes. `snapshots` records which version of each
dependency a package resolved to, which moves when a dependency moves while the
package itself stands still.

Passing `snapshots` over is the point rather than a shortcut. A version is a
package's promise about its public API, and the hash is the evidence behind that
promise — so a package whose version and hash are what they were is the package
it was, and naming it because something underneath it moved would report a
change to an API that did not change. On a real react-native bump this is the
difference between naming 17 packages and naming 89: the other 72 were
byte-identical copies rebuilt against the new peer.

A lockfile named without line information — by `--changed`, or as a binary diff
— names every package it lists, since there is nothing to narrow with.

## Changed resolution

An import names a file through the rules that resolve it, and a change can move the
file without touching the import. A tsconfig whose `paths` stop mapping
`@reduxjs/toolkit` to a shim of the app's own, or the shim deleted from under the
mapping, sends the same specifier to the package. The file that writes the import is
then as changed as if the import had been rewritten, and it is treated that way:
everything in it that reads the import is marked, along with its module
initialisation.

To find those imports, each one a search meets is resolved twice: in the tree as it
is, and in the tree as it was before the change. An import the two answer
differently has moved. The tree before is the current one with these differences:

- a deleted file is there again, and so is the old path of a renamed one;
- a file the change added is not;
- against a base revision, every changed JSON file holds what it held then, which is
  how a tsconfig, or a `package.json`'s `exports`, `imports` or `main`, changing its
  mind is seen.

Both are asked of the same resolver, so whatever decides where an import goes decides
whether it moved: `paths` and their fallbacks, `baseUrl`, `rootDirs`, `extends` and
`references`, which config owns a file, workspace packages, directory `main` files,
Sass partials, inline loaders.

As a line range there is no earlier version of a changed config to put back. A
changed JSON file other than a `package.json` then moves every import of the files it
may govern: those whose tsconfig reads it, through `extends` or `references`, or
whose `tsconfig.json` files above them do, and every file beneath a changed
`tsconfig.json`.

`--changed` names paths and nothing else, so a rename is two paths there: the old
one, which is no longer on disk and so reads as deleted, and the new one.
`git diff --name-only --no-renames` lists both; without `--no-renames` git lists only
the new one.

This is found by the downstream search only. The upstream search starts from the
changed files, and finding every file an import of which moved would mean resolving
every import in the repository first.

## What counts as an import

Static `import`, `export ... from`, `export * from`, dynamic `import()`, and `require()`.

Module resolution follows `tsconfig.json` path mappings, discovered automatically from the
root, and the `exports` and `imports` fields of the nearest `package.json` — so a package
naming its own internals, as in `"#app/*": "./app/*.js"`, resolves the way Node resolves it.
A `tsconfig.json` that is there but cannot be read, or one that extends or references
a config that cannot be, is no answer rather than resolved around: what it maps is not
known, and resolving as if it were not there would send its imports elsewhere.

A specifier ending in `.js` is tried as `.ts` and `.tsx` before `.js`, and `.jsx`,
`.cjs` and `.mjs` likewise. TypeScript makes a specifier name the file the compiler
will *emit* rather than the file beside it, so under `"module": "nodenext"` the file on
disk is `helper.ts` and every import of it is written `./helper.js`. A plain JavaScript
file keeps resolving: the extension it was written with is tried last rather than
dropped.

Non-JavaScript files — images, fonts, JSON — are part of the graph. A changed
PNG marks a page affected if some module the page reaches imports it. Bundler resource
queries (`./logo.png?url`, `./icon.svg?react`) and webpack inline loaders
(`!!file-loader!./logo.png`) resolve to the underlying file. Such files are leaves: they
are never parsed looking for imports of their own.

Stylesheets are not. See below.

### Import aliases

A specifier the bundler resolves and Node does not — `@/components/badge`, or any name
an app's config maps to a directory — has to be declared, for the same reason a
stylesheet alias does: that config is a program rather than data.

```toml
# apps/web/fallout.toml
[aliases]
"@/*" = "src/*"
"#app/assets/*" = "app/assets/*"
```

A key matches three ways, following the convention the bundlers share. `"@/*"`
captures what the `*` stood for and puts it back into the target. `"lodash$"` matches
that specifier exactly and nothing beneath it. A key with neither matches at a path
boundary, so `app` answers `app/lib/x` and leaves `application` alone. Targets are
relative to the `fallout.toml` that declares them, and a list of targets is tried in
turn.

Where several keys match one specifier the most specific is tried first — exact before
wildcard, and the longer literal prefix before the shorter — so `#app/assets/*` and
`#app/*` can both be declared and each mean what it looks like it means.

An alias is tried before the `exports` and `imports` fields of the nearest
`package.json`, and before the specifier is treated as a path or a package name, which
is the order a bundler uses: the project has said this name is already answered. A
`tsconfig.json` path mapping still wins over both.

That order is what makes the second key above worth writing. A package that maps its
own internals with `"#app/*": "./app/*.js"` has declared every `#app` name to be a
JavaScript file, so `#app/assets/cover.png` is looked for at `cover.png.js`, which is
not a file on any disk. The import resolves to nothing, the edge is dropped, and a
change to the asset reports no impact on the page that renders it. One alias puts the
whole tree of assets back.

The general table answers stylesheet imports as well, after `[style.aliases]`. A
bundler has one `resolve.alias` covering every kind of file and that is usually what a
project means; `[style.aliases]` stays for the names that mean something only inside a
stylesheet.

Assets referenced as `new URL("./worker.ts", import.meta.url)` are followed too, which
covers the `new Worker(new URL(...))` form used by Vite and webpack. Because the worker
resolves to a source file, the search continues through its own imports.

### Stylesheets

A `.css`, `.scss` or `.sass` file is read for the files it pulls in, so a chain of
stylesheets is a chain in the graph. `@use`, `@forward` and `@import` are all edges. A
page importing a stylesheet that `@use`s a partial of variables or mixins is reached by
a change to that partial, which is where almost everything in a design system lives.

A stylesheet is one node. Which rule inside one a change touched is not reported, and
will not be: knowing whether a changed rule matters would mean knowing which selectors
a page uses, which is a question about the markup rather than about the stylesheet.
Any change to a stylesheet marks the whole file.

Specifiers are resolved the way Sass resolves them, not the way JavaScript does:

| Written | Found |
| --- | --- |
| `@use "colors"` | `_colors.scss` beside the importing file, before any package |
| `@use "./tokens"` | `tokens/_index.scss` |
| `@use "~pkg/x"` | `pkg/x.scss`, the leading `~` dropped |
| `@use "sass:math"` | nothing — it names no file |

A name without an extension, written in a `.scss` or `.sass` file, is looked for in
the order the Sass spec gives: as `.sass` and `.scss`, each as written and as a `_`
partial, then as `.css` the same way, and only when none of those is there as
`name/index`, again with partials. So `_theme.scss` is found before `theme/_index.scss`
or `theme.css`, and an edit to the one Sass does not load reaches nothing. Where two
files answer the same step, as `theme.scss` beside `_theme.scss`, Sass refuses the
name; both are kept as edges, since which one was meant is not known. A name none of
those finds, and any name in a plain `.css` file, is looked for the way a bundler
looks for it.

A specifier like `~styles/settings` is neither of those. It is a name the app's
bundler config gives to a directory, and that config is a program rather than data, so
the project declares what it means:

```toml
# apps/web/fallout.toml
[style.aliases]
styles = "app/styles"

# One name, several directories. Each is tried in turn.
sass = ["app/sass", "../../packages/ui/sass"]
```

Targets are relative to the `fallout.toml` that declares them. Write the name without
the `~`: it is dropped before anything is looked up, so one entry covers
`~styles/settings` and `styles/settings` both. A name with no entry resolves to
nothing, and a stylesheet reached only through it is not reached at all.

`[aliases]` is consulted after this table, so a name the whole app shares needs
declaring only once — see [Import aliases](#import-aliases).

Two apps may give one name two meanings, because a table is read from the chain above
the stylesheet that wrote the import — see [Where a claim
applies](#where-a-claim-applies). A table nearer the file is tried before one further
up, and both are tried, so a root table stays the fallback for the packages neither
app owns.

### Known gaps

- An image referenced only by `url()` inside a stylesheet is not reached: a stylesheet
  is read for the stylesheets it pulls in, not for the assets it points at.
- Workers named by a bare string — `new Worker("./worker.js")` or
  `navigator.serviceWorker.register("/sw.js")` — are not detected. Bundlers require the
  `new URL` form, but a service worker registered by public URL has no source path to
  resolve.
- At `symbol` granularity, a getter, an iterator or a proxy that does something on load
  can be missed, since reading a property and the like is taken to run nothing — see
  [Granularity](#granularity).
- At `symbol` granularity, a value read from another module reaches only the writers
  in its own file. A value written in a third file, or through the binding an import
  gives, is not linked to its readers yet.
- At `symbol` granularity without `--base`, a declaration that was removed, or edited
  so that it no longer writes a shared value, is not linked to that value's readers:
  a line range cannot say what a statement used to write. A removed writer can still
  be reported through the statements around the lines it left.
- At `symbol` granularity, reading an import is taken to run nothing, in the module body
  and in a local helper. During an import cycle a `let`, `const` or class read before
  its own module has run throws, and that can be missed.
- Single-file component formats such as `.vue` and `.svelte` are treated as leaves rather
  than parsed.
- Only pnpm and npm lockfiles are read. A project on yarn or bun gets no answer
  about its dependencies, rather than a wrong one.
- A change to `[aliases]` in a `fallout.toml` does not move the imports it resolves,
  and neither does a change to a `package.json` given as a line range, with no base
  revision to compare it with.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.
