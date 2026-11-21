//! Align two traces by step and report where they diverge.
//!
//! A diff compares what a run *did*, not when or how fast. Within each step
//! it lines up, in order:
//!
//! - every `model.request`: the message hashes, model, tools and params;
//! - every `tool.call`: the tool name and arguments;
//! - every `policy.decision`: the outcome, and for an `ask` how it was
//!   answered.
//!
//! and then how the two runs ended. Everything else is left out on purpose:
//! timestamps, timings and budgets differ between any two live runs,
//! provider-assigned ids differ too, and a model's reply shows up in the next
//! request's message hashes or in how the run ended.
//!
//! A trace that was resumed is compared as the run it became. The part of a
//! step that was under way when the run halted, and was done again after the
//! resume, is left out.

use std::collections::BTreeMap;

use tether_types::{Decision, Event, EventType, RunState};

/// What kind of thing two traces disagree about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum What {
    ModelRequest,
    ToolCall,
    PolicyDecision,
    /// How the run ended: its last state, or its outcome.
    End,
}

impl What {
    pub fn name(self) -> &'static str {
        match self {
            Self::ModelRequest => "model.request",
            Self::ToolCall => "tool.call",
            Self::PolicyDecision => "policy.decision",
            Self::End => "end of run",
        }
    }
}

/// One side of a divergence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Side {
    pub seq: u64,
    /// The event in one line.
    pub summary: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Divergence {
    pub step: u64,
    pub what: What,
    /// `None` when that trace has nothing at this position.
    pub a: Option<Side>,
    pub b: Option<Side>,
    /// What differs, in words.
    pub detail: String,
    /// For a `model.request` whose messages differ: the first message hash
    /// on each side that does not match, so a caller can show the messages.
    pub messages: Option<MessagePair>,
}

/// A message hash from each trace; `None` where that trace has no message
/// at the position.
pub type MessagePair = (Option<String>, Option<String>);

