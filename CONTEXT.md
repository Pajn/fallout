# fallout

Answers whether a change to a repository can alter what a user sees on a given page, by
following static references from the page's code to the code the change touched.

## Language

### The question

**Anchor**:
A file standing for a page, typically the component behind an end-to-end test, whose
exposure to the change is being asked about.
_Avoid_: target, entry

**Affected**:
The answer for an anchor when a chain of static references connects it to something the
change marks.

**Downstream / Upstream**:
The two directions a chain can run: the anchor imports the changed code, or the changed
code imports the anchor.

### The change

**Change**:
Everything a run takes the PR to have done: the files it changed and the **Extent** of
each, the packages whose lockfile entries changed, and the imports it may have moved.
_Avoid_: changes, touched set

**Change set**:
A diff as parsed, one of the inputs a **Change** is read from. It says what the diff
names, not what a run treats as changed.
_Avoid_: using it to mean the Change

**Extent**:
How much of one file the **Change** reaches: none of it, all of it, some lines, or
particular statements.

**Changed package**:
A dependency whose lockfile entry the change touched, which stands in the graph as a
single node of its own.

**Moved import**:
An import whose text is unchanged but which the tree before the change resolves to a
different file, or to none. It counts as changed as if it had been rewritten.
_Avoid_: repointed import (in prose)

**Marked**:
Said of a file or node that the **Change** reaches. Marking decides what changed;
granularity decides how finely.

### Granularity

**Coarse**:
Said of a file the analysis cannot see inside, and so treats as one opaque node. Anything
unknown becomes coarse, never unaffected.
_Avoid_: unknown, skipped

**Soundness contract**:
The rule that the analysis may over-report but never under-report, relative to what the
import graph can express; a coarser way of naming a change is always at least as loud as
a finer one.
