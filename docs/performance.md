# Selection performance

The benchmark measures the CLI process as CI uses it to select test suites:
multiple anchors, symbol granularity, downstream only, JSON output, and a diff
compared against a base revision. Keep the app checkout and installed dependencies fixed throughout
the comparison. The diff is historical, but the source tree is the pinned app
checkout; this does not check out each historical commit.

## Reproduce

Build each revision with `cargo build --release --locked` and copy its binary to
a separate path before building another. Use an isolated app worktree so another
task cannot switch branches or edit its source during the measurement.

The command below shows the shape of a run. Pass one `--anchor` per page the
comparison covers; the results below used seven anchors of one app, so a run
with other anchors or another app measures a different workload.

```sh
python3 scripts/bench-selection.py \
  --root /path/to/pinned-app \
  --anchor src/pages/CheckoutPage.tsx --anchor src/pages/SettingsPage.tsx \
  --binary released=/path/to/released/fallout \
  --binary candidate=/path/to/candidate/fallout \
  --commit <rev> --commit <rev> --commit <rev> \
  --runs 15 --require-equal-selection --output selection-perf.json
```

The standard-library Python harness runs on macOS and Linux. It warms each build
once per commit, rotates the build order between repetitions, and uses `wait4`
to record CPU time and peak resident memory for each child separately. Diff
generation, output parsing, and checkout checks happen outside the timed child.
Wall time is retained alongside CPU time, but a loaded machine is unsuitable for
judging small wall-time differences. CPU time can also rise under contention;
repeat a comparison on a quieter machine before assigning a release budget.
Each child has a 300-second deadline, configurable with `--timeout`. Completion
polling can add up to 10 ms to recorded wall time; CPU time and peak RSS come
from the kernel's resource usage for the child.

The JSON keeps individual samples, medians, selections, binary hashes, the app
revision and worktree status. A changing selection within one build fails the
run. `--require-equal-selection` also fails on selection differences between
builds; omit it when comparing releases with known correctness changes, and
inspect the reported added and removed anchors instead. Do not trade corrected
selection paths for timing improvements.

## Initial investigation, 2026-10-02

The app, a pnpm monorepo, was pinned to one revision with seven anchors.
Workspace package links pointed into the isolated worktree. An initial run
against an active checkout was discarded because another task changed its branch.

Fifteen interleaved runs of 0.3.1 and 0.4.0 on each of three commits (A, B, C) showed
CPU ratios of 1.072, 1.115 and 1.107 respectively, and about 3 MiB more peak RSS.
Selections agreed on these three inputs. Concurrent compiler jobs drove load
above 100 during this run, so these ratios are provisional and do not replace
the earlier measurements on the app.

Five macOS `sample` profiles of 0.4.0 on commit A collected 5,688 main-thread
samples. About 37% were under reading base text from git, 48% under resolution
and filesystem work, 8% under module analysis, and 4% under graph edge generation.
These are stack samples that **include blocking time**, not CPU attribution.
They identify work to investigate rather than establishing how much CPU an
optimisation would save.

One repeated operation was a filesystem check in `Configs::chain`: every import
specifier asked whether its containing source path was a directory, even though
the caller already knew it was a file. A candidate used the file's parent
directly and shared the existing directory-chain cache, preserving directory
arguments for the general public lookup. It added no cache and changed no
analysis or graph rules. The full test suite and Clippy passed, and selections
matched 0.4.0 in every measured run.

Two comparisons of fifteen interleaved runs each did not show a consistent
improvement. The first suggested roughly 5%, 1% and 4% less CPU. In the final
comparison, which preserved the existing directory behavior for anchor lookup,
the result was:

| Commit | 0.4.0 CPU median | Candidate CPU median | Candidate / 0.4.0 |
| --- | ---: | ---: | ---: |
| A | 0.6067 s | 0.6192 s | 1.021 |
| B | 0.4192 s | 0.4309 s | 1.028 |
| C | 0.5756 s | 0.5561 s | 0.966 |

Memory stayed within 0.1 MiB of the release. The code change was discarded:
removing a syscall alone is not evidence of an end-to-end improvement. The
benchmark harness and this report remain as the starting point for the next
investigation.

## Batched base reads

`Base` now keeps a lazy `git cat-file --batch -Z` reader for the run. Each
uncached text request reads one blob from the same pinned commit through that
process. NUL framing supports newlines in filenames, and byte-length framing
supports empty files, binary contents and blobs larger than a pipe buffer. The
existing contents cache is retained; no whole-tree prefetch is added.

The reader locates ordinary checkouts and linked worktrees through their `.git`
directory or file, without a separate Git command. Paths belonging to another
repository, custom repository layouts, Git without the required protocol, and
interrupted or malformed responses use the original per-file reader. A failed
batch read must not become a claim that the file was absent from the base.
The child is killed and reaped when the reader is dropped.

Git tracing on commit A counted 25 commands in 0.4.0 (one revision lookup and
24 `git show` calls), versus two with batching (the revision lookup and one
`git cat-file`). This also avoids adding a Git startup to a one-file change.

