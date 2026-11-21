//! The run loop.
//!
//! `tether-core` owns a run from `Created` to `Finished`, feeds every event to
//! the single trace writer, consults policy before every side effect, and
//! reads nondeterministic inputs (clock, rng, env) through traits so replay can
//! satisfy them from the trace.
//!
//! It has no opinion about the agent. The agent is a black box that emits
//! model requests and tool requests; this crate answers them.

/// Clock reads go through this so replay can substitute recorded values.
pub trait Clock: Send + Sync {
    /// Milliseconds since the Unix epoch.
    fn now_ms(&self) -> u64;
}

/// Random seeds go through this for the same reason.
pub trait Rng: Send + Sync {
    fn seed(&self) -> u64;
}

/// The real wall clock. This is the boundary: nothing else in the core reads
/// `std::time`.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_ms(&self) -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as u64)
    }
}

pub mod ask;
pub mod run;
mod trace_task;

pub use ask::{Answerer, AskOutcome, AskRequest, Unattended};
pub use run::{Run, RunConfig, RunDeps, RunError, SandboxRef};

pub mod replay {
    //! Re-run a trace with the replay adapter and a replay sandbox.
}
