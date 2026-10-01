//! Gateway-executed `web_fetch` tool for the Anthropic Messages API (#408).
//!
//! A native `web_fetch_20250910` declaration is rewritten into an ordinary
//! function tool for the upstream model, and the resulting `web_fetch` call is
//! executed here instead of reaching a client that expects the server to have
//! run it. This module owns the model-facing policy: argument parsing, URL
//! admission ([`policy`]), domain filtering, text extraction ([`extract`]), the
//! content limit, the ceiling on fetches in flight, and the output shape.
//! Retrieval sits behind [`backend::WebFetchBackend`]; the built-in
//! [`http::HttpFetchBackend`] is the default, and replacing it is a change to
//! this module alone, not to the Messages loop.
//!
//! Documented failures (`url_not_accessible`, `url_not_allowed`, ...) are the
//! tool's answer to the model: they come back as `Ok` output in the
//! `web_fetch_tool_result_error` shape, which the Messages loop marks
//! `is_error`. `Err` is reserved for gateway faults.

pub(crate) mod backend;
mod extract;
mod http;
pub(crate) mod policy;

use std::borrow::Cow;
use std::collections::HashMap;
use std::future::Future;
use std::num::NonZeroUsize;
use std::pin::Pin;
use std::sync::Arc;

use chrono::{SecondsFormat, Utc};
use serde::Serialize;
use serde_json::Value;
use tokio::sync::Semaphore;

use self::backend::{FetchFailure, FetchedDocument, WebFetchBackend};
use self::extract::truncate_to_char_boundary;
use self::http::HttpFetchBackend;
use self::policy::UrlRejection;
use super::handler::{GatewayExecutor, MAX_GATEWAY_TOOL_OUTPUT_BYTES, ToolError, ToolHandler, ToolOutput};
use super::ownership::GatewayBinding;
use super::registry::{ToolEntry, ToolType};
use super::web_search::args::DomainFilter;
use crate::config::WebFetchConfig;
use crate::types::io::FunctionTool;
use crate::types::tools::WebFetchToolParam;

/// The registry key and model-visible name of the tool.
pub(crate) const WEB_FETCH_TOOL_NAME: &str = "web_fetch";
/// Bytes of UTF-8 per token assumed when applying `max_content_tokens`. The
/// limit is documented as approximate.
pub(crate) const BYTES_PER_CONTENT_TOKEN: usize = 4;
/// Ceiling on the text returned to the model, leaving room for the JSON
/// envelope under the gateway tool output cap.
const MAX_CONTENT_BYTES: usize = MAX_GATEWAY_TOOL_OUTPUT_BYTES - 16 * 1024;
const FAILURE_PREFIX: &str = r#"{"type":"web_fetch_tool_result_error""#;

pub(crate) type WebFetchExecutor =
    dyn GatewayExecutor<ToolParams = WebFetchToolParam, ExecutionParams = WebFetchToolParam>;

pub(crate) fn insert_web_fetch_entry(
    entries: &mut HashMap<String, ToolEntry>,
    params: &WebFetchToolParam,
    executor: Arc<WebFetchExecutor>,
) {
    entries.insert(
        WEB_FETCH_TOOL_NAME.to_owned(),
        ToolEntry::gateway(
            ToolType::WebFetch,
            None,
            Some(GatewayBinding::new(executor, params.clone())),
        ),
    );
}

/// The function tool the upstream model sees in place of the native declaration.
#[must_use]
pub(crate) fn web_fetch_function_tool() -> FunctionTool {
    FunctionTool {
        type_: "function".to_owned(),
        name: WEB_FETCH_TOOL_NAME.to_owned(),
        description: Some(
            "Fetch the full text of one web page. Only a URL that already appears in the conversation can be \
             fetched: one the user wrote, one returned by a tool, or one from an earlier search or fetch result."
                .to_owned(),
        ),
        parameters: Some(serde_json::json!({
            "type": "object",
            "properties": {
                "url": {
                    "type": "string",
                    "description": "The absolute http or https URL of the page to fetch."
                }
            },
            "required": ["url"]
        })),
        strict: Some(false),
    }
}

/// The documented `web_fetch_tool_result_error` codes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum WebFetchErrorCode {
    InvalidToolInput,
    UrlTooLong,
    UrlNotAllowed,
    UrlNotInPriorContext,
    UrlNotAccessible,
    TooManyRequests,
    UnsupportedContentType,
    MaxUsesExceeded,
    Unavailable,
}

