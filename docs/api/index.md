# API Reference

## Authentication

Inbound authentication is optional. When the gateway starts with both `OIDC_ISSUER` and `OIDC_AUDIENCE`, every
`/v1/*` HTTP route and the `/v1/responses` WebSocket upgrade require an OIDC `Authorization: Bearer <token>`.
`/health` and `/ready` remain public. Supplying only one OIDC setting is a startup error.

The gateway validates the token signature, issuer, audience, authorized party for multi-audience tokens, subject,
expiration, and not-before time. It consumes the identity token at the gateway boundary instead of forwarding it to
the inference service. WebSocket sessions reject new `response.create` messages after the validated token expires.

Missing or rejected credentials return `401 Unauthorized` with `WWW-Authenticate: Bearer`. OpenAI-compatible routes
use this envelope:

```json
{
  "error": {
    "message": "invalid bearer token",
    "type": "authentication_error",
    "param": null,
    "code": "invalid_token"
  }
}
```

`/v1/messages` and `/v1/messages/count_tokens` use the Anthropic-compatible envelope:

```json
{
  "type": "error",
  "error": {
    "type": "authentication_error",
    "message": "invalid bearer token"
  }
}
```

A JWKS refresh failure returns `503 Service Unavailable`, without `WWW-Authenticate`, so clients can distinguish an
identity-provider dependency failure from rejected credentials. See
[OIDC bearer authentication](../design/oidc-bearer-authentication.md) for configuration and key-cache behavior.
For a complete GitHub-backed deployment example, see
[GitHub authentication with Dex](../deploying/github-oidc.md).

## Responses

### `POST /v1/responses`

HTTP Responses requests use the OpenAI-compatible Responses shape. Requests
with `store=true`, `previous_response_id`, `conversation_id`, compaction input,
or `context_management` run through the executor. Other stateless `store=false`
requests are passed directly to the configured vLLM backend.

### `POST /v1/responses/compact`

Compacts direct input or a stored previous-response chain into a canonical
window of retained user messages plus one `compaction` item. See
[Responses compaction](../guides/responses-compaction.md) for request examples,
automatic threshold management, and the local plaintext limitation.

### `WS /v1/responses`

The same path accepts WebSocket upgrades for Codex-style Responses
continuations. Send one JSON text frame per turn:

```json
{
  "type": "response.create",
  "model": "test-model",
  "input": [{"type": "message", "role": "user", "content": "hi"}],
  "previous_response_id": "resp_optional",
  "store": true,
  "stream": true
}
```

The server normalizes the frame into the internal Responses request model and
uses the same response-store continuation path as HTTP. WebSocket replies are
JSON Responses stream events, including `response.created`,
`response.output_item.added`, `response.output_text.delta`, and
`response.completed`.

Invalid requests are returned as JSON WebSocket error events:

```json
{
  "type": "error",
  "status": 404,
  "error": {
    "message": "human-readable error details",
    "type": "not_found",
    "code": "not_found"
  }
}
```
