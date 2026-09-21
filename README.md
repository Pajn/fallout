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
cargo install fallout
```

## Usage

```sh
fallout --anchor src/pages/CheckoutPage.tsx --changed src/components/Button.tsx

git diff -U3 main... | fallout --anchor src/pages/CheckoutPage.tsx --diff -
```

| Flag | Description |
| --- | --- |
| `-a, --anchor <PATH>` | Target component. Repeatable; the anchor set is affected if *any* anchor is. |
| `-c, --changed <PATH>` | A changed file, as produced by `git diff --name-only`. Repeatable. |
| `-d, --diff <PATH>` | A unified diff describing the change; `-` reads standard input. |
| `-r, --root <PATH>` | Root directory to resolve from. Defaults to the current directory. |
| `-o, --only <DIRECTION>` | Search only `downstream` or `upstream` instead of both. |
| `-g, --granularity <LEVEL>` | `file` (default) or `symbol`. See below. |
| `-e, --explain` | Print the chain of imports that produced the verdict. |

`--diff` and `--changed` may be combined; their file sets are unioned. A diff is read
as text rather than by shelling out, so the tool needs no git checkout at runtime.
Files the change deletes are dropped: they have no after version to reach.

Exit codes: `0` — affected, run the tests. `1` — not affected, or the arguments were
invalid. Errors are written to stderr.

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
declarations that only do those stay apart. Everything else — passing it to a
function, returning it, writing through it, spreading it, or naming a member of it as
an element, which is how a React context is written — counts as a write. A declaration whose initialiser may run something — a
call, a `new`, an `await`, a tagged template, an assignment to a member — belongs to
module initialisation, so importing anything from that file reaches it. A bare
`import "./theme.css"` is a side effect of loading the module and reaches every
importer, while `import logo from "./logo.png"` reaches only the declarations using
`logo`.

A `require("./x")` is an ordinary dependency of the declaration that contains it. It
yields the whole export object, so it reaches every export of its target, the same as
`import * as ns`. A `require` outside every declaration runs on evaluation, like a
bare import.

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

Anything the analyser cannot describe falls back to one opaque node for the whole
file, which is the `file` behaviour: a CommonJS export table (`module.exports`,
`exports.x`), a computed `require()` or `import()` specifier, `eval`, `with`,
TypeScript namespaces, decorators, and any file that fails to parse. Giving up always
means "treat this as one unit", never "not affected".

Only the downstream search narrows. Upstream stays at file granularity, because a
change to a sibling component cannot reach a page through references even though the
two render together.

### Explaining a verdict

```
$ fallout --anchor src/pages/VersionPage.tsx --diff pr.diff --explain -g symbol
Impact detected on target anchor via: "/repo/src/state/client.ts"
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

## What counts as an import

Static `import`, `export ... from`, `export * from`, dynamic `import()`, and `require()`.

Module resolution follows `tsconfig.json` path mappings, discovered automatically from the
root.

Non-JavaScript files — images, fonts, stylesheets, JSON — are part of the graph. A changed
PNG marks a page affected if some module the page reaches imports it. Bundler resource
queries (`./logo.png?url`, `./icon.svg?react`) and webpack inline loaders
(`!!file-loader!./logo.png`) resolve to the underlying file. Such files are leaves: they
are never parsed looking for imports of their own.

Assets referenced as `new URL("./worker.ts", import.meta.url)` are followed too, which
covers the `new Worker(new URL(...))` form used by Vite and webpack. Because the worker
resolves to a source file, the search continues through its own imports.

### Known gaps

- A stylesheet is a leaf, so an image referenced only by `url()` inside an imported CSS
  file is not reached.
- Workers named by a bare string — `new Worker("./worker.js")` or
  `navigator.serviceWorker.register("/sw.js")` — are not detected. Bundlers require the
  `new URL` form, but a service worker registered by public URL has no source path to
  resolve.
- Single-file component formats such as `.vue` and `.svelte` are treated as leaves rather
  than parsed.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.
