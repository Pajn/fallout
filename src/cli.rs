//! Argument parsing, result rendering and exit codes.
//!
//! The exit code is the verdict: 0 when a change reaches the anchors, 1 when it
//! does not, 2 when there is no answer — a bad argument, an anchor that is not there,
//! a `fallout.toml` or a tsconfig that cannot be read, a `--base` revision git cannot
//! find. With
//! `--json` it says only whether there was an answer, since the answers are in the
//! output: 0 or 2.

use std::ffi::OsString;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::Parser as ClapParser;

use crate::base::Earlier;
use crate::query::{Direction, Hit};
use crate::{AnchorOutcome, Granularity, Options, Outcome, Verdict, analysed, analysed_each};

/// Writes a line where standard output would have it, the way `println!` does: a
/// stream that cannot be written to stops the run as it would stop `println!`.
macro_rules! say {
    ($out:expr, $($format:tt)*) => {
        if let Err(error) = writeln!($out, $($format)*) {
            panic!("failed printing to stdout: {}", error);
        }
    };
}

/// The same for standard error, as `eprintln!` does.
macro_rules! say_error {
    ($err:expr, $($format:tt)*) => {
        if let Err(error) = writeln!($err, $($format)*) {
            panic!("failed printing to stderr: {}", error);
        }
    };
}

#[derive(ClapParser)]
#[command(about = "Decide whether a change can reach a page, by walking the import graph")]
pub struct Cli {
    /// Target page component(s) (e.g., src/pages/CheckoutPage.tsx)
    #[arg(short, long, value_delimiter = ' ')]
    pub anchor: Vec<PathBuf>,

    /// Changed files passed from git diff
    #[arg(short, long, value_delimiter = ' ')]
    pub changed: Vec<PathBuf>,

    /// Unified diff describing the change; "-" reads standard input
    #[arg(short, long)]
    pub diff: Option<PathBuf>,

    /// Git revision to compare against, so that comment-only and formatting-only
    /// changes mark nothing (e.g. origin/main)
    #[arg(short, long)]
    pub base: Option<String>,

    /// Root directory to scan for source files (default: anchor's parent or current dir)
    #[arg(short, long)]
    pub root: Option<PathBuf>,

    /// Search only one direction (default: downstream, then upstream)
    #[arg(short, long, value_enum)]
    pub only: Option<Direction>,

    /// How finely to distinguish parts of a file
    #[arg(short, long, value_enum, default_value = "file")]
    pub granularity: Granularity,

    /// Count a change made only of types as a change, which by default it is not
    #[arg(long)]
    pub include_types: bool,

    /// Print the chain of imports that produced the verdict
    #[arg(short, long)]
    pub explain: bool,

    /// List the import specifiers this run reached and could not place on disk
    ///
    /// Each one is an edge the graph does not have. Some name no file and never
    /// will — a package nobody installed here, a virtual module the bundler makes —
    /// so this is a report to read rather than a check that passes or fails.
    ///
    /// It covers what the run reached. A search that stops at the first change it
    /// finds has not looked at the rest of the graph, and does not report on it.
    #[arg(long)]
    pub unresolved: bool,

    /// Answer each anchor on its own, in one run, and print the answers as JSON
    ///
    /// Each answer carries the chain that produced it and the specifiers its search
    /// could not place, each classed as `path`, `alias`, `package` (installed or the
    /// repository's own, but nothing it offers matches `[resolve]`), `missing-package`
    /// or `installed` (written by an installed package's own code). The first three
    /// name something in this repository.
    #[arg(long, conflicts_with_all = ["explain", "unresolved"])]
    pub json: bool,
}

/// Exit code 0: affected.
const AFFECTED: u8 = 0;
/// Exit code 0 with `--json`: answered, whatever the answers are.
const ANSWERED: u8 = 0;
/// Exit code 1: not affected.
const NOT_AFFECTED: u8 = 1;
/// Exit code 2: no answer.
const NO_ANSWER: u8 = 2;

