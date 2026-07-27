use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

/// Longest single sleep we will accept from a `Retry-After` header or a
/// rate-limit reset time, so a bad clock or a hostile header cannot park the
/// process for hours.
const MAX_SLEEP: Duration = Duration::from_secs(15 * 60);

/// Point budget kept in reserve. GraphQL calls cost more than one point, so
/// stopping at zero would mean discovering exhaustion via a failed request.
const BUDGET_FLOOR: u32 = 2;

/// A thin GraphQL-over-HTTP client for github.com or a GitHub Enterprise Server
/// instance. It tracks the primary rate-limit budget from response headers and
/// waits for the reset before spending the last of it, and backs off on
/// secondary rate limits and transient gateway failures.
pub struct GithubClient {
    http: reqwest::Client,
    endpoint: String,
    max_retries: u32,
    state: Mutex<RateState>,
}

#[derive(Debug, Default, Clone, Copy)]
pub struct RateState {
    pub limit: Option<u32>,
    pub remaining: Option<u32>,
    /// Unix seconds at which the budget refills.
    pub reset_at: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct GraphQlResponse<T> {
    data: Option<T>,
    #[serde(default)]
    errors: Vec<GraphQlError>,
}

#[derive(Debug, Deserialize)]
struct GraphQlError {
    message: String,
    #[serde(rename = "type")]
    kind: Option<String>,
    #[serde(default)]
    path: Vec<Value>,
}

impl GraphQlError {
    fn display(&self) -> String {
        let path = if self.path.is_empty() {
            String::new()
        } else {
            let parts: Vec<String> = self
                .path
                .iter()
                .map(|p| match p {
                    Value::String(s) => s.clone(),
                    other => other.to_string(),
                })
                .collect();
            format!(" (at {})", parts.join("."))
        };
        match &self.kind {
            Some(kind) => format!("[{kind}] {}{path}", self.message),
            None => format!("{}{path}", self.message),
        }
    }
}

impl GithubClient {
    /// `api_url` is the full GraphQL endpoint, e.g. `https://api.github.com/graphql`
    /// or `https://ghe.example.com/api/graphql`.
    pub fn new(api_url: &str, token: &str, max_retries: u32) -> Result<Self> {
        let mut headers = reqwest::header::HeaderMap::new();
        let mut auth = reqwest::header::HeaderValue::from_str(&format!("bearer {token}"))
            .context("token contains characters that are not valid in an HTTP header")?;
        auth.set_sensitive(true);
        headers.insert(reqwest::header::AUTHORIZATION, auth);

        let http = reqwest::Client::builder()
            .user_agent(concat!(
                env!("CARGO_PKG_NAME"),
                "/",
                env!("CARGO_PKG_VERSION")
            ))
            .default_headers(headers)
            .timeout(Duration::from_secs(60))
            .build()
            .context("failed to build HTTP client")?;

        Ok(Self {
            http,
            endpoint: api_url.to_string(),
            max_retries,
            state: Mutex::new(RateState::default()),
        })
    }

    pub fn rate_state(&self) -> RateState {
        *self.state.lock().expect("rate state mutex poisoned")
    }

    /// Run a query and deserialize the `data` object into `T`.
    ///
    /// GraphQL errors that still return usable data (a common shape when a
    /// single field is inaccessible) are reported on stderr rather than
    /// aborting the run, so one unreadable org does not lose the whole export.
    pub async fn query<T: DeserializeOwned>(&self, query: &str, variables: Value) -> Result<T> {
        let body = json!({ "query": query, "variables": variables });
        let mut attempt = 0u32;

        loop {
            attempt += 1;
            self.await_budget().await;

            let sent = self.http.post(&self.endpoint).json(&body).send().await;

            let response = match sent {
                Ok(response) => response,
                Err(err) if attempt <= self.max_retries && transport_is_retryable(&err) => {
                    self.backoff(attempt, &format!("request failed: {err}"))
                        .await;
                    continue;
                }
                Err(err) => return Err(err).context("request to GitHub failed"),
            };

            let status = response.status();
            self.record_headers(response.headers());
            let retry_after = header_u64(response.headers(), "retry-after");

            if status.as_u16() == 401 {
                bail!("GitHub rejected the token (401 Unauthorized); check GITHUB_TOKEN");
            }
            if status.as_u16() == 404 {
                bail!("GraphQL endpoint not found (404): {}", self.endpoint);
            }

            let text = response
                .text()
                .await
                .context("failed to read response body")?;

            // 403/429 covers both the primary budget and the undocumented
            // secondary limits, which are signalled only in the body text.
            if status.as_u16() == 403 || status.as_u16() == 429 {
                if attempt > self.max_retries {
                    bail!(
                        "rate limited by GitHub and out of retries (HTTP {status}): {}",
                        truncate(&text, 300)
                    );
                }
                let wait = retry_after
                    .map(Duration::from_secs)
                    .or_else(|| self.reset_delay())
                    .unwrap_or_else(|| backoff_delay(attempt))
                    .min(MAX_SLEEP);
                let kind = if text.contains("secondary rate limit") {
                    "secondary rate limit"
                } else {
                    "rate limited"
                };
                eprintln!(
                    "  … {kind} (HTTP {}); waiting {}s (attempt {attempt}/{})",
                    status.as_u16(),
                    wait.as_secs(),
                    self.max_retries
                );
                tokio::time::sleep(wait).await;
                continue;
            }

            if status.is_server_error() && attempt <= self.max_retries {
                self.backoff(attempt, &format!("HTTP {status}")).await;
                continue;
            }

            if !status.is_success() {
                bail!("HTTP {status} from GitHub: {}", truncate(&text, 400));
            }

            let parsed: GraphQlResponse<T> = serde_json::from_str(&text).with_context(|| {
                format!("failed to parse GraphQL response: {}", truncate(&text, 400))
            })?;

            // A 200 can still carry RATE_LIMITED — that is how the GraphQL API
            // reports an exhausted point budget.
            let rate_limited = parsed.errors.iter().any(|e| {
                e.kind.as_deref() == Some("RATE_LIMITED")
                    || e.message.to_ascii_lowercase().contains("rate limit")
            });
            if rate_limited && parsed.data.is_none() {
                if attempt > self.max_retries {
                    bail!("GraphQL point budget exhausted and out of retries");
                }
                let wait = self
                    .reset_delay()
                    .unwrap_or_else(|| backoff_delay(attempt))
                    .min(MAX_SLEEP);
                eprintln!(
                    "  … GraphQL point budget exhausted; waiting {}s for reset (attempt {attempt}/{})",
                    wait.as_secs(),
                    self.max_retries
                );
                tokio::time::sleep(wait).await;
                continue;
            }

            match parsed.data {
                Some(data) => {
                    for error in &parsed.errors {
                        eprintln!("  warning: {}", error.display());
                    }
                    return Ok(data);
                }
                None => {
                    let joined = parsed
                        .errors
                        .iter()
                        .map(GraphQlError::display)
                        .collect::<Vec<_>>()
                        .join("; ");
                    return Err(anyhow!(if joined.is_empty() {
                        format!(
                            "GraphQL response contained no data: {}",
                            truncate(&text, 400)
                        )
                    } else {
                        joined
                    }));
                }
            }
        }
    }