#[derive(Serialize)]
struct WebFetchErrorOutput<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    error_code: WebFetchErrorCode,
    message: &'a str,
}

/// The model-facing output for a fetch that produced no document: the
/// documented error shape plus a `message` the model can act on.
#[must_use]
pub(crate) fn failure_output(error_code: WebFetchErrorCode, message: &str) -> String {
    serde_json::to_string(&WebFetchErrorOutput {
        kind: "web_fetch_tool_result_error",
        error_code,
        message,
    })
    .unwrap_or_else(|_| {
        r#"{"type":"web_fetch_tool_result_error","error_code":"unavailable","message":"internal error"}"#.to_owned()
    })
}

/// Whether a handler output is a failure result, so the loop can mark the
/// fed-back `tool_result` as an error.
#[must_use]
pub(crate) fn is_failure_output(output: &str) -> bool {
    output.starts_with(FAILURE_PREFIX)
}

/// Validated arguments of one `web_fetch` call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WebFetchArguments {
    pub(crate) url: String,
}

impl WebFetchArguments {
    /// Parse the model's arguments: a JSON object with a non-empty string `url`.
    pub(crate) fn from_json(arguments: &str) -> Result<Self, String> {
        let value: Value =
            serde_json::from_str(arguments).map_err(|error| format!("arguments must be valid JSON: {error}"))?;
        let url = value
            .get("url")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|url| !url.is_empty())
            .ok_or_else(|| "arguments must contain a non-empty string url".to_owned())?;
        Ok(Self { url: url.to_owned() })
    }
}

/// How many fetches a call asks for: the unit a `max_uses` budget counts.
/// Zero when the arguments do not parse, because the handler then fetches
/// nothing.
#[must_use]
pub(crate) fn requested_fetches(arguments: &str) -> usize {
    usize::from(WebFetchArguments::from_json(arguments).is_ok())
}

/// The URL a call asks for, when its arguments parse.
#[must_use]
pub(crate) fn requested_url(arguments: &str) -> Option<String> {
    WebFetchArguments::from_json(arguments).ok().map(|args| args.url)
}

/// Model-facing `web_fetch` output; field order is the wire contract.
#[derive(Serialize)]
struct WebFetchToolOutput<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    url: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    title: Option<&'a str>,
    content_type: &'a str,
    retrieved_at: &'a str,
    truncated: bool,
    content: &'a str,
}

/// Executes `web_fetch` calls against one backend.
#[derive(Debug, Clone)]
pub struct WebFetchHandler {
    backend: Arc<dyn WebFetchBackend>,
    /// Fetches in flight at once across the gateway. The Messages loop starts
    /// a round's admitted calls together, so this is what bounds the open
    /// connections one turn can hold.
    permits: Arc<Semaphore>,
}

impl WebFetchHandler {
    /// Builds the handler on the built-in HTTP backend, with at most
    /// `max_concurrent_fetches` fetches in flight at once.
    #[must_use]
    pub fn from_config(config: &WebFetchConfig, max_concurrent_fetches: NonZeroUsize) -> Self {
        Self::with_backend(Arc::new(HttpFetchBackend::new(config.clone())), max_concurrent_fetches)
    }

    pub(crate) fn with_backend(backend: Arc<dyn WebFetchBackend>, max_concurrent_fetches: NonZeroUsize) -> Self {
        Self {
            backend,
            permits: Arc::new(Semaphore::new(max_concurrent_fetches.get())),
        }
    }

    async fn execute_fetch(
        &self,
        call_id: &str,
        arguments: &str,
        params: &WebFetchToolParam,
    ) -> Result<ToolOutput, ToolError> {
        let output = match self.fetch(arguments, params).await {
            Ok(document) => {
                // Extracting a body up to the download ceiling is CPU work;
                // it runs off the async workers.
                let params = params.clone();
                tokio::task::spawn_blocking(move || render(&document, &params))
                    .await
                    .map_err(|error| ToolError::Execution(format!("web_fetch extraction task failed: {error}")))??
            }
            Err((code, message)) => failure_output(code, &message),
        };
        Ok(ToolOutput {
            call_id: call_id.to_owned(),
            output,
        })
    }

