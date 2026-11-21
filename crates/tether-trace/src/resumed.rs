//! Reading a trace that was resumed.
//!
//! A trace is append-only, so a resumed run keeps everything it did before
//! it halted. Resume goes back to the last checkpoint and does the step that
//! was under way again, which leaves that step in the trace twice: once
//! abandoned, once done. A resume is marked by a `run.state` of `running`
//! with the reason `resumed`.

use tether_types::{Event, EventType, RunState};

/// The reason on the `run.state` that marks a resume.
pub const RESUMED: &str = "resumed";

/// True for the `run.state` event that marks a resume.
pub fn is_resume(event: &Event) -> bool {
    matches!(
        &event.body,
        EventType::RunStateChanged {
            state: RunState::Running,
            reason,
        } if reason == RESUMED
    )
}

/// The events of the run as it finally went: everything except what each
/// resume abandoned, which is the stretch between the checkpoint it went
/// back to and its resume marker. A trace never resumed comes back whole.
pub fn effective(events: &[Event]) -> Vec<&Event> {
    let mut kept: Vec<&Event> = Vec::with_capacity(events.len());
    for event in events {
        if is_resume(event) {
            // Back to the last checkpoint still standing, or to the start of
            // the run if there is none.
            let back_to = kept
                .iter()
                .rposition(|e| matches!(e.body, EventType::Checkpoint { .. }))
                .or_else(|| {
                    kept.iter()
                        .position(|e| matches!(e.body, EventType::RunStateChanged { .. }))
                });
            if let Some(at) = back_to {
                kept.truncate(at + 1);
            }
        }
        kept.push(event);
    }
    kept
}