Fifteen interleaved runs per commit of the final build against 0.4.0 gave:

| Commit | 0.4.0 CPU median | Batch CPU median | CPU reduction | RSS change |
| --- | ---: | ---: | ---: | ---: |
| A | 0.6045 s | 0.2762 s | 54.3% | +0.250 MiB |
| B | 0.4317 s | 0.2753 s | 36.2% | -0.172 MiB |
| C | 0.5645 s | 0.2835 s | 49.8% | +0.016 MiB |

Every measured selection matched 0.4.0. These comparisons use the same pinned
app tree and seven anchors as the initial investigation.

A separate selection comparison also matched 0.4.0 on the 100 most recent
commits touching the anchors' app or its workspace packages, evaluated against
the same pinned tree. This preserves a selection path corrected in 0.4.0.

The release test suite and Clippy passed. Tests exercise missing and non-text
files followed by further reads, empty and large blobs, unusual names, deleted
directories, a moving ref while the reader is live, an interrupted child, broken
response framing and nested repositories.

## Name filter for the tree before

With batched base reads in place, Time Profiler samples on a later revision of
the app put about a third of the run under `BeforeFs::entry`. Resolving an
import in the tree before the change probes candidate paths — each extension,
each index file — and most of them are on neither tree. For each such path,
`entry` checked the disk, then canonicalized ancestors one at a time until one
existed, only to look the result up among the handful of paths the change
touched.

`Before` now also records the last component of every path it holds. A path
spelled without `.` or `..` that the disk does not have cannot canonicalize to
a different name: only the directories above it can be spelled another way. So
when its name is not recorded, it is in neither map however it is spelled, and
the lookup ends without canonicalizing. Paths with `.` or `..` keep the full
lookup.

Eleven interleaved runs per commit, on four commits, against the release
before it:

| Commit | Before CPU median | Filter CPU median | CPU reduction | RSS change |
| --- | ---: | ---: | ---: | ---: |
| D | 0.2397 s | 0.1636 s | 31.7% | +0.38 MiB |
| E | 0.2335 s | 0.1576 s | 32.5% | +0.25 MiB |
| F | 0.2353 s | 0.1655 s | 29.7% | +0.36 MiB |
| G | 0.2460 s | 0.1793 s | 27.1% | +0.28 MiB |

Selections matched on these and on the 60 most recent commits touching the
anchors' app or its workspace packages. The release test suite and Clippy
passed, including the test of a deleted file reached through a workspace
package's `node_modules` link.

## Reading imports only, in parallel, at file granularity

A file-granularity run asks only which files each file imports, but it read
every file through the full module analysis built for symbol granularity:
CommonJS export tables, the coarsening check, semantic analysis and reference
linking, all discarded once the import list was taken. This is the shape of a
caller asking per project whether its entry points import a change, where an
answer of not affected walks the whole application.

The measurement replays that call: four anchors of one application, file
granularity, downstream only, the changed paths with `--changed` and the
commit's parent as `--base`, on each commit's own tree.

Reading for imports now stops after parsing and type erasure, which the full
analysis shares, so both read the same specifiers. Reading a file depends on
no other, so the walk also reads every file waiting in its queue at once, on
the thread pool, before going on in its usual order. The answer, and the chain
it reports, are those of the serial walk; a hit found early only wastes the
reads of files the walk does not reach. Reading for imports consults no
configuration, so reading ahead cannot record a failure that the serial walk
would not.

One run per build on each of the 100 most recent first-parent commits:

| Build | Median wall | Slowest wall | Total CPU |
| --- | ---: | ---: | ---: |
| Before | 0.638 s | 1.430 s | 63.2 s |
| Imports only | 0.469 s | 0.999 s | 42.0 s |
| Imports only, parallel | 0.319 s | 0.806 s | 79.4 s |

Answers matched on every commit. Parallel reads trade CPU for wall time: the
thread pool costs more CPU in total than the serial reads it replaces, and the
resolver remains serial, which bounds the gain. Symbol granularity reads files
through the graph instead and is unchanged.

## Next investigations

- Separate CPU spent in resolution from filesystem waiting before adding caches.
  Repeated config and tsconfig queries are candidates; file ownership cannot be
  inferred solely from the directory because tsconfig projects may claim
  different files within it.
- Comparing imports across the two trees still takes about a fifth of a run.
  The tree before has its own resolver cache, so it repeats every probe the
  tree as it is already made, including for paths the change did not touch.
- Symbol granularity is still single-threaded. Its graph reads files lazily
  through shared, unsynchronized caches, so reading ahead there needs the
  analysis split from the graph first.
- Resolution is serial in both walks and now bounds the file-granularity one.
- Profile module analysis after the filesystem costs are separated. Shared-value
  access classification, local-helper proofs and repeated factory resolution are
  plausible targets from code inspection, but these samples do not establish
  any one of them as the cause of the release-to-release CPU increase.

Use the same pinned inputs to compare subsequent releases, retain raw samples,
and check CPU and memory together. Validate selections across the broader commit
history as well as the slow inputs before accepting an optimisation.
