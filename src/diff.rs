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
/// Unrecognised lines are ignored: a diff carries commit messages, index lines and
/// mode changes that say nothing about which lines moved.
pub fn parse(text: &str) -> ChangeSet {
    let mut files: Vec<ChangedFile> = Vec::new();
    // The file currently being read, kept aside until the next header so that late
    // markers (a binary notice, a `+++ /dev/null`) can still change its verdict.
    let mut current: Option<ChangedFile> = None;

    for line in text.lines() {
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
            if let Some(file) = current.as_mut() {
                if file.change != FileChange::Deleted {
                    file.change = FileChange::Opaque;
                }
            }
        } else if line.starts_with("@@") {
            if let Some(file) = current.as_mut() {
                if let (FileChange::Modified { ranges }, Some(range)) =
                    (&mut file.change, parse_hunk_header(line))
                {
                    ranges.push(range);
                }
            }
        }
    }
    flush(&mut files, current.take());

    ChangeSet { files }
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

/// `@@ -12,3 +14,6 @@ optional context` -> the after-side range, `14..20`.
///
/// A missing count means one line; a count of zero is a pure deletion, kept as a
/// zero-width position.
fn parse_hunk_header(line: &str) -> Option<LineRange> {
    let after = line.split('+').nth(1)?;
    let after = after.split_whitespace().next()?;
    let mut parts = after.splitn(2, ',');
    let start: u32 = parts.next()?.parse().ok()?;
    let len: u32 = match parts.next() {
        Some(count) => count.parse().ok()?,
        None => 1,
    };
    Some(LineRange { start, len })
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
    fn parses_a_single_hunk() {
        let set = parse(
            "diff --git a/src/a.ts b/src/a.ts\n\
             index 1111111..2222222 100644\n\
             --- a/src/a.ts\n\
             +++ b/src/a.ts\n\
             @@ -1,3 +1,4 @@\n\
             \x20context\n\
             -gone\n\
             +added\n",
        );

        assert_eq!(set.files.len(), 1);
        assert_eq!(set.files[0].path, PathBuf::from("src/a.ts"));
        assert_eq!(set.files[0].change, modified(&[(1, 4)]));
    }

    #[test]
    fn parses_several_hunks_in_one_file() {
        let set = parse(
            "diff --git a/src/a.ts b/src/a.ts\n\
             --- a/src/a.ts\n\
             +++ b/src/a.ts\n\
             @@ -1,3 +1,3 @@\n\
             @@ -20,4 +20,6 @@ fn context()\n",
        );

        assert_eq!(set.files[0].change, modified(&[(1, 3), (20, 6)]));
    }

    #[test]
    fn parses_several_files() {
        let set = parse(
            "diff --git a/src/a.ts b/src/a.ts\n\
             --- a/src/a.ts\n\
             +++ b/src/a.ts\n\
             @@ -1 +1 @@\n\
             diff --git a/src/b.ts b/src/b.ts\n\
             --- a/src/b.ts\n\
             +++ b/src/b.ts\n\
             @@ -5,2 +5,2 @@\n",
        );

        assert_eq!(set.files.len(), 2);
        assert_eq!(set.files[0].path, PathBuf::from("src/a.ts"));
        assert_eq!(set.files[1].path, PathBuf::from("src/b.ts"));
        assert_eq!(set.files[1].change, modified(&[(5, 2)]));
    }

    #[test]
    fn a_missing_count_means_one_line() {
        assert_eq!(
            parse_hunk_header("@@ -1 +7 @@"),
            Some(LineRange { start: 7, len: 1 })
        );
    }

    #[test]
    fn a_pure_deletion_is_a_zero_width_position() {
        let set = parse(
            "diff --git a/src/a.ts b/src/a.ts\n\
             --- a/src/a.ts\n\
             +++ b/src/a.ts\n\
             @@ -4,3 +3,0 @@\n\
             -gone\n",
        );

        assert_eq!(set.files[0].change, modified(&[(3, 0)]));
    }

    #[test]
    fn an_addition_at_end_of_file_keeps_its_range() {
        let set = parse(
            "diff --git a/src/a.ts b/src/a.ts\n\
             --- a/src/a.ts\n\
             +++ b/src/a.ts\n\
             @@ -3,0 +4,2 @@\n\
             +one\n\
             +two\n\
             \\ No newline at end of file\n",
        );

        assert_eq!(set.files[0].change, modified(&[(4, 2)]));
    }

    #[test]
    fn a_new_file_is_modified_at_its_after_path() {
        let set = parse(
            "diff --git a/src/new.ts b/src/new.ts\n\
             new file mode 100644\n\
             --- /dev/null\n\
             +++ b/src/new.ts\n\
             @@ -0,0 +1,2 @@\n",
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
             @@ -1,2 +0,0 @@\n",
        );

        assert_eq!(set.files[0].path, PathBuf::from("src/gone.ts"));
        assert_eq!(set.files[0].change, FileChange::Deleted);
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
             @@ -2,3 +2,4 @@\n",
        );

        assert_eq!(set.files[0].path, PathBuf::from("src/new.ts"));
        assert_eq!(set.files[0].change, modified(&[(2, 4)]));
    }

    #[test]
    fn handles_paths_containing_spaces() {
        let set = parse(
            "diff --git a/src/my component.tsx b/src/my component.tsx\n\
             --- a/src/my component.tsx\n\
             +++ b/src/my component.tsx\n\
             @@ -1 +1 @@\n",
        );

        assert_eq!(set.files[0].path, PathBuf::from("src/my component.tsx"));
    }

    #[test]
    fn handles_git_quoted_paths() {
        let set = parse(
            "diff --git \"a/src/caf\\303\\251.ts\" \"b/src/caf\\303\\251.ts\"\n\
             --- \"a/src/caf\\303\\251.ts\"\n\
             +++ \"b/src/caf\\303\\251.ts\"\n\
             @@ -1 +1 @@\n",
        );

        assert_eq!(set.files.len(), 1);
        assert_eq!(set.files[0].change, modified(&[(1, 1)]));
    }

    #[test]
    fn a_plain_diff_u_with_timestamps_still_parses() {
        let set = parse(
            "--- src/a.ts\t2026-01-01 10:00:00.000000000 +0000\n\
             +++ src/a.ts\t2026-01-02 10:00:00.000000000 +0000\n\
             @@ -1,3 +1,4 @@\n",
        );

        assert_eq!(set.files[0].path, PathBuf::from("src/a.ts"));
        assert_eq!(set.files[0].change, modified(&[(1, 4)]));
    }

    #[test]
    fn an_empty_diff_yields_nothing() {
        assert!(parse("").is_empty());
        assert!(parse("commit 1234\n\n    a message\n").is_empty());
    }
}
