//! What the HTTP adapters share: the client, retries, and reading a stream
//! of server-sent events. Nothing here knows a provider's message shape.

use std::future::Future;
use std::time::Duration;

use crate::{Completion, ModelError};

pub(crate) fn provider(message: impl Into<String>) -> ModelError {
    ModelError::Provider(message.into())
}

/// An environment variable, with an empty value counted as unset.
pub(crate) fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty())
}

pub(crate) fn client() -> reqwest::Client {
    // Never follow a redirect: the request carries an API key in a header,
    // and a redirect could take it, and the prompt, to another host.
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .user_agent(concat!("tether/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(30))
        // Between chunks, not for the whole response: a long answer
        // streams for minutes.
        .read_timeout(Duration::from_secs(600))
        .build()
        .unwrap_or_default()
}

/// One HTTP attempt that did not produce a completion.
pub(crate) struct Failed {
    pub error: ModelError,
    /// Worth trying again.
    pub retry: bool,
    /// How long the provider asked the client to wait first.
    pub after: Option<Duration>,
}

impl Failed {
    pub fn fatal(error: ModelError) -> Self {
        Self {
            error,
            retry: false,
            after: None,
        }
    }

    pub fn retryable(error: ModelError) -> Self {
        Self {
            error,
            retry: true,
            after: None,
        }
    }
}

/// Run `attempt` until it succeeds, fails for good, or has been retried
/// `max_retries` times. The wait starts at `first_delay` and doubles, unless
/// the failure says how long to wait.
pub(crate) async fn with_retries<F, Fut>(
    max_retries: u32,
    first_delay: Duration,
    mut attempt: F,
) -> Result<Completion, ModelError>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<Completion, Failed>>,
{
    let mut delay = first_delay;
    let mut retries = 0;
    loop {
        match attempt().await {
            Ok(completion) => return Ok(completion),
            Err(failed) if failed.retry && retries < max_retries => {
                tokio::time::sleep(failed.after.unwrap_or(delay)).await;
                delay *= 2;
                retries += 1;
            }
            Err(failed) => return Err(failed.error),
        }
    }
}

/// Send `request` and return the response if its status is a success, with
/// the request id from the `id_header` header. An error status becomes the
/// error `api_error` makes of it, retryable for a timeout, a conflict, a rate
/// limit or a server error unless the provider's `x-should-retry` header says
/// otherwise.
pub(crate) async fn send(
    request: reqwest::RequestBuilder,
    id_header: &str,
    api_error: fn(u16, &str, Option<String>) -> ModelError,
) -> Result<(reqwest::Response, Option<String>), Failed> {
    let response = request
        .send()
        .await
        .map_err(|e| Failed::retryable(provider(format!("request failed: {e}"))))?;

    let header = |name: &str| {
        response
            .headers()
            .get(name)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned)
    };
    let request_id = header(id_header);
    let status = response.status();
    if status.is_success() {
        return Ok((response, request_id));
    }
    let should_retry = header("x-should-retry");
    let after = header("retry-after")
        .and_then(|secs| secs.parse::<f64>().ok())
        .filter(|secs| secs.is_finite() && *secs >= 0.0)
        .map(|secs| Duration::from_secs_f64(secs.min(60.0)));
    let text = response.text().await.unwrap_or_default();
    let retry = match should_retry.as_deref() {
        Some("true") => true,
        Some("false") => false,
        _ => matches!(status.as_u16(), 408 | 409 | 429) || status.is_server_error(),
    };
    Err(Failed {
        error: api_error(status.as_u16(), &text, request_id),
        retry,
        after,
    })
}

/// Splits a server-sent event stream into the `data` of each event.
#[derive(Default)]
pub(crate) struct Sse {
    /// Bytes of a line not yet ended.
    line: Vec<u8>,
    /// `data:` lines of the event being read.
    data: String,
}

impl Sse {
    /// Take the next bytes of the stream, which may split anywhere, and
    /// return the data of each event they complete.
    pub fn feed(&mut self, chunk: &[u8]) -> Vec<String> {
        let mut events = Vec::new();
        for byte in chunk {
            if *byte != b'\n' {
                self.line.push(*byte);
                continue;
            }
            let line = std::mem::take(&mut self.line);
            let line = String::from_utf8_lossy(&line);
            let line = line.strip_suffix('\r').unwrap_or(&line);
            if line.is_empty() {
                // A blank line ends the event.
                let data = std::mem::take(&mut self.data);
                if !data.is_empty() {
                    events.push(data);
                }
            } else if let Some(data) = line.strip_prefix("data:") {
                if !self.data.is_empty() {
                    self.data.push('\n');
                }
                self.data.push_str(data.strip_prefix(' ').unwrap_or(data));
            }
            // `event:` repeats the type inside the data; comments start with `:`.
        }
        events
    }
}
