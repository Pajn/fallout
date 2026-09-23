//! Argument parsing, result rendering and exit codes.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::Parser as ClapParser;

use crate::query::{Direction, Hit};
use crate::{Granularity, Options, Outcome, Verdict, analyse, canonical_root};

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
}

pub fn run() -> ExitCode {
    let cli = Cli::parse();

    let diff = match cli.diff.as_deref().map(read_diff) {
        Some(Ok(text)) => Some(text),
        Some(Err(message)) => {
            eprintln!("Error: {}", message);
            return ExitCode::FAILURE;
        }
        None => None,
    };

    let (explain, list_unresolved) = (cli.explain, cli.unresolved);
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
            ExitCode::FAILURE
        }
    }
}

fn report(outcome: &Outcome, explain: bool, root: &Path, granularity: Granularity) -> ExitCode {
    match &outcome.verdict {
        Verdict::Affected(hit) => {
            println!(
                "Impact detected on target anchor via: {:?}",
                hit.changed_file()
            );
            if explain {
                print_explanation(hit, &canonical_root(root), granularity);
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
