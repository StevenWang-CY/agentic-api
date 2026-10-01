//! The contract between the `web_fetch` handler and the backend that retrieves
//! a page.
//!
//! The handler owns the model-facing policy (argument parsing, URL admission,
//! domain filtering, text extraction, content limits, output shape); a backend
//! only turns an admitted URL into a document. The built-in backend is
//! [`HttpFetchBackend`](super::http::HttpFetchBackend). The trait is
//! crate-private: replacing the retriever (with an extraction service, say) is
//! a change to this module, not to the Messages loop.

use std::fmt;
use std::future::Future;
use std::pin::Pin;

use url::Url;

use crate::tool::web_search::args::DomainFilter;

/// A page body a backend retrieved, before text extraction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FetchedDocument {
    /// The URL that answered, after redirects.
    pub url: Url,
    /// Lowercase media type of the body without parameters, e.g. `text/html`.
    pub media_type: String,
    /// The body decoded to text. HTML is converted to plain text by the handler.
    pub body: String,
    /// Whether the backend cut the body at its download ceiling.
    pub truncated: bool,
}

/// Why a fetch produced no document. Each variant maps onto one of the
/// documented `web_fetch_tool_result_error` codes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum FetchFailure {
    /// The URL, or a redirect target, is outside what the gateway fetches.
    NotAllowed(String),
    /// The page could not be retrieved: connection, timeout, or HTTP status.
    NotAccessible(String),
    /// The origin answered HTTP 429.
    TooManyRequests,
    /// The body is not text, HTML, or another supported text form.
    UnsupportedContentType(String),
    /// The backend itself failed.
    Unavailable(String),
}

impl fmt::Display for FetchFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotAllowed(reason) | Self::NotAccessible(reason) | Self::Unavailable(reason) => f.write_str(reason),
            Self::TooManyRequests => f.write_str("the origin rate-limited the request (HTTP 429)"),
            Self::UnsupportedContentType(media_type) => write!(f, "content type {media_type:?} is not supported"),
        }
    }
}

/// A page-retrieval backend behind `web_fetch`.
///
/// `url` has passed [`policy::validate_url`](super::policy::validate_url) and
/// `filter`; a backend that follows redirects must re-apply both to every hop
/// and refuse non-public addresses unless configured otherwise.
pub(crate) trait WebFetchBackend: fmt::Debug + Send + Sync {
    fn fetch<'a>(
        &'a self,
        url: &'a Url,
        filter: &'a DomainFilter,
    ) -> Pin<Box<dyn Future<Output = Result<FetchedDocument, FetchFailure>> + Send + 'a>>;
}
