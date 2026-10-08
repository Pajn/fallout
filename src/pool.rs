//! The threads a run reads files on.
//!
//! Reading files ahead of a walk spends most of its time opening them, and past a
//! handful of threads the kernel serializes those opens: more threads add CPU time
//! without finishing sooner. So the run keeps its own pool, small by default, and
//! leaves the global one to whoever embeds it.

use std::num::NonZeroUsize;
use std::sync::OnceLock;

use rayon::{ThreadPool, ThreadPoolBuilder};

/// Sets how many threads a run reads files on.
pub const THREADS_VAR: &str = "FALLOUT_THREADS";

/// The most threads a run uses unless told otherwise.
const DEFAULT_THREADS: usize = 6;

/// The thread count [`THREADS_VAR`] asks for, if it is set, or why it cannot be
/// one.
pub fn requested_threads() -> Result<Option<NonZeroUsize>, String> {
    let Some(value) = std::env::var_os(THREADS_VAR) else {
        return Ok(None);
    };
    value
        .to_str()
        .and_then(|text| text.trim().parse::<NonZeroUsize>().ok())
        .map(Some)
        .ok_or_else(|| format!("{THREADS_VAR} must be a positive whole number, not {value:?}"))
}

/// The run's pool, built on first use: as many threads as [`THREADS_VAR`] asks
/// for, or otherwise as many as the machine runs at once, up to
/// [`DEFAULT_THREADS`]. A value that is not a thread count is ignored here; the
/// command line refuses it before a run starts.
pub fn pool() -> &'static ThreadPool {
    static POOL: OnceLock<ThreadPool> = OnceLock::new();
    POOL.get_or_init(|| {
        let threads = requested_threads()
            .ok()
            .flatten()
            .map_or_else(default_threads, NonZeroUsize::get);
        ThreadPoolBuilder::new()
            .num_threads(threads)
            .thread_name(|index| format!("fallout-{index}"))
            .build()
            .expect("the threads to read files on could not be started")
    })
}

fn default_threads() -> usize {
    std::thread::available_parallelism()
        .map_or(1, NonZeroUsize::get)
        .min(DEFAULT_THREADS)
}
