use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, Request, StatusCode, header};
use axum::middleware::Next;
use axum::response::Response;
use jsonwebtoken::jwk::{Jwk, JwkSet, KeyAlgorithm, KeyOperations, PublicKeyUse};
use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode, decode_header};
use serde::Deserialize;
use serde_json::json;
use tokio::sync::RwLock;
use tracing::{debug, warn};
use url::{Host, Url};

const OIDC_HTTP_TIMEOUT: Duration = Duration::from_secs(10);
const JWKS_REFRESH_COOLDOWN: Duration = Duration::from_secs(30);
const JWKS_REFRESH_COALESCE_WINDOW: Duration = Duration::from_secs(1);
const DEFAULT_JWKS_TTL: Duration = Duration::from_secs(300);
const MAX_JWKS_TTL: Duration = Duration::from_secs(3600);
const MAX_PROVIDER_RESPONSE_BYTES: usize = 1024 * 1024;
const MAX_JWKS_KEYS: usize = 100;
const JWT_CLOCK_SKEW_SECONDS: u64 = 60;

pub(crate) const ANTHROPIC_MESSAGES_PATH: &str = "/v1/messages";
pub(crate) const ANTHROPIC_COUNT_TOKENS_PATH: &str = "/v1/messages/count_tokens";

#[derive(Clone)]
pub struct OidcConfig {
    issuer: Url,
    issuer_value: String,
    audience: String,
    allows_loopback_http: bool,
}

impl OidcConfig {
    /// Create an OIDC bearer-token configuration.
    ///
    /// # Errors
    ///
    /// Returns an error when the issuer is not an absolute HTTPS URL (except
    /// for loopback test/development issuers) or the audience is empty.
    pub fn new(issuer: &str, audience: &str) -> Result<Self, OidcAuthError> {
        let issuer_value = issuer.trim().to_owned();
        let issuer = Url::parse(&issuer_value).map_err(OidcAuthError::InvalidIssuer)?;
        let allows_loopback_http = is_loopback_http(&issuer);
        if issuer.scheme() != "https" && !allows_loopback_http {
            return Err(OidcAuthError::InsecureIssuer);
        }
        if issuer.query().is_some() || issuer.fragment().is_some() {
            return Err(OidcAuthError::InvalidIssuerComponents);
        }

        let audience = audience.trim().to_owned();
        if audience.is_empty() {
            return Err(OidcAuthError::EmptyAudience);
        }

        Ok(Self {
            issuer,
            issuer_value,
            audience,
            allows_loopback_http,
        })
    }

    fn discovery_url(&self) -> Result<Url, OidcAuthError> {
        Url::parse(&format!(
            "{}/.well-known/openid-configuration",
            self.issuer.as_str().trim_end_matches('/')
        ))
        .map_err(OidcAuthError::InvalidIssuer)
    }
}

fn is_loopback_http(url: &Url) -> bool {
    url.scheme() == "http"
        && match url.host() {
            Some(Host::Ipv4(address)) => address.is_loopback(),
            Some(Host::Ipv6(address)) => address.is_loopback(),
            Some(Host::Domain(_)) | None => false,
        }
}

struct CachedKey {
    decoding_key: DecodingKey,
    algorithm: Option<Algorithm>,
}

struct CachedJwks {
    keys: HashMap<String, Arc<CachedKey>>,
    expires_at: Instant,
}

struct RefreshState {
    last_completed: Instant,
    retry_after: Option<Instant>,
    coalesce_until: Option<Instant>,
}

#[derive(Clone)]
pub struct OidcAuthenticator {
    audience: String,
    jwks_uri: Url,
    keys: Arc<RwLock<CachedJwks>>,
    refresh_state: Arc<std::sync::Mutex<RefreshState>>,
    refresh_gate: Arc<tokio::sync::Semaphore>,
    validations: Arc<Vec<(Algorithm, Validation)>>,
    client: reqwest::Client,
}

