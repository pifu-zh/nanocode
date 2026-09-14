//! Error classification & retry logic — Rust port of `src/core/errors.ts`.
//!
//! Retry semantics are user-visible behavior (BEHAVIOR.md §10):
//! 5 retries, 1s initial delay, ×2 backoff, 60s cap, ±20% jitter,
//! Retry-After honored on 429, three consecutive 529s give up,
//! auth / prompt-too-long / abort never retried.

use rand::Rng;
use std::future::Future;
use std::time::Duration;
use thiserror::Error;
use tokio_util::sync::CancellationToken;

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

#[derive(Debug, Error, Clone)]
pub enum NanocodeError {
    #[error("Prompt too long: {message}")]
    PromptTooLong {
        message: String,
        token_count: Option<u64>,
        max_tokens: Option<u64>,
    },

    #[error("Rate limited: {message}")]
    RateLimit { message: String, retry_after_ms: u64 },

    #[error("API overloaded: {message}")]
    Overloaded { message: String },

    #[error("Authentication failed ({status}): {message}")]
    Authentication { status: u16, message: String },

    #[error("Network error: {message}")]
    Network { message: String },

    #[error("Tool execution failed ({tool_name}): {message}")]
    ToolExecution { tool_name: String, message: String },

    #[error("Operation aborted")]
    Abort,

    #[error("{message}")]
    Other { message: String },
}

impl NanocodeError {
    /// Errors that must never be retried (errors.ts withRetry policy).
    pub fn is_non_retryable(&self) -> bool {
        matches!(
            self,
            NanocodeError::Authentication { .. }
                | NanocodeError::PromptTooLong { .. }
                | NanocodeError::Abort
        )
    }

    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            NanocodeError::RateLimit { .. }
                | NanocodeError::Overloaded { .. }
                | NanocodeError::Network { .. }
        )
    }
}

// ---------------------------------------------------------------------------
// Classification (errors.ts classifyError)
// ---------------------------------------------------------------------------

/// Network-flavored message fragments from the TS classifier.
const NETWORK_FRAGMENTS: &[&str] = &[
    "network",
    "econnrefused",
    "econnreset",
    "etimedout",
    "fetch failed",
    "socket hang up",
];

/// Classify an API/network failure. `status` — HTTP status when available;
/// `retry_after` — value of the `retry-after` header in seconds (as text);
/// `message` — error message text.
pub fn classify_error(status: Option<u16>, retry_after: Option<&str>, message: &str) -> NanocodeError {
    if let Some(status) = status {
        match status {
            401 | 403 => {
                return NanocodeError::Authentication {
                    status,
                    message: message.to_string(),
                }
            }
            429 => {
                let retry_after_ms = parse_retry_after(retry_after);
                return NanocodeError::RateLimit {
                    message: message.to_string(),
                    retry_after_ms,
                };
            }
            529 => {
                return NanocodeError::Overloaded { message: message.to_string() };
            }
            _ => {}
        }
    }

    let lower = message.to_lowercase();
    if lower.contains("prompt is too long") || lower.contains("prompt_too_long") {
        return NanocodeError::PromptTooLong { message: message.to_string(), token_count: None, max_tokens: None };
    }

    if NETWORK_FRAGMENTS.iter().any(|f| lower.contains(f)) {
        return NanocodeError::Network { message: message.to_string() };
    }

    NanocodeError::Other { message: message.to_string() }
}

/// Parse a `retry-after` header value (seconds). Missing/invalid → 5000ms,
/// below 1000ms clamps to 1000ms. (errors.ts parseRetryAfter)
pub fn parse_retry_after(value: Option<&str>) -> u64 {
    let Some(value) = value else { return 5000 };
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return 5000;
    }
    match trimmed.parse::<f64>() {
        Ok(seconds) if seconds.is_finite() && seconds >= 0.0 => {
            ((seconds * 1000.0) as u64).max(1000)
        }
        _ => 5000,
    }
}

// ---------------------------------------------------------------------------
// Retry logic (errors.ts withRetry)
// ---------------------------------------------------------------------------

type RetryCallback = Box<dyn Fn(&NanocodeError, u32, u64) + Send + Sync>;

#[derive(Default)]
pub struct RetryOptions {
    pub max_retries: u32,        // TS default 5
    pub initial_delay_ms: u64,   // TS default 1000
    pub max_delay_ms: u64,       // TS default 60_000
    pub backoff_factor: f64,     // TS default 2.0
    /// onRetry callback: (error, attempt(1-based), delay_ms)
    pub on_retry: Option<RetryCallback>,
}