    /// Sleep until the budget refills if the last response left us at the floor.
    async fn await_budget(&self) {
        let (remaining, delay) = {
            let state = self.state.lock().expect("rate state mutex poisoned");
            (state.remaining, delay_until(state.reset_at))
        };
        let Some(remaining) = remaining else { return };
        if remaining > BUDGET_FLOOR {
            return;
        }
        let Some(delay) = delay else { return };
        let delay = delay.min(MAX_SLEEP);
        eprintln!(
            "  … rate-limit budget down to {remaining}; waiting {}s for reset",
            delay.as_secs()
        );
        tokio::time::sleep(delay).await;
        // Assume the window refilled; the next response corrects this.
        let mut state = self.state.lock().expect("rate state mutex poisoned");
        state.remaining = state.limit;
        state.reset_at = None;
    }

    fn record_headers(&self, headers: &reqwest::header::HeaderMap) {
        let limit = header_u64(headers, "x-ratelimit-limit").map(|v| v as u32);
        let remaining = header_u64(headers, "x-ratelimit-remaining").map(|v| v as u32);
        let reset = header_u64(headers, "x-ratelimit-reset");
        if limit.is_none() && remaining.is_none() && reset.is_none() {
            return;
        }
        let mut state = self.state.lock().expect("rate state mutex poisoned");
        if limit.is_some() {
            state.limit = limit;
        }
        if remaining.is_some() {
            state.remaining = remaining;
        }
        if reset.is_some() {
            state.reset_at = reset;
        }
    }

    fn reset_delay(&self) -> Option<Duration> {
        let state = self.state.lock().expect("rate state mutex poisoned");
        delay_until(state.reset_at)
    }

    async fn backoff(&self, attempt: u32, reason: &str) {
        let delay = backoff_delay(attempt);
        eprintln!(
            "  … {reason}; retrying in {}s (attempt {attempt}/{})",
            delay.as_secs(),
            self.max_retries
        );
        tokio::time::sleep(delay).await;
    }
}

/// Whether a transport-level failure is worth retrying. Timeouts and dropped
/// connections are; a hostname that does not resolve will not resolve on the
/// next attempt either, so failing fast beats a minute of backoff.
fn transport_is_retryable(err: &reqwest::Error) -> bool {
    let mut source: Option<&(dyn std::error::Error + 'static)> = Some(err);
    while let Some(current) = source {
        let message = current.to_string().to_ascii_lowercase();
        if message.contains("dns error") || message.contains("failed to lookup address") {
            return false;
        }
        source = current.source();
    }
    err.is_timeout() || err.is_connect() || err.is_request()
}

/// Exponential backoff, capped at a minute.
fn backoff_delay(attempt: u32) -> Duration {
    Duration::from_secs(2u64.saturating_pow(attempt.min(6)).min(60))
}

/// Seconds from now until `reset_at`, plus a second of slack. `None` if the
/// reset is unknown or already past.
fn delay_until(reset_at: Option<u64>) -> Option<Duration> {
    let reset_at = reset_at?;
    let now = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs();
    (reset_at > now).then(|| Duration::from_secs(reset_at - now + 1))
}

fn header_u64(headers: &reqwest::header::HeaderMap, name: &str) -> Option<u64> {
    headers.get(name)?.to_str().ok()?.trim().parse().ok()
}

fn truncate(s: &str, max: usize) -> String {
    let s = s.trim();
    if s.chars().count() <= max {
        return s.to_string();
    }
    let head: String = s.chars().take(max).collect();
    format!("{head}…")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_grows_and_caps() {
        assert_eq!(backoff_delay(1), Duration::from_secs(2));
        assert_eq!(backoff_delay(3), Duration::from_secs(8));
        assert_eq!(backoff_delay(30), Duration::from_secs(60));
    }

    #[test]
    fn reset_in_the_past_does_not_sleep() {
        assert!(delay_until(Some(1)).is_none());
        assert!(delay_until(None).is_none());
    }

    #[test]
    fn reset_in_the_future_sleeps() {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let delay = delay_until(Some(now + 30)).expect("should sleep");
        assert!(delay >= Duration::from_secs(30) && delay <= Duration::from_secs(32));
    }
}
