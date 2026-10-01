//! Declaration parameters for the gateway-executed `web_fetch` tool.
//!
//! A native Messages `web_fetch_20250910` declaration is classified by the
//! Messages tool seam and carried into the request-scoped registry as
//! [`ResponsesTool::WebFetch`](super::ResponsesTool::WebFetch). The tool has no
//! Responses wire form, so this shape is never deserialized from a request
//! body; it holds only what the handler needs for every call of one request.

use serde::{Deserialize, Serialize};

use super::params::WebSearchFilters;

/// Per-request settings of one `web_fetch` declaration.
///
/// `max_uses` is not here: the Messages loop enforces it as a request-wide
/// budget before a call is dispatched, the same way it does for `web_search`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WebFetchToolParam {
    /// `allowed_domains` / `blocked_domains`, matched on the URL host only.
    pub filters: Option<WebSearchFilters>,
    /// Approximate ceiling on the text returned to the model, in tokens.
    pub max_content_tokens: Option<u32>,
}