    async fn fetch(
        &self,
        arguments: &str,
        params: &WebFetchToolParam,
    ) -> Result<FetchedDocument, (WebFetchErrorCode, String)> {
        let args =
            WebFetchArguments::from_json(arguments).map_err(|reason| (WebFetchErrorCode::InvalidToolInput, reason))?;
        let url = policy::validate_url(&args.url).map_err(|rejection| match rejection {
            UrlRejection::InvalidInput(reason) => (WebFetchErrorCode::InvalidToolInput, reason),
            UrlRejection::TooLong => (
                WebFetchErrorCode::UrlTooLong,
                format!("url exceeds {} characters", policy::MAX_URL_CHARS),
            ),
            UrlRejection::NotAllowed(reason) => (WebFetchErrorCode::UrlNotAllowed, reason),
        })?;
        let filters = params.filters.as_ref();
        let filter = DomainFilter::new(
            filters.and_then(|filters| filters.allowed_domains.as_deref()),
            filters.and_then(|filters| filters.blocked_domains.as_deref()),
        );
        if !filter.allows(url.as_str()) {
            return Err((
                WebFetchErrorCode::UrlNotAllowed,
                format!("{} is outside the allowed domains", url.host_str().unwrap_or_default()),
            ));
        }
        let _permit = self.permits.acquire().await.map_err(|error| {
            (
                WebFetchErrorCode::Unavailable,
                format!("fetch scheduler closed: {error}"),
            )
        })?;
        self.backend.fetch(&url, &filter).await.map_err(|failure| {
            let code = match &failure {
                FetchFailure::NotAllowed(_) => WebFetchErrorCode::UrlNotAllowed,
                FetchFailure::NotAccessible(_) => WebFetchErrorCode::UrlNotAccessible,
                FetchFailure::TooManyRequests => WebFetchErrorCode::TooManyRequests,
                FetchFailure::UnsupportedContentType(_) => WebFetchErrorCode::UnsupportedContentType,
                FetchFailure::Unavailable(_) => WebFetchErrorCode::Unavailable,
            };
            (code, failure.to_string())
        })
    }
}

/// Serialize a fetched document for the model: HTML becomes plain text, the
/// text is cut to the content limit, and the whole output stays under the
/// gateway tool output cap even after JSON escaping.
fn render(document: &FetchedDocument, params: &WebFetchToolParam) -> Result<String, ToolError> {
    let (title, text): (Option<String>, Cow<'_, str>) = if is_html(&document.media_type) {
        let extracted = extract::html_to_text(&document.body);
        (extracted.title, Cow::Owned(extracted.text))
    } else {
        (None, Cow::Borrowed(&document.body))
    };
    let retrieved_at = Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true);
    let mut limit = content_limit(params);
    loop {
        let content = truncate_to_char_boundary(&text, limit);
        let output = serde_json::to_string(&WebFetchToolOutput {
            kind: "web_fetch_result",
            url: document.url.as_str(),
            title: title.as_deref(),
            content_type: &document.media_type,
            retrieved_at: &retrieved_at,
            truncated: document.truncated || content.len() < text.len(),
            content,
        })
        .map_err(|error| ToolError::Execution(format!("failed to serialize web_fetch output: {error}")))?;
        if output.len() <= MAX_GATEWAY_TOOL_OUTPUT_BYTES || content.is_empty() {
            return Ok(output);
        }
        // JSON escaping grew the envelope past the cap; cut the text further.
        limit = content.len() * 3 / 4;
    }
}

fn is_html(media_type: &str) -> bool {
    matches!(media_type, "text/html" | "application/xhtml+xml")
}

/// The byte budget for the text returned to the model.
fn content_limit(params: &WebFetchToolParam) -> usize {
    params.max_content_tokens.map_or(MAX_CONTENT_BYTES, |tokens| {
        usize::try_from(tokens)
            .unwrap_or(usize::MAX)
            .saturating_mul(BYTES_PER_CONTENT_TOKEN)
            .min(MAX_CONTENT_BYTES)
    })
}

impl ToolHandler for WebFetchHandler {
    type ToolParams = WebFetchToolParam;

    fn tool_type(&self) -> ToolType {
        ToolType::WebFetch
    }

    /// Declaration fields are validated when the Messages request is
    /// normalized, before a registry exists.
    fn validate(&self, _params: &WebFetchToolParam) -> Result<(), ToolError> {
        Ok(())
    }

    fn normalize(&self, _params: &WebFetchToolParam) -> Vec<FunctionTool> {
        vec![web_fetch_function_tool()]
    }
}

impl GatewayExecutor for WebFetchHandler {
    type ExecutionParams = WebFetchToolParam;

