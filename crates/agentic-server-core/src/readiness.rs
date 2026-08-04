use std::time::Duration;

use tracing::info;

use crate::config::Config;
use crate::error::Error;

fn checked_duration_seconds(name: &str, value: f64) -> Result<Duration, Error> {
    if !value.is_finite() || value <= 0.0 {
        return Err(Error::Config(format!(
            "{name} must be a finite number > 0 (got {value})"
        )));
    }
    Duration::try_from_secs_f64(value)
        .map_err(|_| Error::Config(format!("{name} must be representable as a Duration (got {value})")))
}

fn timeout_error(url: &str, timeout_s: f64) -> Error {
    Error::LlmTimeout {
        url: url.to_owned(),
        timeout_s,
    }
}

/// Result of a single bounded inference-service health probe.
#[derive(Debug)]
pub enum LlmReadiness {
    Ready,
    Rejected(reqwest::StatusCode),
    Unreachable(reqwest::Error),
    TimedOut,
}

/// Probe the inference service's `/health` endpoint once.
///
/// # Errors
///
/// Returns an error when the configured bearer credential cannot be represented
/// as an HTTP header.
pub async fn probe_llm_readiness(
    client: &reqwest::Client,
    llm_api_base: &str,
    openai_api_key: Option<&str>,
    timeout: Duration,
) -> Result<LlmReadiness, Error> {
    let base = llm_api_base.trim_end_matches('/');
    let url = format!("{base}/health");
    let mut request = client.get(url);
    if let Some(key) = openai_api_key.map(str::trim).filter(|key| !key.is_empty()) {
        let value = reqwest::header::HeaderValue::from_str(&format!("Bearer {key}"))?;
        request = request.header(reqwest::header::AUTHORIZATION, value);
    }

    Ok(match tokio::time::timeout(timeout, request.send()).await {
        Ok(Ok(response)) if response.status().is_success() => LlmReadiness::Ready,
        Ok(Ok(response)) => LlmReadiness::Rejected(response.status()),
        Ok(Err(error)) => LlmReadiness::Unreachable(error),
        Err(_) => LlmReadiness::TimedOut,
    })
}

/// Poll LLM `/health` until it responds successfully or the timeout is reached.
///
/// # Errors
///
/// Returns an error if the LLM does not become ready within the configured timeout.
pub async fn wait_llm_ready(config: &Config) -> Result<(), Error> {
    let base = config.llm_api_base.trim_end_matches('/');
    let url = format!("{base}/health");

    let client = reqwest::Client::builder().build().map_err(Error::HttpClient)?;

    let timeout = checked_duration_seconds("llm_ready_timeout_s", config.llm_ready_timeout_s)?;
    let interval = checked_duration_seconds("llm_ready_interval_s", config.llm_ready_interval_s)?;
    let start = tokio::time::Instant::now();
    let mut last_notice = Duration::ZERO;

    loop {
        let remaining = timeout
            .checked_sub(start.elapsed())
            .ok_or_else(|| timeout_error(&url, config.llm_ready_timeout_s))?;
        if remaining.is_zero() {
            return Err(timeout_error(&url, config.llm_ready_timeout_s));
        }

        if matches!(
            probe_llm_readiness(
                &client,
                &config.llm_api_base,
                config.openai_api_key.as_deref(),
                Duration::from_secs(2).min(remaining),
            )
            .await?,
            LlmReadiness::Ready
        ) {
            return Ok(());
        }

        let elapsed = start.elapsed();
        if elapsed.saturating_sub(last_notice) >= interval {
            last_notice = elapsed;
            info!("waiting for LLM ({}s elapsed): {url}", elapsed.as_secs());
        }

        let remaining = timeout
            .checked_sub(start.elapsed())
            .ok_or_else(|| timeout_error(&url, config.llm_ready_timeout_s))?;
        if remaining.is_zero() {
            return Err(timeout_error(&url, config.llm_ready_timeout_s));
        }

        tokio::time::sleep(interval.min(remaining)).await;
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{checked_duration_seconds, probe_llm_readiness, timeout_error};

    #[test]
    fn checked_duration_rejects_non_positive() {
        assert!(checked_duration_seconds("v", 0.0).is_err());
        assert!(checked_duration_seconds("v", -1.0).is_err());
    }

    #[test]
    fn checked_duration_rejects_nan() {
        assert!(checked_duration_seconds("v", f64::NAN).is_err());
    }

    #[test]
    fn checked_duration_rejects_infinite() {
        assert!(checked_duration_seconds("v", f64::INFINITY).is_err());
    }

    #[test]
    fn checked_duration_rejects_too_large_finite() {
        assert!(checked_duration_seconds("v", 1e50).is_err());
    }

    #[test]
    fn checked_duration_accepts_positive_finite() {
        let duration = checked_duration_seconds("v", 0.25).unwrap();
        assert_eq!(duration.as_millis(), 250);
    }

    #[test]
    fn timeout_error_preserves_inputs() {
        let err = timeout_error("http://127.0.0.1:8000/health", 0.5);
        match err {
            crate::error::Error::LlmTimeout { url, timeout_s } => {
                assert_eq!(url, "http://127.0.0.1:8000/health");
                assert!((timeout_s - 0.5).abs() < f64::EPSILON);
            }
            other => panic!("expected timeout error, got {other:?}"),
        }
    }

    #[test]
    fn interval_sleep_is_capped_by_remaining_timeout() {
        let interval = Duration::from_secs(2);
        let remaining = Duration::from_millis(100);
        assert_eq!(interval.min(remaining), Duration::from_millis(100));
    }

    #[tokio::test]
    async fn probe_rejects_invalid_bearer_header_before_network_io() {
        let error = probe_llm_readiness(
            &reqwest::Client::new(),
            "http://127.0.0.1:1",
            Some("invalid\nkey"),
            Duration::from_secs(1),
        )
        .await
        .unwrap_err();

        assert!(matches!(error, crate::error::Error::InvalidHeader(_)));
    }
}