impl OidcAuthenticator {
    /// Discover an OIDC provider and cache its initial verification keys.
    ///
    /// # Errors
    ///
    /// Returns an error when provider discovery or the JSON Web Key Set
    /// (JWKS) request fails, or when the discovered metadata is inconsistent.
    pub async fn discover(config: OidcConfig) -> Result<Self, OidcAuthError> {
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(OIDC_HTTP_TIMEOUT)
            .build()
            .map_err(OidcAuthError::HttpClient)?;
        let (metadata, _) =
            fetch_json::<ProviderMetadata>(&client, config.discovery_url()?, ProviderRequest::Metadata).await?;
        let discovered_issuer = Url::parse(&metadata.issuer).map_err(OidcAuthError::InvalidDiscoveredIssuer)?;
        if discovered_issuer != config.issuer {
            return Err(OidcAuthError::IssuerMismatch {
                expected: config.issuer_value,
                discovered: metadata.issuer,
            });
        }

        let jwks_uri = Url::parse(&metadata.jwks_uri).map_err(OidcAuthError::InvalidJwksUri)?;
        if jwks_uri.scheme() != "https" && !(config.allows_loopback_http && is_loopback_http(&jwks_uri)) {
            return Err(OidcAuthError::InsecureJwksUri);
        }

        let keys = fetch_jwks(&client, jwks_uri.clone()).await?;
        let refresh_completed = Instant::now();
        let validations = build_validations(&metadata.issuer, &config.audience);
        Ok(Self {
            audience: config.audience,
            jwks_uri,
            keys: Arc::new(RwLock::new(keys)),
            refresh_state: Arc::new(std::sync::Mutex::new(RefreshState {
                last_completed: refresh_completed,
                retry_after: None,
                coalesce_until: None,
            })),
            refresh_gate: Arc::new(tokio::sync::Semaphore::new(1)),
            validations: Arc::new(validations),
            client,
        })
    }

    async fn authenticate(&self, token: &str) -> Result<AuthenticatedPrincipal, OidcAuthError> {
        let token_header = decode_header(token).map_err(OidcAuthError::InvalidToken)?;
        if matches!(token_header.alg, Algorithm::HS256 | Algorithm::HS384 | Algorithm::HS512) {
            return Err(OidcAuthError::UnsupportedTokenAlgorithm);
        }
        let kid = token_header.kid.ok_or(OidcAuthError::MissingKeyId)?;
        let key = self.verification_key(&kid).await?;
        if key.algorithm.is_some_and(|algorithm| algorithm != token_header.alg) {
            return Err(OidcAuthError::AlgorithmMismatch);
        }
        let validation = self
            .validations
            .iter()
            .find_map(|(algorithm, validation)| (*algorithm == token_header.alg).then_some(validation))
            .ok_or(OidcAuthError::UnsupportedTokenAlgorithm)?;

        let claims = decode::<IdentityClaims>(token, &key.decoding_key, validation)
            .map_err(OidcAuthError::InvalidToken)?
            .claims;
        if claims.sub.is_empty() {
            return Err(OidcAuthError::EmptySubject);
        }
        if !claims.audience_allows(&self.audience) {
            return Err(OidcAuthError::InvalidAuthorizedParty);
        }
        Ok(AuthenticatedPrincipal {
            issuer: claims.iss,
            subject: claims.sub,
            expires_at: claims.exp,
        })
    }

