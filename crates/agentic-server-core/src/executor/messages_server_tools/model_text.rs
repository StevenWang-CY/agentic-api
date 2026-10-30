//! What the model reads for a native server tool call: the `tool_result` text
//! projected from a result block, and the sealed form of a search result.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::tool::web_fetch::failure_output;
use crate::tool::web_search::args::WebSearchArguments;
use crate::tool::web_search::provider::WebSearchResult;
use crate::types::messages::{
    GatewayToolResult, ServerToolResultBlock, WebFetchErrorCode, WebFetchResult, WebFetchToolResultContent,
    WebSearchErrorCode, WebSearchToolResultContent, tool_result_block,
};

/// Domain separation for the associated data of a sealed search result.
const SEARCH_RESULT_AAD: &[u8] = b"agentic-api/messages/web_search_result/v1";

/// One search result as sealed into `encrypted_content`: the result exactly as
/// the executor returned it, and the section it came from.
#[derive(Serialize, Deserialize)]
pub(super) struct SealedSearchResult {
    pub(super) section: SearchSection,
    pub(super) result: WebSearchResult,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum SearchSection {
    Web,
    News,
}

/// The parts of the executor's `web_search` output a native call returns.
#[derive(Deserialize)]
pub(super) struct ExecutorSearchOutput {
    pub(super) results: ExecutorSearchSections,
}

#[derive(Deserialize)]
pub(super) struct ExecutorSearchSections {
    #[serde(default)]
    pub(super) web: Vec<WebSearchResult>,
    #[serde(default)]
    pub(super) news: Vec<WebSearchResult>,
}

/// The executor's `web_fetch` output for a fetched page.
#[derive(Deserialize)]
pub(super) struct ExecutorFetchOutput {
    pub(super) url: String,
    #[serde(default)]
    pub(super) title: Option<String>,
    pub(super) content_type: String,
    pub(super) retrieved_at: String,
    pub(super) truncated: bool,
    pub(super) content: String,
}

/// The executor's `web_fetch` output for a refused or failed fetch.
#[derive(Deserialize)]
pub(super) struct ExecutorFetchFailure {
    pub(super) error_code: WebFetchErrorCode,
    pub(super) message: String,
}

/// The model-facing text of a native search; field order follows the
/// executor's output, without its provider metadata.
#[derive(Serialize)]
struct SearchText<'a> {
    query: &'a str,
    queries: &'a [String],
    results: SearchSections,
}

#[derive(Serialize)]
struct SearchSections {
    web: Vec<WebSearchResult>,
    news: Vec<WebSearchResult>,
}

/// The model-facing text of a fetched page, from the fields its result block
/// carries; with all of them present it is the executor's output exactly.
#[derive(Serialize)]
struct FetchText<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    url: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    title: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    content_type: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    retrieved_at: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    truncated: Option<bool>,
    content: &'a str,
}

/// The `tool_result` text for a search that did not run or did not complete.
pub(crate) fn search_error_text(code: WebSearchErrorCode) -> &'static str {
    match code {
        WebSearchErrorCode::MaxUsesExceeded => "web_search max_uses exceeded; search was not run",
        WebSearchErrorCode::InvalidToolInput => "web_search input was invalid; search was not run",
        WebSearchErrorCode::TooManyRequests => "web_search was rate limited; search was not run",
        WebSearchErrorCode::QueryTooLong => "web_search query was too long; search was not run",
        WebSearchErrorCode::RequestTooLarge => "web_search request was too large; search was not run",
        WebSearchErrorCode::Unavailable => "web_search is unavailable; search did not complete",
    }
}

/// The message of a fetch that produced no document, by its documented code.
pub(crate) fn fetch_error_message(code: WebFetchErrorCode) -> &'static str {
    match code {
        WebFetchErrorCode::InvalidToolInput => "the web_fetch input was invalid; page was not fetched",
        WebFetchErrorCode::UrlTooLong => "the url is too long; page was not fetched",
        WebFetchErrorCode::UrlNotAllowed => "the url is not allowed; page was not fetched",
        WebFetchErrorCode::UrlNotInPriorContext => {
            "the url did not appear earlier in the conversation; only a url from a user message or a tool result \
             can be fetched"
        }
        WebFetchErrorCode::UrlNotAccessible => "the page could not be retrieved",
        WebFetchErrorCode::TooManyRequests => "the site refused further requests; page was not fetched",
        WebFetchErrorCode::UnsupportedContentType => "the page's content type is not supported",
        WebFetchErrorCode::MaxUsesExceeded => "web_fetch max_uses exceeded; page was not fetched",
        WebFetchErrorCode::Unavailable => "web_fetch is unavailable; page was not fetched",
    }
}