/// The command line, run in this process: parses `args`, the program's name first,
/// reads the diff, analyses, writes what the binary would print to `out` and `err`,
/// and returns the exit code the binary would end with.
///
/// Arguments clap turns away, and `--help`, end the run with clap's own text and
/// exit code rather than ending the process. The text is plain, since neither
/// stream here is known to be a terminal. `--diff -` reads this process's standard
/// input.
///
/// `earlier` stands in for whatever `--base` names: when it is given, the versions
/// of the files before the change are read from it and the revision is not read.
/// Without it, a `--base` revision is read from git as usual, and one git cannot
/// find is no answer.
///
/// # Panics
///
/// When writing to `out` or `err` fails, as `println!` does for the binary.
pub fn execute<I, T>(
    args: I,
    earlier: Option<Box<dyn Earlier>>,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> u8
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    match Cli::try_parse_from(args) {
        Ok(cli) => answer(cli, earlier, out, err),
        Err(error) => {
            let stream: (&str, &mut dyn Write) = if error.use_stderr() {
                ("stderr", err)
            } else {
                ("stdout", out)
            };
            if let Err(failure) = write!(stream.1, "{}", error.render()) {
                panic!("failed printing to {}: {}", stream.0, failure);
            }
            // Clap's codes are 0 and 2, which it spells as `i32` for `exit`.
            u8::try_from(error.exit_code()).unwrap_or(NO_ANSWER)
        }
    }
}

/// The binary's entry point: [`execute`] over the process's own arguments and
/// streams.
///
/// Clap parses the arguments itself here and prints what it turns away the way it
/// always has, colouring the text when it goes to a terminal that takes colour.
/// [`execute`], which writes to streams it cannot ask, prints the same text plain.
pub fn run() -> ExitCode {
    let cli = Cli::parse();
    ExitCode::from(answer(
        cli,
        None,
        &mut std::io::stdout(),
        &mut std::io::stderr(),
    ))
}

/// Everything after parsing: the run, what it prints and the exit code it ends with.
fn answer(
    cli: Cli,
    earlier: Option<Box<dyn Earlier>>,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> u8 {
    let diff = match cli.diff.as_deref().map(read_diff) {
        Some(Ok(text)) => Some(text),
        Some(Err(message)) => {
            say_error!(err, "Error: {}", message);
            return NO_ANSWER;
        }
        None => None,
    };

    let (explain, list_unresolved, json) = (cli.explain, cli.unresolved, cli.json);
    let options = Options {
        anchors: cli.anchor,
        changed: cli.changed,
        diff,
        base: cli.base,
        root: cli.root.unwrap_or_else(|| PathBuf::from(".")),
        only: cli.only,
        granularity: cli.granularity,
        include_types: cli.include_types,
    };
    // Earlier versions handed over are taken as they are. Only a revision read from
    // git is checked, since git may not know it.
    let earlier = match earlier {
        Some(given) => Ok(Some(given)),
        None => crate::earlier(&options),
    };
    let earlier = match earlier {
        Ok(earlier) => earlier,
        Err(error) => {
            say_error!(err, "Error: {}", error);
            return NO_ANSWER;
        }
    };

    // Paths are displayed against the root the run itself measured from, so what is
    // printed names exactly the files that were analysed.
    if json {
        return match analysed_each(&options, earlier) {
            Ok((answers, root)) => {
                say!(out, "{}", render_json(&answers, &root, options.granularity));
                ANSWERED
            }
            Err(error) => {
                say_error!(err, "Error: {}", error);
                NO_ANSWER
            }
        };
    }

    match analysed(&options, earlier) {
        Ok((outcome, root)) => {
            let code = report(out, &outcome, explain, &root, options.granularity);
            if list_unresolved {
                print_unresolved(out, &outcome, &root);
            }
            code
        }
        Err(error) => {
            say_error!(err, "Error: {}", error);
            NO_ANSWER
        }
    }
}

/// The answers as one JSON document, anchors in the order they were given.
///
/// ```json
/// {"anchors": [{"anchor": "src/pages/A.tsx", "affected": true,
///   "direction": "downstream", "changed": "src/lib/x.ts", "path": ["File(...)", ...],
///   "unresolved": [{"specifier": "./gone", "kind": "path", "in_repo": true,
///                   "from": ["src/pages/A.tsx"]}]}]}
/// ```
fn render_json(answers: &[AnchorOutcome], root: &Path, granularity: Granularity) -> String {
    let mut out = String::from("{\"anchors\":[");
    for (index, answer) in answers.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        out.push_str("{\"anchor\":");
        push_string(&mut out, &display_path(&answer.anchor, root));
        match &answer.verdict {
            Verdict::Affected(hit) => {
                out.push_str(",\"affected\":true,\"direction\":");
                push_string(&mut out, hit.direction.as_str());
                out.push_str(",\"changed\":");
                push_string(&mut out, &display_path(hit.changed_file(), root));
                out.push_str(",\"path\":[");
                let nodes: Vec<String> = match &hit.rendered {
                    Some(nodes) => nodes.clone(),
                    None => hit
                        .path
                        .iter()
                        .map(|file| format!("File({})", display_path(file, root)))
                        .collect(),
                };
                for (index, node) in nodes.iter().enumerate() {
                    if index > 0 {
                        out.push(',');
                    }
                    push_string(&mut out, node);
                }
                out.push(']');
            }
            Verdict::NotAffected => out.push_str(",\"affected\":false"),
        }
        out.push_str(",\"granularity\":");
        push_string(&mut out, granularity.as_str());
        out.push_str(",\"unresolved\":[");
        for (index, import) in answer.unresolved.iter().enumerate() {
            if index > 0 {
                out.push(',');
            }
            out.push_str("{\"specifier\":");
            push_string(&mut out, &import.specifier);
            out.push_str(",\"kind\":");
            push_string(&mut out, import.kind.as_str());
            out.push_str(",\"in_repo\":");
            out.push_str(if import.kind.in_repo() {
                "true"
            } else {
                "false"
            });
            out.push_str(",\"from\":[");
            for (index, file) in import.from.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                push_string(&mut out, &display_path(file, root));
            }
            out.push_str("]}");
        }
        out.push_str("]}");
    }
    out.push_str("]}");
    out
}

