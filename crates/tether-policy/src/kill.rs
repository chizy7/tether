//! The kill switch: a way to stop a run from outside it.
//!
//! Two things trip it. A signal handler (the CLI's SIGINT) calls
//! [`KillSwitch::trip`], and a halt file, `<run>.halt` beside the trace,
//! trips it by existing. The switch only records that it was tripped; the
//! run loop asks [`KillSwitch::tripped`] at event boundaries and halts there,
//! so a run is never stopped in the middle of writing an event.

use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

#[derive(Debug, Default)]
struct Inner {
    /// Why the switch was tripped. The first reason is kept.
    reason: Option<String>,
    halt_files: Vec<PathBuf>,
}

/// Cheap to clone; every clone is the same switch.
#[derive(Debug, Clone, Default)]
pub struct KillSwitch {
    inner: Arc<Mutex<Inner>>,
}

impl KillSwitch {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Trip the switch. Safe to call from a signal-handling task.
    pub fn trip(&self, reason: impl Into<String>) {
        self.lock().reason.get_or_insert_with(|| reason.into());
    }

    /// Also trip when `path` exists.
    pub fn watch_file(&self, path: impl Into<PathBuf>) {
        self.lock().halt_files.push(path.into());
    }

    /// Why the switch is tripped, or `None` if it is not. Once tripped it
    /// stays tripped, even if the halt file is removed.
    pub fn tripped(&self) -> Option<String> {
        let mut inner = self.lock();
        if inner.reason.is_none() {
            let found = inner.halt_files.iter().find(|path| path.exists());
            inner.reason = found.map(|path| format!("halt file {}", path.display()));
        }
        inner.reason.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::KillSwitch;

    #[test]
    fn untripped_until_told_and_the_first_reason_sticks() {
        let kill = KillSwitch::new();
        assert_eq!(kill.tripped(), None);

        let handle = kill.clone();
        handle.trip("SIGINT");
        handle.trip("something later");
        assert_eq!(kill.tripped().as_deref(), Some("SIGINT"));
    }

    #[test]
    fn a_halt_file_trips_it_and_removing_the_file_does_not_untrip() {
        let dir = std::env::temp_dir().join(format!("tether-kill-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create dir");
        let file = dir.join("run.halt");
        let _ = std::fs::remove_file(&file);

        let kill = KillSwitch::new();
        kill.watch_file(&file);
        assert_eq!(kill.tripped(), None);

        std::fs::write(&file, "").expect("write halt file");
        let reason = kill.tripped().expect("tripped");
        assert_eq!(reason, format!("halt file {}", file.display()));

        std::fs::remove_dir_all(&dir).expect("clean up");
        assert_eq!(kill.tripped(), Some(reason));
    }
}
