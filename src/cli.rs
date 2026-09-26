//! Argument parsing, result rendering and exit codes.
//!
//! The exit code is the verdict: 0 when a change reaches the anchors, 1 when it
//! does not, 2 when there is no answer — a bad argument, an anchor that is not there,
//! a `fallout.toml` that cannot be read. With `--json` it says only whether there
//! was an answer, since the answers are in the output: 0 or 2.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::Parser as ClapParser;

use crate::query::{Direction, Hit};
use crate::{
    AnchorOutcome, Granularity, Options, Outcome, Verdict, analyse, analyse_each, canonical_root,
};

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
    /// could not place, each classed as `path`, `alias`, `package` (installed, but
    /// nothing it offers matches `[resolve]`) or `missing-package`. The first three
    /// name something in this repository.
    #[arg(long)]
    pub json: bool,
}

/// Exit code 2: no answer.
fn failure() -> ExitCode {
    ExitCode::from(2)
}

pub fn run() -> ExitCode {
    let cli = Cli::parse();

    let diff = match cli.diff.as_deref().map(read_diff) {
        Some(Ok(text)) => Some(text),
        Some(Err(message)) => {
            eprintln!("Error: {}", message);
            return failure();
        }
        None => None,
    };

    let (explain, list_unresolved, json) = (cli.explain, cli.unresolved, cli.json);
    let root = cli.root.unwrap_or_else(|| PathBuf::from("."));
    let options = Options {
        anchors: cli.anchor,
        changed: cli.changed,
        diff,
        base: cli.base,
        root: root.clone(),
        only: cli.only,
        granularity: cli.granularity,
        include_types: cli.include_types,
    };

    if json {
        return match analyse_each(&options) {
            Ok(answers) => {
                println!(
                    "{}",
                    render_json(&answers, &canonical_root(&root), options.granularity)
                );
                ExitCode::SUCCESS
            }
            Err(error) => {
                eprintln!("Error: {}", error);
                failure()
            }
        };
    }

    match analyse(&options) {
        Ok(outcome) => {
            let code = report(&outcome, explain, &root, options.granularity);
            if list_unresolved {
                print_unresolved(&outcome, &canonical_root(&root));
            }
            code
        }
        Err(error) => {
            eprintln!("Error: {}", error);
            failure()
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

fn report(outcome: &Outcome, explain: bool, root: &Path, granularity: Granularity) -> ExitCode {
    match &outcome.verdict {
        Verdict::Affected(hit) => {
            let root = canonical_root(root);
            println!(
                "Impact detected on target anchor via: {:?}",
                display_path(hit.changed_file(), &root)
            );
            if explain {
                print_explanation(hit, &root, granularity);
            }
            ExitCode::SUCCESS
        }
        Verdict::NotAffected => {
            println!("No reachability impact detected");
            ExitCode::FAILURE
        }
    }
}

/// Every specifier the run could not place on disk, and who wrote it.
///
/// Printed after the verdict and never instead of it. An unresolved specifier is not
/// by itself a fault — a package nobody installed on this machine looks exactly the
/// same — so this reports and does not judge.
fn print_unresolved(outcome: &Outcome, root: &Path) {
    if outcome.unresolved.is_empty() {
        println!("\nEvery specifier this run reached resolved to a file.");
        return;
    }
    let writers: usize = outcome.unresolved.iter().map(|(_, from)| from.len()).sum();
    println!(
        "\nResolved to nothing: {} specifier(s), written in {} file(s).",
        outcome.unresolved.len(),
        writers
    );
    for (specifier, from) in &outcome.unresolved {
        println!("  {specifier}");
        for file in from {
            println!("    {}", display_path(file, root));
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
fn print_explanation(hit: &Hit, root: &Path, granularity: Granularity) {
    println!(
        "Path ({}, {} granularity):",
        hit.direction.as_str(),
        granularity.as_str()
    );

    // A symbol run carries the nodes it actually walked; a file run has only paths.
    match &hit.rendered {
        Some(nodes) => {
            for node in nodes {
                println!("  {}", node);
            }
        }
        None => {
            for file in &hit.path {
                println!("  File({})", display_path(file, root));
            }
        }
    }
}

/// Relative to the root where possible, always with forward slashes, so that an
/// explanation reads the same everywhere.
fn display_path(path: &Path, root: &Path) -> String {
    crate::graph::display_path(path, root)
}
