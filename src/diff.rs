//! Unified diff parsing.
//!
//! The diff is consumed as text rather than by shelling out to git, so that fixtures
//! can be plain files and the tool needs no git checkout at runtime.
//!
//! Only the *after* version matters: a change is a set of line ranges in the file as
//! it will exist once the change lands. Paths are recorded exactly as the diff spells
//! them, relative to the repository root.

use std::path::PathBuf;

/// A run of lines in the after version of a file, 1-based.
///
/// `len == 0` is a pure deletion: a zero-width position where lines used to be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LineRange {
    pub start: u32,
    pub len: u32,
}

/// What happened to one file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileChange {
    /// Content changed, with line information for the after version.
    Modified { ranges: Vec<LineRange> },
    /// Changed, but with no usable line information: a binary file, or a rename
    /// recorded without hunks. Always marks the whole file.
    Opaque,
    /// Gone in the after version.
    Deleted,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangedFile {
    /// Path in the after version, relative to the repository root.
    pub path: PathBuf,
    pub change: FileChange,
}

/// Every file a diff touches, in the order the diff lists them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ChangeSet {
    pub files: Vec<ChangedFile>,
}

impl ChangeSet {
    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }
}

/// Parses a unified diff.
///
/// Ranges come from the hunk *body*, not its header: a header range includes the
/// surrounding context lines, and marking three untouched lines either side of every
/// edit would attribute changes to neighbouring declarations.
///
/// Unrecognised lines are ignored: a diff carries commit messages, index lines and
/// mode changes that say nothing about which lines moved.
pub fn parse(text: &str) -> ChangeSet {
    let mut files: Vec<ChangedFile> = Vec::new();
    // The file currently being read, kept aside until the next header so that late
    // markers (a binary notice, a `+++ /dev/null`) can still change its verdict.
    let mut current: Option<ChangedFile> = None;
    let mut hunk: Option<Hunk> = None;

    for line in text.lines() {
        // A hunk header states how many lines of each side follow, so the body ends
        // deterministically rather than by guessing which line looks like a header.
        if let Some(active) = hunk.as_mut() {
            if active.consume(line, &mut current) {
                if active.finished() {
                    hunk = None;
                }
                continue;
            }
            hunk = None;
        }

        if let Some(rest) = line.strip_prefix("diff --git ") {
            flush(&mut files, current.take());
            current = Some(ChangedFile {
                path: git_header_after_path(rest),
                change: FileChange::Modified { ranges: Vec::new() },
            });
        } else if let Some(rest) = line.strip_prefix("+++ ") {
            // Authoritative for the after path, and for deletion.
            if let Some(file) = current.as_mut() {
                let target = strip_path_field(rest);
                if target.as_os_str() == "/dev/null" {
                    file.change = FileChange::Deleted;
                } else if !target.as_os_str().is_empty() {
                    file.path = target;
                }
            } else {
                let target = strip_path_field(rest);
                if target.as_os_str() != "/dev/null" && !target.as_os_str().is_empty() {
                    current = Some(ChangedFile {
                        path: target,
                        change: FileChange::Modified { ranges: Vec::new() },
                    });
                }
            }
        } else if let Some(rest) = line.strip_prefix("rename to ") {
            if let Some(file) = current.as_mut() {
                file.path = unquote(rest.trim());
            }
        } else if line.starts_with("Binary files ") || line.starts_with("GIT binary patch") {
            if let Some(file) = current.as_mut()
                && file.change != FileChange::Deleted
            {
                file.change = FileChange::Opaque;
            }
        } else if line.starts_with("@@") {
            hunk = parse_hunk_header(line);
        }
    }
    flush(&mut files, current.take());

    ChangeSet { files }
}

/// Tracks position within a hunk body.
struct Hunk {
    /// Next line number in the after version.
    after_line: u32,
    old_remaining: u32,
    new_remaining: u32,
}

impl Hunk {
    fn finished(&self) -> bool {
        self.old_remaining == 0 && self.new_remaining == 0
    }