    async fn verification_key(&self, kid: &str) -> Result<Arc<CachedKey>, OidcAuthError> {
        {
            let keys = self.keys.read().await;
            if Instant::now() < keys.expires_at {
                if let Some(key) = keys.keys.get(kid) {
                    return Ok(Arc::clone(key));
                }
            }
        }

        let _refresh_permit = self
            .refresh_gate
            .acquire()
            .await
            .map_err(|_| OidcAuthError::RefreshStateUnavailable)?;
        {
            let keys = self.keys.read().await;
            let refresh_state = self
                .refresh_state
                .lock()
                .map_err(|_| OidcAuthError::RefreshStateUnavailable)?;
            let now = Instant::now();
            if now < keys.expires_at {
                if let Some(key) = keys.keys.get(kid) {
                    return Ok(Arc::clone(key));
                }
                if now.duration_since(refresh_state.last_completed) < JWKS_REFRESH_COOLDOWN {
                    return Err(OidcAuthError::UnknownKeyId);
                }
            }
            if refresh_state.coalesce_until.is_some_and(|deadline| now < deadline) {
                return keys.keys.get(kid).cloned().ok_or(OidcAuthError::UnknownKeyId);
            }
            if refresh_state.retry_after.is_some_and(|deadline| now < deadline) {
                return Err(OidcAuthError::JwksRefreshBackoff);
            }
        }

        self.refresh_state
            .lock()
            .map_err(|_| OidcAuthError::RefreshStateUnavailable)?
            .retry_after = Some(Instant::now() + JWKS_REFRESH_COOLDOWN);
        let refreshed = fetch_jwks(&self.client, self.jwks_uri.clone()).await?;
        let key = refreshed.keys.get(kid).cloned();
        *self.keys.write().await = refreshed;
        let refresh_completed = Instant::now();
        let mut refresh_state = self
            .refresh_state
            .lock()
            .map_err(|_| OidcAuthError::RefreshStateUnavailable)?;
        refresh_state.last_completed = refresh_completed;
        refresh_state.retry_after = None;
        refresh_state.coalesce_until = Some(refresh_completed + JWKS_REFRESH_COALESCE_WINDOW);
        key.ok_or(OidcAuthError::UnknownKeyId)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AuthenticatedPrincipal {
    issuer: String,
    subject: String,
    expires_at: u64,
}

impl AuthenticatedPrincipal {
    #[must_use]
    pub fn issuer(&self) -> &str {
        &self.issuer
    }

    #[must_use]
    pub fn subject(&self) -> &str {
        &self.subject
    }

    #[must_use]
    pub fn expires_at(&self) -> u64 {
        self.expires_at
    }

    pub(crate) fn is_expired(&self) -> bool {
        self.is_expired_at(jsonwebtoken::get_current_timestamp())
    }

    fn is_expired_at(&self, timestamp: u64) -> bool {
        timestamp > self.expires_at.saturating_add(JWT_CLOCK_SKEW_SECONDS)
    }

    #[cfg(test)]
    pub(crate) fn expired_for_test() -> Self {
        Self {
            issuer: "https://issuer.example".to_owned(),
            subject: "subject".to_owned(),
            expires_at: 0,
        }
    }
}

pub async fn require_oidc(
    State(authenticator): State<OidcAuthenticator>,
    mut request: Request<Body>,
    next: Next,
) -> Response {
    let error_format = AuthErrorFormat::for_path(request.uri().path());
    let Some(token) = bearer_token(request.headers()) else {
        return authentication_error(error_format, "missing_bearer_token", "missing bearer token");
    };
    let duplicate_identity_api_key = request
        .headers()
        .get_all("x-api-key")
        .iter()
        .any(|value| value.to_str().ok().is_some_and(|value| value.trim() == token));

    match authenticator.authenticate(token).await {
        Ok(principal) => {
            request.headers_mut().remove(header::AUTHORIZATION);
            if duplicate_identity_api_key {
                request.headers_mut().remove("x-api-key");
            }
            request.extensions_mut().insert(principal);
            next.run(request).await
        }
        Err(error) => {
            if error.is_dependency_failure() {
                warn!(error = %error, "OIDC token verification dependency failed");
                authentication_service_unavailable(error_format)
            } else {
                debug!(error = %error, "OIDC bearer token rejected");
                authentication_error(error_format, "invalid_token", "invalid bearer token")
            }
        }
    }
}

fn bearer_token(headers: &axum::http::HeaderMap) -> Option<&str> {
    headers
        .get(header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .split_once(' ')
        .and_then(|(scheme, token)| {
            let token = token.trim();
            (scheme.eq_ignore_ascii_case("bearer") && !token.is_empty()).then_some(token)
        })
}

#[derive(Clone, Copy)]
enum AuthErrorFormat {
    OpenAi,
    Anthropic,
}

impl AuthErrorFormat {
    fn for_path(path: &str) -> Self {
        if matches!(path, ANTHROPIC_MESSAGES_PATH | ANTHROPIC_COUNT_TOKENS_PATH) {
            Self::Anthropic
        } else {
            Self::OpenAi
        }
    }
}

fn authentication_error(format: AuthErrorFormat, code: &'static str, message: &'static str) -> Response {
    protocol_error(
        format,
        StatusCode::UNAUTHORIZED,
        "authentication_error",
        "authentication_error",
        code,
        message,
        true,
    )
}

fn authentication_service_unavailable(format: AuthErrorFormat) -> Response {
    protocol_error(
        format,
        StatusCode::SERVICE_UNAVAILABLE,
        "server_error",
        "api_error",
        "authentication_service_unavailable",
        "authentication service temporarily unavailable",
        false,
    )
}

fn protocol_error(
    format: AuthErrorFormat,
    status: StatusCode,
    openai_error_type: &'static str,
    anthropic_error_type: &'static str,
    code: &'static str,
    message: &'static str,
    challenge: bool,
) -> Response {
    let body = match format {
        AuthErrorFormat::OpenAi => json!({
            "error": {
                "message": message,
                "type": openai_error_type,
                "param": null,
                "code": code
            }
        }),
        AuthErrorFormat::Anthropic => json!({
            "type": "error",
            "error": {
                "type": anthropic_error_type,
                "message": message
            }
        }),
    };
    let mut builder = Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/json");
    if challenge {
        builder = builder.header(header::WWW_AUTHENTICATE, "Bearer");
    }
    builder
        .body(Body::from(body.to_string()))
        .expect("valid authentication protocol error response")
}

#[derive(Clone, Copy)]
enum ProviderRequest {
    Metadata,
    Jwks,
}

impl ProviderRequest {
    fn error(self, error: reqwest::Error) -> OidcAuthError {
        match self {
            Self::Metadata => OidcAuthError::ProviderMetadataRequest(error),
            Self::Jwks => OidcAuthError::JwksRequest(error),
        }
    }
}

async fn fetch_jwks(client: &reqwest::Client, uri: Url) -> Result<CachedJwks, OidcAuthError> {
    let (keys, headers) = fetch_json::<JwkSet>(client, uri, ProviderRequest::Jwks).await?;
    compile_jwks(keys, jwks_ttl(&headers))
}

async fn fetch_json<T>(
    client: &reqwest::Client,
    uri: Url,
    request_kind: ProviderRequest,
) -> Result<(T, HeaderMap), OidcAuthError>
where
    T: serde::de::DeserializeOwned,
{
    let mut response = client
        .get(uri)
        .send()
        .await
        .map_err(|error| request_kind.error(error))?
        .error_for_status()
        .map_err(|error| request_kind.error(error))?;
    if response
        .content_length()
        .is_some_and(|length| length > MAX_PROVIDER_RESPONSE_BYTES as u64)
    {
        return Err(OidcAuthError::ProviderResponseTooLarge);
    }

    let headers = response.headers().clone();
    let mut body = Vec::with_capacity(
        response
            .content_length()
            .and_then(|length| usize::try_from(length).ok())
            .unwrap_or_default()
            .min(MAX_PROVIDER_RESPONSE_BYTES),
    );
    while let Some(chunk) = response.chunk().await.map_err(|error| request_kind.error(error))? {
        if body.len().saturating_add(chunk.len()) > MAX_PROVIDER_RESPONSE_BYTES {
            return Err(OidcAuthError::ProviderResponseTooLarge);
        }
        body.extend_from_slice(&chunk);
    }

    let value = serde_json::from_slice(&body).map_err(OidcAuthError::InvalidProviderJson)?;
    Ok((value, headers))
}

fn compile_jwks(keys: JwkSet, ttl: Duration) -> Result<CachedJwks, OidcAuthError> {
    if keys.keys.is_empty() {
        return Err(OidcAuthError::EmptyJwks);
    }
    if keys.keys.len() > MAX_JWKS_KEYS {
        return Err(OidcAuthError::TooManyJwksKeys);
    }

    let mut compiled = HashMap::with_capacity(keys.keys.len());
    for key in keys.keys {
        let Some(kid) = key.common.key_id.clone() else {
            continue;
        };
        let algorithm = match verification_algorithm(&key) {
            VerificationAlgorithm::Skip => continue,
            VerificationAlgorithm::AnyAsymmetric => None,
            VerificationAlgorithm::Exact(algorithm) => Some(algorithm),
        };
        let Ok(decoding_key) = DecodingKey::from_jwk(&key) else {
            continue;
        };
        if compiled
            .insert(
                kid.clone(),
                Arc::new(CachedKey {
                    decoding_key,
                    algorithm,
                }),
            )
            .is_some()
        {
            return Err(OidcAuthError::DuplicateKeyId(kid));
        }
    }
    if compiled.is_empty() {
        return Err(OidcAuthError::EmptyJwks);
    }

    Ok(CachedJwks {
        keys: compiled,
        expires_at: Instant::now() + ttl,
    })
}

enum VerificationAlgorithm {
    Skip,
    AnyAsymmetric,
    Exact(Algorithm),
}

fn verification_algorithm(key: &Jwk) -> VerificationAlgorithm {
    if key
        .common
        .public_key_use
        .as_ref()
        .is_some_and(|key_use| key_use != &PublicKeyUse::Signature)
        || key
            .common
            .key_operations
            .as_ref()
            .is_some_and(|operations| !operations.contains(&KeyOperations::Verify))
    {
        return VerificationAlgorithm::Skip;
    }

    match key.common.key_algorithm {
        Some(key_algorithm) => VerificationAlgorithm::Exact(match key_algorithm {
            KeyAlgorithm::ES256 => Algorithm::ES256,
            KeyAlgorithm::ES384 => Algorithm::ES384,
            KeyAlgorithm::RS256 => Algorithm::RS256,
            KeyAlgorithm::RS384 => Algorithm::RS384,
            KeyAlgorithm::RS512 => Algorithm::RS512,
            KeyAlgorithm::PS256 => Algorithm::PS256,
            KeyAlgorithm::PS384 => Algorithm::PS384,
            KeyAlgorithm::PS512 => Algorithm::PS512,
            KeyAlgorithm::EdDSA => Algorithm::EdDSA,
            KeyAlgorithm::HS256
            | KeyAlgorithm::HS384
            | KeyAlgorithm::HS512
            | KeyAlgorithm::RSA1_5
            | KeyAlgorithm::RSA_OAEP
            | KeyAlgorithm::RSA_OAEP_256
            | KeyAlgorithm::UNKNOWN_ALGORITHM => return VerificationAlgorithm::Skip,
        }),
        None => VerificationAlgorithm::AnyAsymmetric,
    }
}

fn build_validations(issuer: &str, audience: &str) -> Vec<(Algorithm, Validation)> {
    [
        Algorithm::ES256,
        Algorithm::ES384,
        Algorithm::RS256,
        Algorithm::RS384,
        Algorithm::RS512,
        Algorithm::PS256,
        Algorithm::PS384,
        Algorithm::PS512,
        Algorithm::EdDSA,
    ]
    .into_iter()
    .map(|algorithm| {
        let mut validation = Validation::new(algorithm);
        validation.leeway = JWT_CLOCK_SKEW_SECONDS;
        validation.set_audience(&[audience]);
        validation.set_issuer(&[issuer]);
        validation.set_required_spec_claims(&["exp", "iss", "aud", "sub"]);
        validation.validate_nbf = true;
        (algorithm, validation)
    })
    .collect()
}

fn jwks_ttl(headers: &HeaderMap) -> Duration {
    let mut max_age: Option<u64> = None;
    for value in headers.get_all(header::CACHE_CONTROL) {
        let Ok(value) = value.to_str() else {
            continue;
        };
        for directive in value.split(',').map(str::trim) {
            if directive.eq_ignore_ascii_case("no-cache") || directive.eq_ignore_ascii_case("no-store") {
                return Duration::ZERO;
            }
            if let Some((name, seconds)) = directive.split_once('=') {
                if name.trim().eq_ignore_ascii_case("max-age") {
                    let parsed = seconds.trim().trim_matches('"').parse::<u64>().ok();
                    max_age = match (max_age, parsed) {
                        (Some(existing), Some(parsed)) => Some(existing.min(parsed)),
                        (None, parsed) => parsed,
                        (existing, None) => existing,
                    };
                }
            }
        }
    }
    let age = headers
        .get_all(header::AGE)
        .iter()
        .filter_map(|value| value.to_str().ok()?.trim().parse::<u64>().ok())
        .max()
        .map_or(Duration::ZERO, Duration::from_secs);
    max_age.map_or_else(
        || DEFAULT_JWKS_TTL.saturating_sub(age),
        |seconds| Duration::from_secs(seconds).saturating_sub(age).min(MAX_JWKS_TTL),
    )
}

#[derive(Deserialize)]
struct ProviderMetadata {
    issuer: String,
    jwks_uri: String,
}

#[derive(Deserialize)]
struct IdentityClaims {
    iss: String,
    sub: String,
    aud: AudienceClaim,
    exp: u64,
    #[serde(default)]
    azp: Option<String>,
}

impl IdentityClaims {
    fn audience_allows(&self, expected: &str) -> bool {
        match &self.aud {
            AudienceClaim::One(audience) => audience == expected,
            AudienceClaim::Many(audiences) if audiences.len() == 1 => audiences[0] == expected,
            AudienceClaim::Many(audiences) => {
                audiences.iter().any(|audience| audience == expected) && self.azp.as_deref() == Some(expected)
            }
        }
    }
}

#[derive(Deserialize)]
#[serde(untagged)]
enum AudienceClaim {
    One(String),
    Many(Vec<String>),
}

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum OidcAuthError {
    #[error("OIDC audience must not be empty")]
    EmptyAudience,
    #[error("OIDC issuer must not include a query or fragment")]
    InvalidIssuerComponents,
    #[error("OIDC issuer must use HTTPS; HTTP is only permitted for loopback IP addresses (127.0.0.1 or ::1)")]
    InsecureIssuer,
    #[error("OIDC JWKS URI must use HTTPS; HTTP is only permitted for loopback IP addresses (127.0.0.1 or ::1)")]
    InsecureJwksUri,
    #[error("invalid OIDC issuer URL")]
    InvalidIssuer(#[source] url::ParseError),
    #[error("invalid OIDC JWKS URI")]
    InvalidJwksUri(#[source] url::ParseError),
    #[error("OIDC discovery returned an invalid issuer URL")]
    InvalidDiscoveredIssuer(#[source] url::ParseError),
    #[error("failed to build OIDC HTTP client")]
    HttpClient(#[source] reqwest::Error),
    #[error("OIDC provider metadata request failed")]
    ProviderMetadataRequest(#[source] reqwest::Error),
    #[error("OIDC JWKS request failed")]
    JwksRequest(#[source] reqwest::Error),
    #[error("OIDC provider response exceeded the size limit")]
    ProviderResponseTooLarge,
    #[error("OIDC provider returned invalid JSON")]
    InvalidProviderJson(#[source] serde_json::Error),
    #[error("OIDC discovery returned issuer {discovered}, expected {expected}")]
    IssuerMismatch { expected: String, discovered: String },
    #[error("OIDC provider returned an empty JWKS")]
    EmptyJwks,
    #[error("OIDC provider returned too many JWKs")]
    TooManyJwksKeys,
    #[error("OIDC provider returned duplicate JWK key ID {0}")]
    DuplicateKeyId(String),
    #[error("OIDC JWKS refresh is temporarily backed off after a provider failure")]
    JwksRefreshBackoff,
    #[error("OIDC JWKS refresh coordination is unavailable")]
    RefreshStateUnavailable,
    #[error("bearer token is missing a key ID")]
    MissingKeyId,
    #[error("bearer token references an unknown key ID")]
    UnknownKeyId,
    #[error("bearer token subject must not be empty")]
    EmptySubject,
    #[error("bearer token authorized party does not match the configured audience")]
    InvalidAuthorizedParty,
    #[error("bearer token uses an unsupported algorithm")]
    UnsupportedTokenAlgorithm,
    #[error("bearer token and JWK algorithms do not match")]
    AlgorithmMismatch,
    #[error("bearer token validation failed")]
    InvalidToken(#[source] jsonwebtoken::errors::Error),
}

impl OidcAuthError {
    fn is_dependency_failure(&self) -> bool {
        matches!(
            self,
            Self::JwksRequest(_)
                | Self::ProviderResponseTooLarge
                | Self::InvalidProviderJson(_)
                | Self::EmptyJwks
                | Self::TooManyJwksKeys
                | Self::DuplicateKeyId(_)
                | Self::JwksRefreshBackoff
                | Self::RefreshStateUnavailable
        )
    }
}

#[cfg(test)]
mod tests {
    use super::{
        AuthenticatedPrincipal, MAX_JWKS_KEYS, MAX_JWKS_TTL, OidcAuthError, VerificationAlgorithm, compile_jwks,
        jwks_ttl, verification_algorithm,
    };
    use axum::http::{HeaderMap, HeaderValue, header};
    use jsonwebtoken::jwk::{Jwk, JwkSet, KeyAlgorithm, KeyOperations, PublicKeyUse};
    use jsonwebtoken::{Algorithm, EncodingKey};
    use rand::rngs::OsRng;
    use rsa::RsaPrivateKey;
    use rsa::pkcs1::EncodeRsaPrivateKey;
    use std::time::Duration;

    fn test_jwk() -> Jwk {
        let private_key = RsaPrivateKey::new(&mut OsRng, 2048).expect("generate test RSA key");
        let private_key = private_key.to_pkcs1_der().expect("encode test RSA key");
        let mut jwk = Jwk::from_encoding_key(&EncodingKey::from_rsa_der(private_key.as_bytes()), Algorithm::RS256)
            .expect("test JWK");
        jwk.common.key_id = Some("test-key".to_owned());
        jwk.common.key_algorithm = Some(KeyAlgorithm::RS256);
        jwk.common.public_key_use = Some(PublicKeyUse::Signature);
        jwk
    }

    #[test]
    fn jwks_cache_lifetime_uses_provider_max_age_with_a_cap() {
        let mut headers = HeaderMap::new();
        headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("public, max-age=60"));
        assert_eq!(jwks_ttl(&headers), Duration::from_secs(60));

        headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("max-age=86400"));
        assert_eq!(jwks_ttl(&headers), MAX_JWKS_TTL);

        headers.insert(
            header::CACHE_CONTROL,
            HeaderValue::from_static("private, Max-Age=\"30\""),
        );
        assert_eq!(jwks_ttl(&headers), Duration::from_secs(30));

        headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
        assert_eq!(jwks_ttl(&headers), Duration::ZERO);

        headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("max-age=60"));
        headers.insert(header::AGE, HeaderValue::from_static("55"));
        assert_eq!(jwks_ttl(&headers), Duration::from_secs(5));

        headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("max-age=86400"));
        headers.insert(header::AGE, HeaderValue::from_static("4000"));
        assert_eq!(jwks_ttl(&headers), MAX_JWKS_TTL);

        headers.remove(header::AGE);
        headers.append(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
        assert_eq!(jwks_ttl(&headers), Duration::ZERO);
    }

    #[test]
    fn authenticated_principal_expiration_includes_clock_skew() {
        let principal = AuthenticatedPrincipal {
            issuer: "https://issuer.example".to_owned(),
            subject: "subject".to_owned(),
            expires_at: 100,
        };

        assert!(!principal.is_expired_at(160));
        assert!(principal.is_expired_at(161));
    }

    #[test]
    fn jwks_limits_and_signature_metadata_are_enforced() {
        let mut encryption_key = test_jwk();
        encryption_key.common.public_key_use = Some(PublicKeyUse::Encryption);
        assert!(matches!(
            compile_jwks(
                JwkSet {
                    keys: vec![encryption_key]
                },
                Duration::from_secs(60)
            ),
            Err(OidcAuthError::EmptyJwks)
        ));

        let mut non_verifying_key = test_jwk();
        non_verifying_key.common.key_operations = Some(vec![KeyOperations::Encrypt]);
        assert!(matches!(
            compile_jwks(
                JwkSet {
                    keys: vec![non_verifying_key]
                },
                Duration::from_secs(60)
            ),
            Err(OidcAuthError::EmptyJwks)
        ));

        let mut mismatched_algorithm = test_jwk();
        mismatched_algorithm.common.key_algorithm = Some(KeyAlgorithm::RS512);
        assert!(matches!(
            verification_algorithm(&mismatched_algorithm),
            VerificationAlgorithm::Exact(Algorithm::RS512)
        ));

        let too_many_keys = vec![test_jwk(); MAX_JWKS_KEYS + 1];
        assert!(matches!(
            compile_jwks(JwkSet { keys: too_many_keys }, Duration::from_secs(60)),
            Err(OidcAuthError::TooManyJwksKeys)
        ));
    }
}
