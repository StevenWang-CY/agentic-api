# PR #142 Review Follow-ups Design

## Goal

Address the actionable review feedback on PR #142 without expanding the PostgreSQL persistence slice into a
conversation-wide distributed locking feature.

## Scope

This follow-up will:

- replace case-sensitive database URL prefix checks with a shared `DatabaseBackend` classification parsed from
  `url::Url::scheme()`;
- use that classification for PostgreSQL/SQLite configuration, pool setup, startup diagnostics, and URL redaction;
- preserve SQLx migration error types and source chains;
- import `ExecutorError` directly in the persistence module; and
- reply to the concurrency review with the verified limitation of the proposed unique-index-only remedy.

It will not remove the per-conversation persistence lock, add a database lease, or claim that storage serialization
provides causal inference semantics.

## Considered Approaches

### Shared parsed backend classification

Add a small `DatabaseBackend` enum in the core storage boundary and derive it from `url::Url`. Callers reuse the enum
instead of independently comparing raw string prefixes. This is the selected approach because it fixes uppercase URI
schemes while keeping the existing public configuration shape.

### Case-insensitive prefix checks

Each existing caller could switch to case-insensitive comparisons. This would fix the reported examples but preserve
the duplicated, stringly typed dispatch that caused the bug.

### Replace the configured URL with a new validated URL type

The entire configuration API could store a parsed URL plus backend. This would provide stronger type-level validation
but would broaden a focused review fix into a public configuration refactor.

## Components and Data Flow

`DatabaseBackend` will distinguish PostgreSQL, SQLite, and other configured schemes. Parsing an invalid URL will remain
a configuration error. The server will use the shared classification when selecting environment-derived database
settings. Core pool creation will classify once and pass the enum through URL preparation, pool option selection, and
SQLite WAL setup. Startup diagnostics will use the same enum for backend names.

Redaction must also recognize case variants embedded in error text. It will preserve only the normalized scheme and a
`[redacted]` marker; credentials, hosts, paths, and query parameters remain hidden.

## Error Handling

Embedded migration failures will convert through `sqlx::Error::from`, producing `sqlx::Error::Migrate` and retaining
the `MigrateError` source. Existing startup categorization can then reliably report a migration error.

## Conversation Concurrency

The existing row lock serializes sequence allocation during persistence, but it does not span conversation
rehydration and inference. A unique `(conversation_id, seq)` index without an expected history version cannot detect
all stale inference:

1. requests A and B both rehydrate history `H`;
2. A completes inference and persistence;
3. B completes inference later;
4. B computes its sequence range from A's newly committed rows and persists output generated from stale `H`.

Full causal semantics require either a lease covering rehydration through persistence or optimistic concurrency using
a version captured during rehydration. That behavior is outside this focused follow-up. The review response will make
the distinction explicit and retain the row lock.

## Testing

Focused unit tests will first demonstrate that uppercase PostgreSQL and SQLite schemes are misclassified and not
redacted by the existing code. Further tests will verify the shared backend classification, correct pool/config
selection, case-insensitive redaction, and preservation of `sqlx::Error::Migrate`.

The completed change will run formatting, targeted tests, the full Rust test suite, Clippy with warnings denied,
pre-commit, the read-only Claude review, and the gstack pre-landing review.