    /// Consumes one body line, returning whether it belonged to the hunk.
    fn consume(&mut self, line: &str, current: &mut Option<ChangedFile>) -> bool {
        // "\ No newline at end of file" annotates the previous line and consumes
        // nothing from either side.
        if line.starts_with('\\') {
            return true;
        }

        match line.chars().next() {
            // An empty line is a context line whose trailing space was stripped.
            None | Some(' ') => {
                self.old_remaining = self.old_remaining.saturating_sub(1);
                self.new_remaining = self.new_remaining.saturating_sub(1);
                self.after_line += 1;
                true
            }
            Some('+') => {
                self.new_remaining = self.new_remaining.saturating_sub(1);
                push_range(
                    current,
                    LineRange {
                        start: self.after_line,
                        len: 1,
                    },
                );
                self.after_line += 1;
                true
            }
            Some('-') => {
                self.old_remaining = self.old_remaining.saturating_sub(1);
                // The line is gone, so it has no extent in the after version. Record
                // where it used to be.
                push_range(
                    current,
                    LineRange {
                        start: self.after_line,
                        len: 0,
                    },
                );
                true
            }
            _ => false,
        }
    }
}

/// Adds a range, merging it into the previous one when they are adjacent.
fn push_range(current: &mut Option<ChangedFile>, range: LineRange) {
    let Some(file) = current.as_mut() else { return };
    let FileChange::Modified { ranges } = &mut file.change else {
        return;
    };

    if let Some(last) = ranges.last_mut() {
        if last.len > 0 && range.len > 0 && last.start + last.len == range.start {
            last.len += range.len;
            return;
        }
        if *last == range {
            return;
        }
    }
    ranges.push(range);
}

fn flush(files: &mut Vec<ChangedFile>, file: Option<ChangedFile>) {
    let Some(mut file) = file else { return };

    // A rename with no hunks carries no line information, so it marks the whole file.
    if matches!(&file.change, FileChange::Modified { ranges } if ranges.is_empty()) {
        file.change = FileChange::Opaque;
    }
    if file.path.as_os_str().is_empty() {
        return;
    }
    files.push(file);
}

/// `@@ -12,3 +14,6 @@ optional context` -> a cursor at after-line 14, expecting 3
/// old-side and 6 new-side lines. A missing count means one line.
fn parse_hunk_header(line: &str) -> Option<Hunk> {
    let (old_start, old_count) = hunk_side(line, '-')?;
    let (new_start, new_count) = hunk_side(line, '+')?;
    let _ = old_start;
    Some(Hunk {
        // A hunk that adds at the very start of a file is numbered from 0.
        after_line: new_start.max(1),
        old_remaining: old_count,
        new_remaining: new_count,
    })
}

fn hunk_side(line: &str, marker: char) -> Option<(u32, u32)> {
    let field = line.split(marker).nth(1)?;
    let field = field.split_whitespace().next()?;
    let mut parts = field.splitn(2, ',');
    let start: u32 = parts.next()?.parse().ok()?;
    let count: u32 = match parts.next() {
        Some(count) => count.parse().ok()?,
        None => 1,
    };
    Some((start, count))
}

/// The after path from a `diff --git a/x b/x` header.
///
/// Both halves are prefixed, so the second half is everything after the midpoint. Git
/// quotes paths containing unusual bytes, which makes splitting on whitespace wrong;
/// prefer the quoted form when one is present.
fn git_header_after_path(rest: &str) -> PathBuf {
    if let Some(open) = rest.rfind(" \"") {
        return unquote(rest[open + 1..].trim());
    }
    match rest.rfind(" b/") {
        Some(index) => strip_path_field(&rest[index + 1..]),
        None => rest
            .split_whitespace()
            .next_back()
            .map(strip_path_field)
            .unwrap_or_default(),
    }
}

/// Strips the `a/` or `b/` prefix a diff puts on each path, plus any trailing
/// timestamp column that `diff -u` adds.
fn strip_path_field(field: &str) -> PathBuf {
    let field = field.split('\t').next().unwrap_or(field).trim_end();
    let field = unquote(field);

    let Some(text) = field.to_str() else {
        return field;
    };
    if text == "/dev/null" {
        return PathBuf::from(text);
    }
    match text.split_once('/') {
        Some((prefix, rest)) if prefix == "a" || prefix == "b" => PathBuf::from(rest),
        _ => PathBuf::from(text),
    }
}

