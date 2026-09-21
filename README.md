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
```

| Flag | Description |
| --- | --- |
| `-a, --anchor <PATH>` | Target component. Repeatable; the anchor set is affected if *any* anchor is. |
| `-c, --changed <PATH>` | A changed file, as produced by `git diff --name-only`. Repeatable. |
| `-r, --root <PATH>` | Root directory to resolve from. Defaults to the current directory. |
| `-o, --only <DIRECTION>` | Search only `downstream` or `upstream` instead of both. |

Exit codes: `0` — affected, run the tests. `1` — not affected, or the arguments were
invalid. Errors are written to stderr.

### Direction

By default `fallout` searches both directions and stops at the first hit.

- `downstream` — the anchor imports the changed file, directly or transitively.
- `upstream` — the changed file imports the anchor, directly or transitively.

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