/// The note the model sees for search results sealed under a key this gateway
/// no longer holds.
pub(super) const UNAVAILABLE_RESULTS_TEXT: &str = "these web_search results were sealed under another gateway key and are no longer available; search again if \
     they are needed";

/// The associated data binding one sealed search result to its call, the
/// queries the call ran, and its position among the call's results. Every
/// variable-length field is length-prefixed, so no two bindings encode alike.
pub(super) fn search_result_aad(upstream_id: &str, queries: &[String], index: usize, count: usize) -> Vec<u8> {
    let mut aad = SEARCH_RESULT_AAD.to_vec();
    let mut put = |bytes: &[u8]| {
        aad.extend_from_slice(&u64::try_from(bytes.len()).unwrap_or(u64::MAX).to_be_bytes());
        aad.extend_from_slice(bytes);
    };
    put(upstream_id.as_bytes());
    for query in queries {
        put(query.as_bytes());
    }
    for number in [queries.len(), index, count] {
        aad.extend_from_slice(&u64::try_from(number).unwrap_or(u64::MAX).to_be_bytes());
    }
    aad
}

/// The queries a search call ran, read from its input the way the executor reads them.
pub(super) fn search_queries(input: &Value) -> Option<Vec<String>> {
    let arguments = serde_json::to_string(input).ok()?;
    WebSearchArguments::from_json(&arguments)
        .ok()
        .map(|arguments| arguments.queries().to_vec())
}

/// The `tool_result` text of a search, from the queries it ran and its sealed results.
pub(super) fn search_text(queries: &[String], payloads: &[Vec<u8>]) -> Option<String> {
    let mut sections = SearchSections {
        web: Vec::new(),
        news: Vec::new(),
    };
    for payload in payloads {
        let sealed: SealedSearchResult = serde_json::from_slice(payload).ok()?;
        match sealed.section {
            SearchSection::Web => sections.web.push(sealed.result),
            SearchSection::News => sections.news.push(sealed.result),
        }
    }
    serde_json::to_string(&SearchText {
        query: queries.first()?,
        queries,
        results: sections,
    })
    .ok()
}

pub(super) fn fetch_text(result: &WebFetchResult) -> String {
    let text = FetchText {
        kind: "web_fetch_result",
        url: &result.url,
        title: result.content.title.as_deref(),
        content_type: result.content_type.as_deref(),
        retrieved_at: result.retrieved_at.as_deref(),
        truncated: result.truncated,
        content: &result.content.source.data,
    };
    serde_json::to_string(&text).unwrap_or_default()
}

/// The `tool_result` the model sees for a non-search result block.
pub(super) fn projected_tool_result(upstream_id: &str, block: &ServerToolResultBlock) -> Option<GatewayToolResult> {
    let (text, is_error) = match block {
        ServerToolResultBlock::WebSearchToolResult {
            content: WebSearchToolResultContent::Error(error),
            ..
        } => (search_error_text(error.error_code).to_owned(), true),
        ServerToolResultBlock::WebSearchToolResult {
            content: WebSearchToolResultContent::Results(_),
            ..
        } => return None,
        ServerToolResultBlock::WebFetchToolResult {
            content: WebFetchToolResultContent::WebFetchResult(result),
            ..
        } => (fetch_text(result), false),
        ServerToolResultBlock::WebFetchToolResult {
            content: WebFetchToolResultContent::WebFetchToolResultError { error_code, message },
            ..
        } => (
            failure_output(
                *error_code,
                message.as_deref().unwrap_or_else(|| fetch_error_message(*error_code)),
            ),
            true,
        ),
    };
    Some(tool_result_block(upstream_id, text, is_error))
}