impl RetryOptions {
    pub fn defaults() -> Self {
        RetryOptions {
            max_retries: 5,
            initial_delay_ms: 1000,
            max_delay_ms: 60_000,
            backoff_factor: 2.0,
            on_retry: None,
        }
    }
}

/// Jitter the delay by ±20% (errors.ts: `delay * (0.8 + Math.random() * 0.4)`).
fn jitter(delay_ms: u64) -> u64 {
    let factor = rand::thread_rng().gen_range(0.8..1.2f64);
    (delay_ms as f64 * factor) as u64
}

/// Retry `f` according to the classified error type. Cancellation surfaces as
/// [`NanocodeError::Abort`].
pub async fn with_retry<T, F, Fut>(
    mut f: F,
    opts: &RetryOptions,
    cancel: &CancellationToken,
) -> Result<T, NanocodeError>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, NanocodeError>>,
{
    let max_retries = if opts.max_retries == 0 { 5 } else { opts.max_retries };
    let initial = if opts.initial_delay_ms == 0 { 1000 } else { opts.initial_delay_ms };
    let cap = if opts.max_delay_ms == 0 { 60_000 } else { opts.max_delay_ms };
    let factor = if opts.backoff_factor <= 1.0 { 2.0 } else { opts.backoff_factor };

    let mut consecutive_529: u32 = 0;

    for attempt in 0..=max_retries {
        if cancel.is_cancelled() {
            return Err(NanocodeError::Abort);
        }

        match f().await {
            Ok(value) => return Ok(value),
            Err(err) => {
                // Non-retryable errors surface immediately.
                if err.is_non_retryable() {
                    return Err(err);
                }

                if attempt >= max_retries {
                    return Err(err);
                }

                let delay_ms = if let NanocodeError::RateLimit { retry_after_ms, .. } = &err {
                    if *retry_after_ms > 0 { *retry_after_ms } else { initial }
                } else if matches!(err, NanocodeError::Overloaded { .. }) {
                    consecutive_529 += 1;
                    if consecutive_529 >= 3 {
                        return Err(err);
                    }
                    (initial as f64 * factor.powi(attempt as i32)) as u64
                } else {
                    consecutive_529 = 0;
                    (initial as f64 * factor.powi(attempt as i32)) as u64
                };

                let delay_ms = jitter(delay_ms.min(cap));
                if let Some(on_retry) = opts.on_retry.as_deref() {
                    on_retry(&err, attempt + 1, delay_ms);
                }

                // Sleep cancellably.
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_millis(delay_ms)) => {}
                    _ = cancel.cancelled() => return Err(NanocodeError::Abort),
                }
            }
        }
    }

    unreachable!("loop returns on every branch")
}

