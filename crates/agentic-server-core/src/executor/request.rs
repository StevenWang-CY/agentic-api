use std::sync::Arc;
use std::time::Duration;

use crate::config::Config;
use crate::error::Error;
use crate::executor::modes::{ConversationHandler, ResponseHandler};
use crate::storage::{ConversationStore, ResponseStore, create_pool_with_schema_and_configs};
use crate::tool::{GatewayExecutor, GatewayExecutors};
use crate::types::io::InputItem;
use crate::types::messages::GatewayToolMap;
use crate::types::request_response::{RequestPayload, ResponsePayload};

/// Env var configuring client-tool → gateway-executor aliases for `/v1/messages`
/// (e.g. `WebSearch=web_search`). Empty/unset means no aliases — client
/// functions stay client-owned (the ownership doctrine's default).
const GATEWAY_TOOL_ALIASES_ENV: &str = "MESSAGES_GATEWAY_TOOL_ALIASES";

/// Context built by `rehydrate_conversation`, threaded through the execute pipeline.
#[derive(Debug)]
pub struct RequestContext {
    /// Untouched original request from the client.
    pub original_request: RequestPayload,
    /// Enriched request with rehydrated conversation history injected into `.input`.
    /// This is the request forwarded to the LLM.
    pub enriched_request: RequestPayload,
    /// Only the new input items submitted by the client this turn (used for persistence).
    pub new_input_items: Vec<InputItem>,
    /// Our generated response ID (uuid7 with "resp_" prefix).
    pub response_id: String,
    /// Resolved conversation ID. `None` when `store=false` or non-conversational.
    pub conversation_id: Option<String>,
}

impl RequestContext {
    /// Inject our `response_id` and `conversation_id` into a `ResponsePayload`
    /// received from the LLM (which carries the upstream's own IDs).
    pub(crate) fn inject_ids(&self, payload: &mut ResponsePayload) {
        payload.id.clone_from(&self.response_id);
        payload.conversation_id.clone_from(&self.conversation_id);
        payload
            .previous_response_id
            .clone_from(&self.original_request.previous_response_id);
    }
}

/// Runtime dependencies passed into `execute()`.
///
/// Owns the storage handlers, HTTP client, and LLM endpoint configuration.
/// Per-request auth is supplied via [`crate::executor::engine::ExecuteRequest::with_auth`]
/// rather than stored here, keeping this context purely shared and immutable.
#[derive(Clone, Debug)]
pub struct ExecutionContext {
    pub conv_handler: ConversationHandler,
    pub resp_handler: ResponseHandler,
    pub client: Arc<reqwest::Client>,
    pub gateway_executors: GatewayExecutors,
    /// Client-tool → gateway-executor aliases for the `/v1/messages` loop
    /// (e.g. Claude Code's `WebSearch` → `web_search`). Empty unless configured.
    pub messages_gateway_tools: GatewayToolMap,
    /// Base URL for the LLM backend, e.g. `"http://localhost:8000"`.
    pub llm_base_url: String,
    /// Maximum wait time for the next SSE chunk.  `Duration::ZERO` disables the timeout.
    /// Sourced from [`Config::streaming_chunk_timeout_s`](crate::config::Config::streaming_chunk_timeout_s).
    pub streaming_timeout: Duration,
}

impl ExecutionContext {
    /// Returns the full URL for the `/v1/responses` endpoint.
    #[must_use]
    pub fn responses_url(&self) -> String {
        format!("{}/v1/responses", self.llm_base_url)
    }

    /// Returns the full URL for the `/v1/conversations` endpoint.
    #[must_use]
    pub fn conversations_url(&self) -> String {
        format!("{}/v1/conversations", self.llm_base_url)
    }

    #[must_use]
    pub fn new(
        conv_handler: ConversationHandler,
        resp_handler: ResponseHandler,
        client: Arc<reqwest::Client>,
        llm_base_url: String,
    ) -> Self {
        let gateway_executors = GatewayExecutors::from_env(Arc::clone(&client));
        Self {
            conv_handler,
            resp_handler,
            client,
            gateway_executors,
            messages_gateway_tools: messages_gateway_tools_from_env(),
            llm_base_url,
            streaming_timeout: Duration::from_secs(30),
        }
    }

    #[must_use]
    pub fn with_gateway_executor(mut self, executor: Arc<dyn GatewayExecutor>) -> Self {
        self.gateway_executors.insert(executor);
        self
    }

    /// Build an `ExecutionContext` directly from [`Config`](crate::config::Config).
    ///
    /// Creates the database pool, both storage handlers, and an HTTP client
    /// internally so callers don't need to depend on the storage layer.
    ///
    /// # Errors
    ///
    /// Returns an error if the database pool cannot be opened or the schema
    /// migration fails.
    pub async fn from_config(cfg: &Config) -> Result<Self, Error> {
        let db_url = cfg.db_url.as_deref().unwrap_or("sqlite://./agentic_api.db");
        let database_backend = database_backend(db_url);
        let pool = create_pool_with_schema_and_configs(Some(db_url), cfg.sqlite, cfg.postgres)
            .await
            .map_err(|error| database_open_error(database_backend, &error))?;

        let conv_handler = ConversationHandler::new(ConversationStore::new(pool.clone()));
        let resp_handler = ResponseHandler::new(ResponseStore::new(pool));
        let client = Arc::new(reqwest::Client::new());
        let gateway_executors = GatewayExecutors::from_env(Arc::clone(&client));

        Ok(Self {
            conv_handler,
            resp_handler,
            client,
            gateway_executors,
            messages_gateway_tools: messages_gateway_tools_from_env(),
            llm_base_url: cfg.llm_api_base.clone(),
            streaming_timeout: Duration::from_secs(30),
        })
    }
}

