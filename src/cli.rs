//! Argument parsing, result rendering and exit codes.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::Parser as ClapParser;

use crate::query::{Direction, Hit};
use crate::{Granularity, Options, Verdict, analyse, canonical_root};

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

    /// Root directory to scan for source files (default: anchor's parent or current dir)
    #[arg(short, long)]
    pub root: Option<PathBuf>,

    /// Search only one direction (default: downstream, then upstream)
    #[arg(short, long, value_enum)]
    pub only: Option<Direction>,

    /// How finely to distinguish parts of a file
    #[arg(short, long, value_enum, default_value = "file")]
    pub granularity: Granularity,

    /// Print the chain of imports that produced the verdict
    #[arg(short, long)]
    pub explain: bool,
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

    let root = cli.root.unwrap_or_else(|| PathBuf::from("."));
    let options = Options {
        anchors: cli.anchor,
        changed: cli.changed,
        diff,
        root: root.clone(),
        only: cli.only,
        granularity: cli.granularity,
    };

    match analyse(&options) {
        Ok(Verdict::Affected(hit)) => {
            println!(
                "Impact detected on target anchor via: {:?}",
                hit.changed_file()
            );
            if cli.explain {
                print_explanation(&hit, &canonical_root(&root), options.granularity);
            }
            ExitCode::SUCCESS
        }
        Ok(Verdict::NotAffected) => {
            println!("No reachability impact detected");
            ExitCode::FAILURE
        }
        Err(error) => {
            eprintln!("Error: {}", error);
            ExitCode::FAILURE
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
    for file in &hit.path {
        println!("  File({})", display_path(file, root));
    }
}

/// Relative to the root where possible, always with forward slashes, so that an
/// explanation reads the same everywhere.
fn display_path(path: &Path, root: &Path) -> String {
    let relative = path.strip_prefix(root).unwrap_or(path);
    relative
        .components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}
