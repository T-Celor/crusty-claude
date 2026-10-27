//! Retry loop for provider calls. Only failures before the response stream starts are
//! retried, so callers wrap the request and not the body reading.

use std::future::Future;
use std::time::Duration;

use crusty_core::{CancellationToken, ProviderError};

const MAX_BACKOFF: Duration = Duration::from_secs(30);
const DEFAULT_BASE: Duration = Duration::from_secs(1);

/// Runs `f` up to `max_retries + 1` times with the default 1 s base backoff.
pub async fn with_retries<T, F, Fut>(max_retries: u32, cancel: &CancellationToken, f: F) -> Result<T, ProviderError>
where
    F: FnMut(u32) -> Fut,
    Fut: Future<Output = Result<T, ProviderError>>,
{
    with_retries_base(DEFAULT_BASE, max_retries, cancel, f).await
}

/// Like [`with_retries`] with an explicit base delay (tests use a few milliseconds).
pub async fn with_retries_base<T, F, Fut>(
    base: Duration,
    max_retries: u32,
    cancel: &CancellationToken,
    mut f: F,
) -> Result<T, ProviderError>
where
    F: FnMut(u32) -> Fut,
    Fut: Future<Output = Result<T, ProviderError>>,
{
    let mut attempt: u32 = 0;
    loop {
        let result = tokio::select! {
            _ = cancel.cancelled() => return Err(ProviderError::Cancelled),
            r = f(attempt) => r,
        };
        match result {
            Ok(value) => return Ok(value),
            Err(err) if err.is_retryable() && attempt < max_retries => {
                let delay = backoff(base, attempt);
                tracing::warn!(attempt, ?delay, error = %err, "provider call failed, retrying");
                tokio::select! {
                    _ = cancel.cancelled() => return Err(ProviderError::Cancelled),
                    _ = tokio::time::sleep(delay) => {}
                }
                attempt += 1;
            }
            Err(err) => return Err(err),
        }
    }
}

/// Exponential delay capped at 30 s, plus a deterministic 0-499 ms jitter.
fn backoff(base: Duration, attempt: u32) -> Duration {
    let exp = base.saturating_mul(2u32.saturating_pow(attempt.min(20)));
    let jitter = Duration::from_millis(u64::from(attempt) * 137 % 500);
    exp.min(MAX_BACKOFF) + jitter
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::*;

    fn fast() -> Duration {
        Duration::from_millis(1)
    }

    #[tokio::test]
    async fn retries_retryable_then_succeeds() {
        let calls = AtomicU32::new(0);
        let cancel = CancellationToken::new();
        let out = with_retries_base(fast(), 3, &cancel, |_| {
            let n = calls.fetch_add(1, Ordering::SeqCst);
            async move { if n < 2 { Err(ProviderError::Http { status: 529, message: "busy".into() }) } else { Ok(n) } }
        })
        .await;
        assert_eq!(out, Ok(2));
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn does_not_retry_permanent_errors() {
        let calls = AtomicU32::new(0);
        let cancel = CancellationToken::new();
        let out: Result<(), _> = with_retries_base(fast(), 3, &cancel, |_| {
            calls.fetch_add(1, Ordering::SeqCst);
            async { Err(ProviderError::Http { status: 400, message: "bad".into() }) }
        })
        .await;
        assert!(matches!(out, Err(ProviderError::Http { status: 400, .. })));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn gives_up_after_max_retries() {
        let calls = AtomicU32::new(0);
        let cancel = CancellationToken::new();
        let out: Result<(), _> = with_retries_base(fast(), 2, &cancel, |_| {
            calls.fetch_add(1, Ordering::SeqCst);
            async { Err(ProviderError::Timeout) }
        })
        .await;
        assert_eq!(out, Err(ProviderError::Timeout));
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn cancellation_interrupts_backoff() {
        let cancel = CancellationToken::new();
        let trigger = cancel.clone();
        let out: Result<(), _> = with_retries_base(Duration::from_secs(60), 5, &cancel, |_| {
            trigger.cancel();
            async { Err(ProviderError::Network("down".into())) }
        })
        .await;
        assert_eq!(out, Err(ProviderError::Cancelled));
    }

    #[test]
    fn backoff_grows_and_caps() {
        assert_eq!(backoff(Duration::from_secs(1), 0), Duration::from_secs(1));
        assert_eq!(backoff(Duration::from_secs(1), 1), Duration::from_millis(2000 + 137));
        assert_eq!(backoff(Duration::from_secs(1), 10), Duration::from_millis(30_000 + 370));
    }
}