fn database_open_error(database_backend: &str, error: &sqlx::Error) -> Error {
    let category = match error {
        sqlx::Error::Configuration(_) | sqlx::Error::InvalidArgument(_) => "configuration error".to_owned(),
        sqlx::Error::Database(database_error) => database_error
            .code()
            .filter(|code| code.len() <= 5 && code.bytes().all(|byte| byte.is_ascii_alphanumeric()))
            .map_or_else(
                || "database error".to_owned(),
                |code| format!("database error (SQLSTATE {code})"),
            ),
        sqlx::Error::Io(_) => "database I/O error".to_owned(),
        sqlx::Error::Tls(_) => "database TLS error".to_owned(),
        sqlx::Error::Protocol(_) => "database protocol error".to_owned(),
        sqlx::Error::PoolTimedOut => "connection pool timeout".to_owned(),
        sqlx::Error::PoolClosed => "connection pool closed".to_owned(),
        sqlx::Error::WorkerCrashed => "database worker crashed".to_owned(),
        sqlx::Error::Migrate(_) => "database migration error".to_owned(),
        _ => "database error".to_owned(),
    };
    let detail = redact_database_urls(&error.to_string());
    Error::Config(format!(
        "failed to open {database_backend} database: {category}: {detail}"
    ))
}

fn redact_database_urls(message: &str) -> String {
    const DATABASE_SCHEMES: [&str; 3] = ["postgresql://", "postgres://", "mysql://"];

    let mut redacted = message.to_owned();
    for scheme in DATABASE_SCHEMES {
        let mut search_from = 0;
        while let Some(offset) = redacted[search_from..].find(scheme) {
            let start = search_from + offset;
            let end = redacted[start..]
                .find(char::is_whitespace)
                .map_or(redacted.len(), |length| start + length);
            let replacement = format!("{scheme}[redacted]");
            redacted.replace_range(start..end, &replacement);
            search_from = start + replacement.len();
        }
    }
    redacted
}

fn database_backend(database_url: &str) -> &'static str {
    if database_url.starts_with("postgres://") || database_url.starts_with("postgresql://") {
        "PostgreSQL"
    } else if database_url.starts_with("sqlite") {
        "SQLite"
    } else {
        "configured"
    }
}

/// Load the `/v1/messages` gateway-tool alias map from the environment.
fn messages_gateway_tools_from_env() -> GatewayToolMap {
    std::env::var(GATEWAY_TOOL_ALIASES_ENV)
        .ok()
        .map(|raw| GatewayToolMap::from_env_str(&raw))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::{database_backend, database_open_error};

    #[test]
    fn database_errors_are_actionable_without_exposing_credentials() {
        let database_url = "postgresql://agentic-api:super'secret@postgres.example.com/agentic_api";
        let backend = database_backend(database_url);
        let tls_error = sqlx::Error::Tls(Box::new(std::io::Error::other(format!(
            "{database_url} certificate verify failed"
        ))));
        let pool_error = sqlx::Error::PoolTimedOut;
        let tls_message = database_open_error(backend, &tls_error).to_string();
        let pool_message = database_open_error(backend, &pool_error).to_string();

        assert!(tls_message.contains("database TLS error"));
        assert!(tls_message.contains("certificate verify failed"));
        assert!(tls_message.contains("postgresql://[redacted]"));
        assert_eq!(
            pool_message,
            "failed to open PostgreSQL database: connection pool timeout: \
             pool timed out while waiting for an open connection"
        );
        assert!(!tls_message.contains("super-secret"));
        assert!(!tls_message.contains("secret"));
        assert!(!tls_message.contains("agentic-api"));
        assert_ne!(tls_message, pool_message);

        let mysql_error = sqlx::Error::Io(std::io::Error::other(
            "mysql://gateway:mysql-secret@mysql.example.com/agentic_api refused",
        ));
        let mysql_message = database_open_error("configured", &mysql_error).to_string();
        assert!(mysql_message.contains("mysql://[redacted]"));
        assert!(!mysql_message.contains("mysql-secret"));

        let short_postgres_url = "postgres://gateway:postgres-secret@postgres.example.com/agentic_api";
        let short_postgres_error = sqlx::Error::Io(std::io::Error::other(format!("{short_postgres_url} refused")));
        let short_postgres_message =
            database_open_error(database_backend(short_postgres_url), &short_postgres_error).to_string();
        assert!(short_postgres_message.contains("failed to open PostgreSQL database"));
        assert!(short_postgres_message.contains("postgres://[redacted]"));
        assert!(!short_postgres_message.contains("postgres-secret"));
    }
}