/// How many of each compared event a step has in one trace.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Counts {
    pub model_requests: usize,
    pub tool_calls: usize,
    pub decisions: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StepSummary {
    pub step: u64,
    /// `None` when the trace has no compared events in this step.
    pub a: Option<Counts>,
    pub b: Option<Counts>,
    pub divergences: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TraceDiff {
    /// One entry per step either trace has, in order.
    pub steps: Vec<StepSummary>,
    /// Every divergence, in trace order. The first is the one that matters;
    /// after it the runs are doing different things.
    pub divergences: Vec<Divergence>,
    /// Whether the policy recorded in `run.created` differs. Not itself a
    /// divergence: a changed policy matters only where a decision changes.
    pub policy_differs: bool,
}

impl TraceDiff {
    pub fn is_empty(&self) -> bool {
        self.divergences.is_empty()
    }

    pub fn first(&self) -> Option<&Divergence> {
        self.divergences.first()
    }
}

/// A compared event, reduced to the fields that are compared.
#[derive(Debug, Clone, PartialEq)]
enum Item {
    ModelRequest {
        model: String,
        messages: Vec<String>,
        tools: Vec<String>,
        params: serde_json::Value,
    },
    ToolCall {
        name: String,
        arguments: serde_json::Value,
    },
    PolicyDecision {
        outcome: &'static str,
        /// For an `ask`: `Some(allow)` once answered.
        answer: Option<bool>,
        /// Shown, not compared: a different rule reaching the same outcome
        /// is not a divergence.
        note: String,
        /// `seq` of the call the decision is about, to pair it with its
        /// answer. Not compared.
        subject: u64,
    },
}

struct Entry {
    seq: u64,
    item: Item,
}

impl Item {
    fn what(&self) -> What {
        match self {
            Self::ModelRequest { .. } => What::ModelRequest,
            Self::ToolCall { .. } => What::ToolCall,
            Self::PolicyDecision { .. } => What::PolicyDecision,
        }
    }

    fn summary(&self) -> String {
        match self {
            Self::ModelRequest {
                model,
                messages,
                tools,
                ..
            } => format!(
                "{model}, {} messages, {} tools",
                messages.len(),
                tools.len()
            ),
            Self::ToolCall { name, arguments } => format!("{name} {arguments}"),
            Self::PolicyDecision {
                outcome,
                answer,
                note,
                ..
            } => {
                let answered = match answer {
                    Some(true) => ", answered allow",
                    Some(false) => ", answered deny",
                    None => "",
                };
                format!("{outcome}{answered}{note}")
            }
        }
    }

    /// Whether two items of the same kind agree on what is compared.
    fn same_as(&self, other: &Self) -> bool {
        match (self, other) {
            (
                Self::PolicyDecision {
                    outcome, answer, ..
                },
                Self::PolicyDecision {
                    outcome: other_outcome,
                    answer: other_answer,
                    ..
                },
            ) => outcome == other_outcome && answer == other_answer,
            _ => self == other,
        }
    }
}

/// Everything compared in one trace.
#[derive(Default)]
struct Compared {
    steps: BTreeMap<u64, Vec<Entry>>,
    policy: Option<serde_json::Value>,
    /// The last `run.state`: its seq, state and reason.
    last_state: Option<(u64, RunState, String)>,
    /// `run.finished.outcome` and its seq.
    outcome: Option<(u64, serde_json::Value)>,
}

fn compared(events: &[Event]) -> Compared {
    let mut out = Compared::default();
    // A resumed run is compared as the run it became: what it did before
    // each resume and then abandoned is left out.
    for event in crate::resumed::effective(events) {
        let item = match &event.body {
            EventType::RunCreated(data) => {
                out.policy = Some(data["policy"].clone());
                continue;
            }
            EventType::RunStateChanged { state, reason } => {
                out.last_state = Some((event.seq, *state, reason.clone()));
                continue;
            }
            EventType::RunFinished(data) => {
                out.outcome = Some((event.seq, data["outcome"].clone()));
                continue;
            }
            EventType::PolicyAnswer { subject, allow, .. } => {
                // The answer completes the `ask` decision about `subject`.
                let asked =
                    out.steps.values_mut().flatten().rev().find_map(|entry| {
                        match &mut entry.item {
                            Item::PolicyDecision {
                                answer,
                                subject: about,
                                ..
                            } if about == subject => Some(answer),
                            _ => None,
                        }
                    });
                if let Some(answer) = asked {
                    *answer = Some(*allow);
                }
                continue;
            }
            EventType::ModelRequest(request) => Item::ModelRequest {
                model: request.model.clone(),
                messages: request.messages.clone(),
                tools: request.tools.clone(),
                params: request.params.clone(),
            },
            EventType::ToolCall(call) => Item::ToolCall {
                name: call.name.clone(),
                arguments: call.arguments.clone(),
            },
            EventType::PolicyDecision {
                decision,
                rule,
                subject,
                ..
            } => {
                let (outcome, why) = match decision {
                    Decision::Allow => ("allow", None),
                    Decision::Deny { reason } => ("deny", Some(reason.as_str())),
                    Decision::Ask { .. } => ("ask", None),
                };
                let rule = rule
                    .as_deref()
                    .map_or("no rule".to_owned(), |r| format!("rule {r}"));
                let note = match why {
                    Some(why) => format!(" ({rule}: {why})"),
                    None => format!(" ({rule})"),
                };
                Item::PolicyDecision {
                    outcome,
                    answer: None,
                    note,
                    subject: *subject,
                }
            }
            _ => continue,
        };
        out.steps.entry(event.step).or_default().push(Entry {
            seq: event.seq,
            item,
        });
    }
    out
}

fn counts(entries: &[Entry]) -> Counts {
    let mut counts = Counts::default();
    for entry in entries {
        match entry.item {
            Item::ModelRequest { .. } => counts.model_requests += 1,
            Item::ToolCall { .. } => counts.tool_calls += 1,
            Item::PolicyDecision { .. } => counts.decisions += 1,
        }
    }
    counts
}

fn side(entry: &Entry) -> Side {
    Side {
        seq: entry.seq,
        summary: entry.item.summary(),
    }
}

/// What differs between two items, and for requests the first unequal
/// message on each side.
fn explain(a: &Item, b: &Item) -> (String, Option<MessagePair>) {
    match (a, b) {
        (
            Item::ModelRequest {
                model,
                messages,
                tools,
                params,
            },
            Item::ModelRequest {
                model: other_model,
                messages: other_messages,
                tools: other_tools,
                params: other_params,
            },
        ) => {
            if messages != other_messages {
                let at = messages
                    .iter()
                    .zip(other_messages)
                    .position(|(x, y)| x != y)
                    .unwrap_or(messages.len().min(other_messages.len()));
                let detail = format!(
                    "message {} differs ({} messages in a, {} in b)",
                    at + 1,
                    messages.len(),
                    other_messages.len()
                );
                let pair = (messages.get(at).cloned(), other_messages.get(at).cloned());
                (detail, Some(pair))
            } else if model != other_model {
                (
                    format!("model differs: {model} in a, {other_model} in b"),
                    None,
                )
            } else if tools != other_tools {
                ("the tools offered differ".into(), None)
            } else {
                debug_assert_ne!(params, other_params);
                ("the request params differ".into(), None)
            }
        }
        (
            Item::ToolCall { name, .. },
            Item::ToolCall {
                name: other_name, ..
            },
        ) => {
            if name != other_name {
                ("a different tool is called".into(), None)
            } else {
                ("the arguments differ".into(), None)
            }
        }
        (Item::PolicyDecision { .. }, Item::PolicyDecision { .. }) => {
            ("the outcome differs".into(), None)
        }
        _ => (
            format!("a has a {}, b has a {}", a.what().name(), b.what().name()),
            None,
        ),
    }
}

/// Compare two traces. `a` and `b` are the events of each, in order.
pub fn diff(a: &[Event], b: &[Event]) -> TraceDiff {
    let (a, b) = (compared(a), compared(b));
    let mut divergences = Vec::new();
    let mut steps = Vec::new();

    let numbers: std::collections::BTreeSet<u64> =
        a.steps.keys().chain(b.steps.keys()).copied().collect();
    for step in numbers {
        let empty = Vec::new();
        let in_a = a.steps.get(&step);
        let in_b = b.steps.get(&step);
        let (left, right) = (in_a.unwrap_or(&empty), in_b.unwrap_or(&empty));
        let before = divergences.len();
        for index in 0..left.len().max(right.len()) {
            let divergence = match (left.get(index), right.get(index)) {
                (Some(x), Some(y)) if x.item.same_as(&y.item) => continue,
                (Some(x), Some(y)) => {
                    let (detail, messages) = explain(&x.item, &y.item);
                    Divergence {
                        step,
                        // Name it by what a did; the detail says if b did
                        // something of another kind.
                        what: x.item.what(),
                        a: Some(side(x)),
                        b: Some(side(y)),
                        detail,
                        messages,
                    }
                }
                (Some(x), None) => Divergence {
                    step,
                    what: x.item.what(),
                    a: Some(side(x)),
                    b: None,
                    detail: "only a has this".into(),
                    messages: None,
                },
                (None, Some(y)) => Divergence {
                    step,
                    what: y.item.what(),
                    a: None,
                    b: Some(side(y)),
                    detail: "only b has this".into(),
                    messages: None,
                },
                (None, None) => continue,
            };
            divergences.push(divergence);
        }
        steps.push(StepSummary {
            step,
            a: in_a.map(|entries| counts(entries)),
            b: in_b.map(|entries| counts(entries)),
            divergences: divergences.len() - before,
        });
    }

    // How each run ended, after everything it did.
    let last_step = steps.last().map_or(0, |s| s.step);
    let ending = |c: &Compared| {
        let state = c.last_state.as_ref().map(|(seq, state, reason)| Side {
            seq: *seq,
            summary: format!("{state:?}: {reason}").to_lowercase(),
        });
        let outcome = c.outcome.as_ref().map(|(seq, outcome)| Side {
            seq: *seq,
            summary: format!("outcome {outcome}"),
        });
        (state, outcome)
    };
    let ((state_a, outcome_a), (state_b, outcome_b)) = (ending(&a), ending(&b));
    let summary = |side: &Option<Side>| side.as_ref().map(|s| s.summary.clone());
    if summary(&state_a) != summary(&state_b) {
        divergences.push(Divergence {
            step: last_step,
            what: What::End,
            a: state_a,
            b: state_b,
            detail: "the runs ended in different states".into(),
            messages: None,
        });
    } else if summary(&outcome_a) != summary(&outcome_b) {
        divergences.push(Divergence {
            step: last_step,
            what: What::End,
            a: outcome_a,
            b: outcome_b,
            detail: "the runs finished with different outcomes".into(),
            messages: None,
        });
    }

    TraceDiff {
        steps,
        divergences,
        policy_differs: a.policy != b.policy,
    }
}