// ---------------------------------------------------------------------------
// Tests — ported from test/core/errors.test.ts
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_401_is_auth() {
        let e = classify_error(Some(401), None, "invalid api key");
        assert!(matches!(e, NanocodeError::Authentication { status: 401, .. }));
        assert!(e.is_non_retryable());
    }

    #[test]
    fn classify_403_is_auth() {
        let e = classify_error(Some(403), None, "forbidden");
        assert!(matches!(e, NanocodeError::Authentication { status: 403, .. }));
    }

    #[test]
    fn classify_429_uses_retry_after() {
        let e = classify_error(Some(429), Some("2"), "rate limited");
        match &e {
            NanocodeError::RateLimit { retry_after_ms, .. } => assert_eq!(*retry_after_ms, 2000),
            other => panic!("expected rate limit, got {other:?}"),
        }
        assert!(e.is_retryable());
    }

    #[test]
    fn classify_429_defaults_to_5000() {
        let e = classify_error(Some(429), None, "slow down");
        match &e {
            NanocodeError::RateLimit { retry_after_ms, .. } => assert_eq!(*retry_after_ms, 5000),
            other => panic!("expected rate limit, got {other:?}"),
        }
    }

    #[test]
    fn retry_after_clamps_to_1000() {
        assert_eq!(parse_retry_after(Some("0.5")), 1000);
        assert_eq!(parse_retry_after(Some("abc")), 5000);
        assert_eq!(parse_retry_after(None), 5000);
        assert_eq!(parse_retry_after(Some("")), 5000);
        assert_eq!(parse_retry_after(Some("10")), 10_000);
    }

    #[test]
    fn classify_529_is_overloaded() {
        let e = classify_error(Some(529), None, "overloaded");
        assert!(matches!(e, NanocodeError::Overloaded { .. }));
        assert!(e.is_retryable());
    }

    #[test]
    fn classify_prompt_to_long_by_message() {
        for msg in ["prompt is too long: 300000 tokens", "Request failed: prompt_too_long"] {
            let e = classify_error(None, None, msg);
            assert!(matches!(e, NanocodeError::PromptTooLong { .. }), "msg={msg}");
            assert!(e.is_non_retryable());
        }
    }

    #[test]
    fn classify_network_fragments() {
        for msg in [
            "network error occurred",
            "connect ECONNREFUSED 127.0.0.1:1",
            "socket hang up",
            "fetch failed",
            "request ETIMEDOUT",
        ] {
            let e = classify_error(None, None, msg);
            assert!(matches!(e, NanocodeError::Network { .. }), "msg={msg}");
        }
    }

    #[test]
    fn classify_other() {
        let e = classify_error(None, None, "something odd");
        assert!(matches!(e, NanocodeError::Other { .. }));
        assert!(!e.is_retryable() && !e.is_non_retryable());
    }

    #[tokio::test]
    async fn with_retry_succeeds_after_failures() {
        let cancel = CancellationToken::new();
        let attempts = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
        let a2 = attempts.clone();
        let result: Result<u32, NanocodeError> = with_retry(
            move || {
                let n = a2.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                async move {
                    if n < 2 {
                        Err(NanocodeError::Network { message: "boom".into() })
                    } else {
                        Ok(n)
                    }
                }
            },
            &{
                let mut o = RetryOptions::defaults();
                o.initial_delay_ms = 1; // fast test
                o
            },
            &cancel,
        )
        .await;
        assert_eq!(result.unwrap(), 2);
        assert_eq!(attempts.load(std::sync::atomic::Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn with_retry_gives_up_after_max() {
        let cancel = CancellationToken::new();
        let opts = RetryOptions {
            max_retries: 2,
            initial_delay_ms: 1,
            ..RetryOptions::defaults()
        };
        let result: Result<(), NanocodeError> = with_retry(
            || async { Err(NanocodeError::Network { message: "down".into() }) },
            &opts,
            &cancel,
        )
        .await;
        assert!(matches!(result.unwrap_err(), NanocodeError::Network { .. }));
    }

    #[tokio::test]
    async fn with_retry_never_retries_auth() {
        let cancel = CancellationToken::new();
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
        let c2 = calls.clone();
        let opts = RetryOptions::defaults();
        let result: Result<(), NanocodeError> = with_retry(
            move || {
                let c = c2.clone();
                async move {
                    c.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    Err::<(), _>(classify_error(Some(401), None, "bad key"))
                }
            },
            &opts,
            &cancel,
        )
        .await;
        assert!(matches!(result.unwrap_err(), NanocodeError::Authentication { .. }));
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn with_retry_never_retries_ptl() {
        let cancel = CancellationToken::new();
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
        let c2 = calls.clone();
        let opts = RetryOptions::defaults();
        let result: Result<(), NanocodeError> = with_retry(
            move || {
                let c = c2.clone();
                async move {
                    c.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    Err::<(), _>(classify_error(None, None, "prompt is too long"))
                }
            },
            &opts,
            &cancel,
        )
        .await;
        assert!(matches!(result.unwrap_err(), NanocodeError::PromptTooLong { .. }));
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn with_retry_overloaded_circuit_breaker_after_3() {
        let cancel = CancellationToken::new();
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
        let c2 = calls.clone();
        let opts = RetryOptions {
            max_retries: 10,
            initial_delay_ms: 1,
            ..RetryOptions::defaults()
        };
        let result: Result<(), NanocodeError> = with_retry(
            move || {
                let c = c2.clone();
                async move {
                    c.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    Err::<(), _>(NanocodeError::Overloaded { message: "529".into() })
                }
            },
            &opts,
            &cancel,
        )
        .await;
        assert!(matches!(result.unwrap_err(), NanocodeError::Overloaded { .. }));
        // attempt 1 + retry 2 → consecutive==3 → give up on the 3rd call
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn with_retry_cancelled_returns_abort() {
        let cancel = CancellationToken::new();
        cancel.cancel();
        let opts = RetryOptions::defaults();
        let result: Result<(), NanocodeError> =
            with_retry(|| async { Ok(()) }, &opts, &cancel).await;
        assert!(matches!(result.unwrap_err(), NanocodeError::Abort));
    }
}