    fn execute(
        &self,
        call_id: &str,
        tool_name: &str,
        arguments: &str,
        params: &WebFetchToolParam,
    ) -> Pin<Box<dyn Future<Output = Result<ToolOutput, ToolError>> + Send + '_>> {
        let call_id = call_id.to_owned();
        let tool_name = tool_name.to_owned();
        let arguments = arguments.to_owned();
        let params = params.clone();
        Box::pin(async move {
            if tool_name != WEB_FETCH_TOOL_NAME {
                return Err(ToolError::Config(format!(
                    "web_fetch handler cannot execute tool '{tool_name}'"
                )));
            }
            self.execute_fetch(&call_id, &arguments, &params).await
        })
    }

    fn supports_parallel_execution(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use url::Url;

    use super::*;
    use crate::config::DEFAULT_MAX_CONCURRENT_GATEWAY_CALLS;
    use crate::types::tools::WebSearchFilters;

    fn handler(backend: Arc<dyn WebFetchBackend>) -> WebFetchHandler {
        WebFetchHandler::with_backend(backend, DEFAULT_MAX_CONCURRENT_GATEWAY_CALLS)
    }

    /// A backend that answers every URL with one fixed document and records
    /// nothing; failures are injected through `failure`.
    #[derive(Debug)]
    struct FixedBackend {
        media_type: &'static str,
        body: String,
        truncated: bool,
        failure: Option<FetchFailure>,
    }

    impl FixedBackend {
        fn html(body: &str) -> Arc<Self> {
            Arc::new(Self {
                media_type: "text/html",
                body: body.to_owned(),
                truncated: false,
                failure: None,
            })
        }

        fn failing(failure: FetchFailure) -> Arc<Self> {
            Arc::new(Self {
                media_type: "text/plain",
                body: String::new(),
                truncated: false,
                failure: Some(failure),
            })
        }
    }

    impl WebFetchBackend for FixedBackend {
        fn fetch<'a>(
            &'a self,
            url: &'a Url,
            _filter: &'a DomainFilter,
        ) -> Pin<Box<dyn Future<Output = Result<FetchedDocument, FetchFailure>> + Send + 'a>> {
            Box::pin(async move {
                if let Some(failure) = &self.failure {
                    return Err(failure.clone());
                }
                Ok(FetchedDocument {
                    url: url.clone(),
                    media_type: self.media_type.to_owned(),
                    body: self.body.clone(),
                    truncated: self.truncated,
                })
            })
        }
    }

    async fn run(handler: &WebFetchHandler, arguments: &str, params: &WebFetchToolParam) -> Value {
        let output = handler
            .execute("call_fetch", WEB_FETCH_TOOL_NAME, arguments, params)
            .await
            .expect("documented failures are outputs, not errors");
        assert_eq!(output.call_id, "call_fetch");
        serde_json::from_str(&output.output).expect("JSON output")
    }

    fn error_code(output: &Value) -> &str {
        assert_eq!(output["type"], "web_fetch_tool_result_error", "{output}");
        output["error_code"].as_str().expect("error_code")
    }

    #[test]
    fn upstream_schema_requires_a_single_url() {
        let tool = web_fetch_function_tool();
        assert_eq!(tool.name, "web_fetch");
        let parameters = tool.parameters.expect("parameters");
        assert_eq!(parameters["required"], serde_json::json!(["url"]));
        assert_eq!(parameters["properties"]["url"]["type"], "string");
    }

    #[test]
    fn failure_output_uses_the_documented_shape_and_is_recognised() {
        let output = failure_output(WebFetchErrorCode::UrlNotInPriorContext, "not seen");
        assert_eq!(
            output,
            r#"{"type":"web_fetch_tool_result_error","error_code":"url_not_in_prior_context","message":"not seen"}"#
        );
        assert!(is_failure_output(&output));
        assert!(!is_failure_output(
            r#"{"type":"web_fetch_result","url":"https://example.com/"}"#
        ));
    }

    #[test]
    fn requested_fetches_and_url_follow_the_parsed_arguments() {
        assert_eq!(requested_fetches(r#"{"url":" https://example.com/a "}"#), 1);
        assert_eq!(
            requested_url(r#"{"url":" https://example.com/a "}"#).as_deref(),
            Some("https://example.com/a")
        );
        for rejected in [r#"{"url":""}"#, r#"{"url":5}"#, "{}", "{not json", r#"{"urls":["x"]}"#] {
            assert_eq!(requested_fetches(rejected), 0, "{rejected}");
            assert_eq!(requested_url(rejected), None, "{rejected}");
        }
    }

    #[tokio::test]
    async fn html_is_extracted_into_a_titled_text_result() {
        let handler = handler(FixedBackend::html(
            "<html><head><title>Doc &amp; Co</title></head><body><h1>Hi</h1><p>Body <b>text</b>.</p></body></html>",
        ));
        let output = run(
            &handler,
            r#"{"url":"https://example.com/doc"}"#,
            &WebFetchToolParam::default(),
        )
        .await;
        assert_eq!(output["type"], "web_fetch_result");
        assert_eq!(output["url"], "https://example.com/doc");
        assert_eq!(output["title"], "Doc & Co");
        assert_eq!(output["content_type"], "text/html");
        assert_eq!(output["content"], "Hi\nBody text.");
        assert_eq!(output["truncated"], false);
        assert!(output["retrieved_at"].as_str().unwrap().ends_with('Z'));
    }

    #[tokio::test]
    async fn plain_text_passes_through_and_the_content_limit_truncates() {
        let backend = Arc::new(FixedBackend {
            media_type: "text/plain",
            body: "é".repeat(100),
            truncated: false,
            failure: None,
        });
        let handler = handler(backend);
        let params = WebFetchToolParam {
            filters: None,
            max_content_tokens: Some(10),
        };
        let output = run(&handler, r#"{"url":"https://example.com/t"}"#, &params).await;
        assert!(output.get("title").is_none(), "plain text has no title");
        // 10 tokens * 4 bytes = 40 bytes; `é` is 2 bytes, so 20 characters survive.
        assert_eq!(output["content"], "é".repeat(20));
        assert_eq!(output["truncated"], true);

        let untouched = run(
            &handler,
            r#"{"url":"https://example.com/t"}"#,
            &WebFetchToolParam::default(),
        )
        .await;
        assert_eq!(untouched["content"], "é".repeat(100));
        assert_eq!(untouched["truncated"], false);
    }

    #[tokio::test]
    async fn a_backend_truncation_is_reported() {
        let backend = Arc::new(FixedBackend {
            media_type: "text/plain",
            body: "partial".to_owned(),
            truncated: true,
            failure: None,
        });
        let handler = handler(backend);
        let output = run(
            &handler,
            r#"{"url":"https://example.com/big"}"#,
            &WebFetchToolParam::default(),
        )
        .await;
        assert_eq!(output["truncated"], true);
        assert_eq!(output["content"], "partial");
    }

    #[tokio::test]
    async fn output_stays_under_the_gateway_cap_after_json_escaping() {
        let backend = Arc::new(FixedBackend {
            media_type: "text/plain",
            body: "\"\n".repeat(MAX_GATEWAY_TOOL_OUTPUT_BYTES),
            truncated: false,
            failure: None,
        });
        let handler = handler(backend);
        let output = handler
            .execute(
                "c",
                WEB_FETCH_TOOL_NAME,
                r#"{"url":"https://example.com/escaped"}"#,
                &WebFetchToolParam::default(),
            )
            .await
            .unwrap();
        assert!(
            output.output.len() <= MAX_GATEWAY_TOOL_OUTPUT_BYTES,
            "{}",
            output.output.len()
        );
        let value: Value = serde_json::from_str(&output.output).unwrap();
        assert_eq!(value["truncated"], true);
        assert!(!value["content"].as_str().unwrap().is_empty());
    }

    #[tokio::test]
    async fn invalid_arguments_and_urls_report_documented_codes_without_fetching() {
        let handler = handler(FixedBackend::failing(FetchFailure::Unavailable(
            "must not be reached".to_owned(),
        )));
        let params = WebFetchToolParam::default();
        for (arguments, expected) in [
            (r#"{"url":""}"#, "invalid_tool_input"),
            ("{not json", "invalid_tool_input"),
            (r#"{"url":"example.com/page"}"#, "invalid_tool_input"),
            (r#"{"url":"ftp://example.com/f"}"#, "invalid_tool_input"),
            (r#"{"url":"https://user:pw@example.com/"}"#, "url_not_allowed"),
        ] {
            let output = run(&handler, arguments, &params).await;
            assert_eq!(error_code(&output), expected, "{arguments}");
        }
        let long = format!(r#"{{"url":"https://example.com/{}"}}"#, "x".repeat(300));
        assert_eq!(error_code(&run(&handler, &long, &params).await), "url_too_long");
    }

    #[tokio::test]
    async fn domain_filters_apply_to_the_requested_url() {
        let handler = handler(FixedBackend::html("<p>ok</p>"));
        let allowlist = WebFetchToolParam {
            filters: Some(WebSearchFilters {
                allowed_domains: Some(vec!["example.com".to_owned()]),
                blocked_domains: None,
            }),
            max_content_tokens: None,
        };
        let allowed = run(&handler, r#"{"url":"https://docs.example.com/a"}"#, &allowlist).await;
        assert_eq!(allowed["content"], "ok");
        let refused = run(&handler, r#"{"url":"https://example.org/a"}"#, &allowlist).await;
        assert_eq!(error_code(&refused), "url_not_allowed");

        let blocklist = WebFetchToolParam {
            filters: Some(WebSearchFilters {
                allowed_domains: None,
                blocked_domains: Some(vec!["example.com".to_owned()]),
            }),
            max_content_tokens: None,
        };
        let refused = run(&handler, r#"{"url":"https://api.example.com/a"}"#, &blocklist).await;
        assert_eq!(error_code(&refused), "url_not_allowed");
        let allowed = run(&handler, r#"{"url":"https://example.org/a"}"#, &blocklist).await;
        assert_eq!(allowed["content"], "ok");
    }

    #[tokio::test]
    async fn backend_failures_map_onto_documented_codes() {
        let params = WebFetchToolParam::default();
        for (failure, expected) in [
            (FetchFailure::NotAllowed("private".to_owned()), "url_not_allowed"),
            (FetchFailure::NotAccessible("HTTP 404".to_owned()), "url_not_accessible"),
            (FetchFailure::TooManyRequests, "too_many_requests"),
            (
                FetchFailure::UnsupportedContentType("application/pdf".to_owned()),
                "unsupported_content_type",
            ),
            (FetchFailure::Unavailable("boom".to_owned()), "unavailable"),
        ] {
            let handler = handler(FixedBackend::failing(failure));
            let output = run(&handler, r#"{"url":"https://example.com/x"}"#, &params).await;
            assert_eq!(error_code(&output), expected);
            assert!(output["message"].as_str().is_some_and(|message| !message.is_empty()));
        }
    }

    #[tokio::test]
    async fn only_the_web_fetch_name_is_executed() {
        let handler = handler(FixedBackend::html("<p>ok</p>"));
        let error = handler
            .execute(
                "c",
                "web_search",
                r#"{"url":"https://example.com/"}"#,
                &WebFetchToolParam::default(),
            )
            .await
            .unwrap_err();
        assert!(matches!(error, ToolError::Config(message) if message.contains("cannot execute tool 'web_search'")));
    }

    /// A backend that records how many fetches are in flight at once.
    #[derive(Debug, Default)]
    struct CountingBackend {
        in_flight: AtomicUsize,
        peak: AtomicUsize,
    }

    impl WebFetchBackend for CountingBackend {
        fn fetch<'a>(
            &'a self,
            url: &'a Url,
            _filter: &'a DomainFilter,
        ) -> Pin<Box<dyn Future<Output = Result<FetchedDocument, FetchFailure>> + Send + 'a>> {
            Box::pin(async move {
                let active = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
                self.peak.fetch_max(active, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(20)).await;
                self.in_flight.fetch_sub(1, Ordering::SeqCst);
                Ok(FetchedDocument {
                    url: url.clone(),
                    media_type: "text/plain".to_owned(),
                    body: "ok".to_owned(),
                    truncated: false,
                })
            })
        }
    }

    #[tokio::test]
    async fn fetches_in_flight_are_capped_by_the_concurrency_ceiling() {
        let backend = Arc::new(CountingBackend::default());
        let handler = WebFetchHandler::with_backend(
            Arc::clone(&backend) as Arc<dyn WebFetchBackend>,
            NonZeroUsize::new(2).expect("nonzero"),
        );
        let params = WebFetchToolParam::default();
        let ids: Vec<String> = (0..6).map(|index| format!("c{index}")).collect();
        let outputs = futures::future::join_all(
            ids.iter()
                .map(|id| handler.execute(id, WEB_FETCH_TOOL_NAME, r#"{"url":"https://example.com/"}"#, &params)),
        )
        .await;
        assert!(outputs.iter().all(Result::is_ok));
        assert_eq!(
            backend.peak.load(Ordering::SeqCst),
            2,
            "at most two fetches ran at once"
        );
        assert_eq!(backend.in_flight.load(Ordering::SeqCst), 0);
    }
}