/// Undoes git's C-style quoting. Only the escapes git actually emits for paths are
/// handled; anything else is passed through.
fn unquote(field: &str) -> PathBuf {
    let Some(inner) = field.strip_prefix('"').and_then(|f| f.strip_suffix('"')) else {
        return PathBuf::from(field);
    };

    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some(other) => out.push(other),
            None => out.push('\\'),
        }
    }
    PathBuf::from(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn modified(ranges: &[(u32, u32)]) -> FileChange {
        FileChange::Modified {
            ranges: ranges
                .iter()
                .map(|&(start, len)| LineRange { start, len })
                .collect(),
        }
    }

    #[test]
    fn attributes_only_the_changed_line_not_its_context() {
        let set = parse(
            "diff --git a/src/a.ts b/src/a.ts\n\
             index 1111111..2222222 100644\n\
             --- a/src/a.ts\n\
             +++ b/src/a.ts\n\
             @@ -1,3 +1,3 @@\n\
             \x20first\n\
             -second\n\
             +SECOND\n\
             \x20third\n",
        );

        assert_eq!(set.files.len(), 1);
        assert_eq!(set.files[0].path, PathBuf::from("src/a.ts"));
        // Line 2 changed. Lines 1 and 3 are context and must not be marked.
        assert_eq!(set.files[0].change, modified(&[(2, 0), (2, 1)]));
    }

    #[test]
    fn merges_a_run_of_added_lines() {
        let set = parse(
            "diff --git a/src/a.ts b/src/a.ts\n\
             --- a/src/a.ts\n\
             +++ b/src/a.ts\n\
             @@ -1,1 +1,4 @@\n\
             \x20first\n\
             +added one\n\
             +added two\n\
             +added three\n",
        );

        assert_eq!(set.files[0].change, modified(&[(2, 3)]));
    }

    #[test]
    fn a_pure_deletion_is_a_zero_width_position() {
        let set = parse(
            "diff --git a/src/a.ts b/src/a.ts\n\
             --- a/src/a.ts\n\
             +++ b/src/a.ts\n\
             @@ -1,3 +1,2 @@\n\
             \x20first\n\
             -gone\n\
             \x20third\n",
        );

        assert_eq!(set.files[0].change, modified(&[(2, 0)]));
    }

    #[test]
    fn parses_several_hunks_in_one_file() {
        let set = parse(
            "diff --git a/src/a.ts b/src/a.ts\n\
             --- a/src/a.ts\n\
             +++ b/src/a.ts\n\
             @@ -1,2 +1,2 @@\n\
             -one\n\
             +ONE\n\
             \x20two\n\
             @@ -20,2 +20,2 @@ fn context()\n\
             \x20twenty\n\
             -twentyone\n\
             +TWENTYONE\n",
        );

        assert_eq!(
            set.files[0].change,
            modified(&[(1, 0), (1, 1), (21, 0), (21, 1)])
        );
    }

    #[test]
    fn parses_several_files() {
        let set = parse(
            "diff --git a/src/a.ts b/src/a.ts\n\
             --- a/src/a.ts\n\
             +++ b/src/a.ts\n\
             @@ -1,1 +1,1 @@\n\
             -one\n\
             +ONE\n\
             diff --git a/src/b.ts b/src/b.ts\n\
             --- a/src/b.ts\n\
             +++ b/src/b.ts\n\
             @@ -5,1 +5,1 @@\n\
             -five\n\
             +FIVE\n",
        );

        assert_eq!(set.files.len(), 2);
        assert_eq!(set.files[0].path, PathBuf::from("src/a.ts"));
        assert_eq!(set.files[1].path, PathBuf::from("src/b.ts"));
        assert_eq!(set.files[1].change, modified(&[(5, 0), (5, 1)]));
    }

    #[test]
    fn a_missing_count_means_one_line() {
        let set = parse(
            "diff --git a/src/a.ts b/src/a.ts\n\
             --- a/src/a.ts\n\
             +++ b/src/a.ts\n\
             @@ -7 +7 @@\n\
             -seven\n\
             +SEVEN\n",
        );

        assert_eq!(set.files[0].change, modified(&[(7, 0), (7, 1)]));
    }

    #[test]
    fn an_addition_at_end_of_file_without_a_newline() {
        let set = parse(
            "diff --git a/src/a.ts b/src/a.ts\n\
             --- a/src/a.ts\n\
             +++ b/src/a.ts\n\
             @@ -3,1 +3,2 @@\n\
             \x20third\n\
             +fourth\n\
             \\ No newline at end of file\n",
        );

        assert_eq!(set.files[0].change, modified(&[(4, 1)]));
    }

    #[test]
    fn a_new_file_is_modified_at_its_after_path() {
        let set = parse(
            "diff --git a/src/new.ts b/src/new.ts\n\
             new file mode 100644\n\
             --- /dev/null\n\
             +++ b/src/new.ts\n\
             @@ -0,0 +1,2 @@\n\
             +one\n\
             +two\n",
        );

        assert_eq!(set.files[0].path, PathBuf::from("src/new.ts"));
        assert_eq!(set.files[0].change, modified(&[(1, 2)]));
    }

    #[test]
    fn a_deleted_file_is_recorded_as_deleted() {
        let set = parse(
            "diff --git a/src/gone.ts b/src/gone.ts\n\
             deleted file mode 100644\n\
             --- a/src/gone.ts\n\
             +++ /dev/null\n\
             @@ -1,2 +0,0 @@\n\
             -one\n\
             -two\n",
        );

        assert_eq!(set.files[0].path, PathBuf::from("src/gone.ts"));
        assert_eq!(set.files[0].change, FileChange::Deleted);
    }

    #[test]
    fn a_deleted_line_starting_with_dashes_is_body_not_a_header() {
        // The hunk's stated counts are what end the body, so content that looks like
        // a header cannot truncate it.
        let set = parse(
            "diff --git a/src/a.ts b/src/a.ts\n\
             --- a/src/a.ts\n\
             +++ b/src/a.ts\n\
             @@ -1,2 +1,2 @@\n\
             --- not a header\n\
             +++ also not a header\n\
             \x20context\n",
        );

        assert_eq!(set.files.len(), 1);
        assert_eq!(set.files[0].path, PathBuf::from("src/a.ts"));
        assert_eq!(set.files[0].change, modified(&[(1, 0), (1, 1)]));
    }

    #[test]
    fn a_binary_file_is_opaque() {
        let set = parse(
            "diff --git a/src/logo.png b/src/logo.png\n\
             index 1111111..2222222 100644\n\
             Binary files a/src/logo.png and b/src/logo.png differ\n",
        );

        assert_eq!(set.files[0].path, PathBuf::from("src/logo.png"));
        assert_eq!(set.files[0].change, FileChange::Opaque);
    }

    #[test]
    fn a_rename_without_hunks_is_opaque_at_the_new_path() {
        let set = parse(
            "diff --git a/src/old.ts b/src/new.ts\n\
             similarity index 100%\n\
             rename from src/old.ts\n\
             rename to src/new.ts\n",
        );

        assert_eq!(set.files.len(), 1);
        assert_eq!(set.files[0].path, PathBuf::from("src/new.ts"));
        assert_eq!(set.files[0].change, FileChange::Opaque);
    }

    #[test]
    fn a_rename_with_modification_keeps_its_hunks() {
        let set = parse(
            "diff --git a/src/old.ts b/src/new.ts\n\
             similarity index 88%\n\
             rename from src/old.ts\n\
             rename to src/new.ts\n\
             --- a/src/old.ts\n\
             +++ b/src/new.ts\n\
             @@ -2,1 +2,1 @@\n\
             -old\n\
             +new\n",
        );

        assert_eq!(set.files[0].path, PathBuf::from("src/new.ts"));
        assert_eq!(set.files[0].change, modified(&[(2, 0), (2, 1)]));
    }

    #[test]
    fn handles_paths_containing_spaces() {
        let set = parse(
            "diff --git a/src/my component.tsx b/src/my component.tsx\n\
             --- a/src/my component.tsx\n\
             +++ b/src/my component.tsx\n\
             @@ -1,1 +1,1 @@\n\
             -a\n\
             +b\n",
        );

        assert_eq!(set.files[0].path, PathBuf::from("src/my component.tsx"));
    }

    #[test]
    fn handles_git_quoted_paths() {
        let set = parse(
            "diff --git \"a/src/caf\\303\\251.ts\" \"b/src/caf\\303\\251.ts\"\n\
             --- \"a/src/caf\\303\\251.ts\"\n\
             +++ \"b/src/caf\\303\\251.ts\"\n\
             @@ -1,1 +1,1 @@\n\
             -a\n\
             +b\n",
        );

        assert_eq!(set.files.len(), 1);
        assert_eq!(set.files[0].change, modified(&[(1, 0), (1, 1)]));
    }

    #[test]
    fn a_plain_diff_u_with_timestamps_still_parses() {
        let set = parse(
            "--- src/a.ts\t2026-01-01 10:00:00.000000000 +0000\n\
             +++ src/a.ts\t2026-01-02 10:00:00.000000000 +0000\n\
             @@ -1,1 +1,1 @@\n\
             -a\n\
             +b\n",
        );

        assert_eq!(set.files[0].path, PathBuf::from("src/a.ts"));
        assert_eq!(set.files[0].change, modified(&[(1, 0), (1, 1)]));
    }

    #[test]
    fn an_empty_diff_yields_nothing() {
        assert!(parse("").is_empty());
        assert!(parse("commit 1234\n\n    a message\n").is_empty());
    }
}
