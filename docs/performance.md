# Selection performance

The benchmark measures the CLI process used for VRT selection: multiple anchors,
symbol granularity, downstream only, JSON output, and a diff compared against a
base revision. Keep the app checkout and installed dependencies fixed throughout
the comparison. The diff is historical, but the source tree is the pinned app
checkout; this does not check out each historical commit.

## Reproduce

Build each revision with `cargo build --release --locked` and copy its binary to
a separate path before building another. Use an isolated app worktree so another
task cannot switch branches or edit its source during the measurement.

```sh
python3 scripts/bench-selection.py \
  --root /path/to/pinned-app \
  --cases /path/to/pinned-app/apps/mobile/vrt/cases.gen.json \
  --binary released=/path/to/released/fallout \
  --binary candidate=/path/to/candidate/fallout \
  --commit c385052125 --commit 364a22d5ed --commit 16014498e9 \
  --runs 15 --require-equal-selection --output /tmp/selection-perf.json
```

The standard-library Python harness runs on macOS and Linux. It warms each build
once per commit, rotates the build order between repetitions, and uses `wait4`
to record CPU time and peak resident memory for each child separately. Diff
generation, output parsing, and checkout checks happen outside the timed child.
Wall time is retained alongside CPU time, but a loaded machine is unsuitable for
judging small wall-time differences. CPU time can also rise under contention;
repeat a comparison on a quieter machine before assigning a release budget.

The JSON keeps individual samples, medians, selections, binary hashes, the app
revision and worktree status. A changing selection within one build fails the
run. `--require-equal-selection` also fails on selection differences between
builds; omit it when comparing releases with known correctness changes, and
inspect the reported added and removed anchors instead. Do not trade corrected
selection paths for timing improvements.

## Initial investigation, 2026-10-02

The app was pinned to #8955's branch at
`78f729678f8a503c4273fbfaa6c9160d5695fa94`, with seven VRT anchors. Installed
external packages came from the existing local dependency store; workspace
package links pointed into the isolated worktree. An initial run against the
active checkout was discarded because another task changed its branch.

Fifteen interleaved runs of 0.3.1 and 0.4.0 on each of the three commits showed
CPU ratios of 1.072, 1.115 and 1.107 respectively, and about 3 MiB more peak RSS.
Selections agreed on these three inputs. Concurrent compiler jobs drove load
above 100 during this run, so these ratios are provisional and do not replace
the earlier measurements on the app.

Five macOS `sample` profiles of 0.4.0 on `c385052125` collected 5,688 main-thread
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
| `c385052125` | 0.6067 s | 0.6192 s | 1.021 |
| `364a22d5ed` | 0.4192 s | 0.4309 s | 1.028 |
| `16014498e9` | 0.5756 s | 0.5561 s | 0.966 |

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

Git tracing on `c385052125` counted 25 commands in 0.4.0 (one revision lookup and
24 `git show` calls), versus two with batching (the revision lookup and one
`git cat-file`). This also avoids adding a Git startup to a one-file change.

Fifteen interleaved runs per commit of the final build against 0.4.0 gave:

| Commit | 0.4.0 CPU median | Batch CPU median | CPU reduction | RSS change |
| --- | ---: | ---: | ---: | ---: |
| `c385052125` | 0.6045 s | 0.2762 s | 54.3% | +0.250 MiB |
| `364a22d5ed` | 0.4317 s | 0.2753 s | 36.2% | -0.172 MiB |
| `16014498e9` | 0.5645 s | 0.2835 s | 49.8% | +0.016 MiB |

Every measured selection matched 0.4.0. These comparisons use the same pinned
app tree and seven anchors as the initial investigation. The final raw samples
are saved locally in `/tmp/fallout-perf-batch-final.json`.

A separate selection comparison also matched 0.4.0 on all 100 most recent
commits touching `apps/mobile` or `packages` on app main at
`c0dacb350ac042a4184f2f81a2c7ce3ea2cb3086`, evaluated against the same pinned
#8955 tree. This preserves the Throbber path corrected in 0.4.0. The answers and
exact commit list are saved locally in `/tmp/fallout-perf-history.json`.

The release test suite and Clippy passed. Tests exercise missing and non-text
files followed by further reads, empty and large blobs, unusual names, deleted
directories, a moving ref while the reader is live, an interrupted child, broken
response framing and nested repositories.

## Next investigations

- Separate CPU spent in resolution from filesystem waiting before adding caches.
  Repeated config and tsconfig queries are candidates; file ownership cannot be
  inferred solely from the directory because tsconfig projects may claim
  different files within it.
- Profile module analysis after the filesystem costs are separated. Shared-value
  access classification, local-helper proofs and repeated factory resolution are
  plausible targets from code inspection, but these samples do not establish
  any one of them as the cause of the release-to-release CPU increase.

Use the same pinned inputs to compare subsequent releases, retain raw samples,
and check CPU and memory together. Validate selections across the broader commit
history as well as the slow inputs before accepting an optimisation.
