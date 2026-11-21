//! Scripted responses for tests. No API key.

use std::collections::VecDeque;
use std::sync::{Mutex, MutexGuard, PoisonError};

use async_trait::async_trait;
use tether_types::{Price, Usage};

use crate::{Completion, CompletionRequest, ModelAdapter, ModelError};

/// Answers each request with the next completion in its script and remembers
/// every request it was sent. A request past the end of the script is a
/// provider error.
#[derive(Debug, Default)]
pub struct FakeAdapter {
    script: Mutex<VecDeque<Completion>>,
    requests: Mutex<Vec<CompletionRequest>>,
    usd_per_call: f64,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl FakeAdapter {
    pub fn new(script: impl IntoIterator<Item = Completion>) -> Self {
        Self {
            script: Mutex::new(script.into_iter().collect()),
            requests: Mutex::default(),
            usd_per_call: 0.0,
        }
    }

    /// Charge a flat price for every completion, to exercise dollar budgets.
    pub fn priced(mut self, usd_per_call: f64) -> Self {
        self.usd_per_call = usd_per_call;
        self
    }

    /// Every request received so far, oldest first.
    pub fn requests(&self) -> Vec<CompletionRequest> {
        lock(&self.requests).clone()
    }

    /// Scripted completions not yet used.
    pub fn remaining(&self) -> usize {
        lock(&self.script).len()
    }
}

#[async_trait]
impl ModelAdapter for FakeAdapter {
    fn id(&self) -> &str {
        "fake"
    }

    async fn complete(&self, req: CompletionRequest) -> Result<Completion, ModelError> {
        lock(&self.requests).push(req);
        lock(&self.script)
            .pop_front()
            .ok_or_else(|| ModelError::Provider("fake adapter: script exhausted".into()))
    }

    /// Unpriced unless [`FakeAdapter::priced`] set a flat price per call.
    fn price(&self, _model: &str, _usage: &Usage) -> Option<Price> {
        (self.usd_per_call > 0.0).then(|| Price {
            usd: self.usd_per_call,
            table: "fake".into(),
        })
    }
}