/// A JSON string literal.
fn push_string(out: &mut String, text: &str) {
    out.push('"');
    for character in text.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            character if (character as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", character as u32));
            }
            character => out.push(character),
        }
    }
    out.push('"');
}

/// Prints the verdict, and returns the exit code that says it.
fn report(
    out: &mut dyn Write,
    outcome: &Outcome,
    explain: bool,
    root: &Path,
    granularity: Granularity,
) -> u8 {
    match &outcome.verdict {
        Verdict::Affected(hit) => {
            say!(
                out,
                "Impact detected on target anchor via: {:?}",
                display_path(hit.changed_file(), root)
            );
            if explain {
                print_explanation(out, hit, root, granularity);
            }
            AFFECTED
        }
        Verdict::NotAffected => {
            say!(out, "No reachability impact detected");
            NOT_AFFECTED
        }
    }
}

/// Every specifier the run could not place on disk, and who wrote it.
///
/// Printed after the verdict and never instead of it. An unresolved specifier is not
/// by itself a fault — a package nobody installed on this machine looks exactly the
/// same — so this reports and does not judge.
fn print_unresolved(out: &mut dyn Write, outcome: &Outcome, root: &Path) {
    if outcome.unresolved.is_empty() {
        say!(
            out,
            "\nEvery specifier this run reached resolved to a file."
        );
        return;
    }
    let writers: usize = outcome.unresolved.iter().map(|(_, from)| from.len()).sum();
    say!(
        out,
        "\nResolved to nothing: {} specifier(s), written in {} file(s).",
        outcome.unresolved.len(),
        writers
    );
    for (specifier, from) in &outcome.unresolved {
        say!(out, "  {specifier}");
        for file in from {
            say!(out, "    {}", display_path(file, root));
        }
    }
}

fn read_diff(path: &Path) -> Result<String, String> {
    if path.as_os_str() == "-" {
        let mut text = String::new();
        return std::io::stdin()
            .read_to_string(&mut text)
            .map(|_| text)
            .map_err(|e| format!("could not read diff from stdin: {}", e));
    }

    std::fs::read_to_string(path)
        .map_err(|e| format!("could not read diff at {}: {}", path.display(), e))
}

/// Prints the chain in import order, one node per line.
///
/// Node names match the vocabulary in `docs/symbol-level-analysis.md`, so a path
/// stays comparable as finer node kinds arrive.
fn print_explanation(out: &mut dyn Write, hit: &Hit, root: &Path, granularity: Granularity) {
    say!(
        out,
        "Path ({}, {} granularity):",
        hit.direction.as_str(),
        granularity.as_str()
    );

    // A symbol run carries the nodes it actually walked; a file run has only paths.
    match &hit.rendered {
        Some(nodes) => {
            for node in nodes {
                say!(out, "  {}", node);
            }
        }
        None => {
            for file in &hit.path {
                say!(out, "  File({})", display_path(file, root));
            }
        }
    }
}

/// Relative to the root where possible, always with forward slashes, so that an
/// explanation reads the same everywhere.
fn display_path(path: &Path, root: &Path) -> String {
    crate::graph::display_path(path, root)
}
