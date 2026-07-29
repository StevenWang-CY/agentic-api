# PR #142 Review Follow-ups Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Fix the actionable backend-classification, migration-error, and import review comments on PR #142 while
retaining the existing persistence lock and documenting why a unique index alone cannot provide causal inference.

**Architecture:** Add a small URL-parsed `DatabaseBackend` enum at the core storage boundary and pass it through pool
configuration instead of branching on raw prefixes. Reuse that classification in the server and startup diagnostics,
and centralize case-insensitive database URL redaction beside it. Convert `MigrateError` directly into
`sqlx::Error::Migrate` so its source chain remains inspectable.

**Tech Stack:** Rust 2024, SQLx 0.8 `Any`, `url` 2, Tokio tests, Cargo, pre-commit.

## Global Constraints

- Keep the change within issue #102's PostgreSQL persistence slice and PR #142.
- Preserve SQLite and PostgreSQL behavior; do not add MySQL support.
- Preserve error sources and avoid `unwrap` or `expect` in production paths.
- Do not remove the persistence row lock or implement conversation-wide leases in this follow-up.
- Keep lines within 120 characters and pass Clippy with warnings denied.

---

### Task 1: Parse and reuse database backend identity

**Files:**
- Create: `crates/agentic-server-core/src/storage/backend.rs`
- Modify: `crates/agentic-server-core/src/storage/mod.rs`
- Modify: `crates/agentic-server-core/src/lib.rs`
- Modify: `crates/agentic-server-core/src/storage/pool.rs`
- Modify: `crates/agentic-server-core/src/executor/request.rs`
- Modify: `crates/agentic-server/src/main.rs`
- Test: unit tests in the modified Rust modules

**Interfaces:**
- Produces: `DatabaseBackend::from_url(&str) -> Result<DatabaseBackend, url::ParseError>`
- Produces: `DatabaseBackend::display_name(self) -> &'static str`
- Produces: `redact_database_urls(&str) -> String`
- Consumes: `DatabaseBackend` in pool option, SQLite WAL, server configuration, and startup diagnostic branches

- [ ] **Step 1: Write failing backend regression tests**

Add tests that expect:

```rust
assert_eq!(
    DatabaseBackend::from_url("POSTGRESQL://user:pass@localhost/db").expect("valid URL"),
    DatabaseBackend::Postgres
);
assert_eq!(
    DatabaseBackend::from_url("SQLITE://test.db").expect("valid URL"),
    DatabaseBackend::Sqlite
);
assert_eq!(
    redact_database_urls("failed POSTGRESQL://user:secret@host/db"),
    "failed postgresql://[redacted]"
);
```

Update the pool tests so uppercase `SQLITE://` receives `mode=rwc` and uppercase PostgreSQL selects the explicit
PostgreSQL pool settings.

- [ ] **Step 2: Run the focused tests and verify RED**

Run:

```bash
cargo test -p agentic-server-core storage::backend
cargo test -p agentic-server-core storage::pool
```

Expected: compilation or assertion failure because `DatabaseBackend` and case-insensitive behavior do not exist.

- [ ] **Step 3: Implement the backend boundary**

Create:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DatabaseBackend {
    Postgres,
    Sqlite,
    Other,
}

impl DatabaseBackend {
    pub fn from_url(database_url: &str) -> Result<Self, url::ParseError> {
        let url = url::Url::parse(database_url)?;
        Ok(match url.scheme() {
            "postgres" | "postgresql" => Self::Postgres,
            "sqlite" => Self::Sqlite,
            _ => Self::Other,
        })
    }

    pub const fn display_name(self) -> &'static str {
        match self {
            Self::Postgres => "PostgreSQL",
            Self::Sqlite => "SQLite",
            Self::Other => "configured",
        }
    }
}
```

Move database URL redaction into the same module and make supported schemes case-insensitive. Export the enum and
redactor through `storage::mod` and the crate root.

Parse once in `create_pool_with_configs`, then pass `DatabaseBackend` to `prepare_db_url`, `pool_options`, and
`sqlite_should_enable_wal`. Replace the duplicated server and executor prefix branches with `DatabaseBackend`.

- [ ] **Step 4: Run the focused tests and verify GREEN**

Run:

```bash
cargo test -p agentic-server-core storage::backend
cargo test -p agentic-server-core storage::pool
cargo test -p agentic-server-core executor::request
cargo test -p agentic-server --bin agentic-server
```

Expected: all focused tests pass.

### Task 2: Preserve typed migration errors

**Files:**
- Modify: `crates/agentic-server-core/src/storage/schema.rs`
- Test: `crates/agentic-server-core/src/storage/schema.rs`

**Interfaces:**
- Produces: `migration_error(sqlx::migrate::MigrateError) -> sqlx::Error`
- Consumes: SQLx migrator failures in `run_embedded_migrations`

- [ ] **Step 1: Write a failing typed-error regression test**

Add:

```rust
#[test]
fn migration_errors_preserve_their_type_and_source() {
    use std::error::Error as _;

    let error = migration_error(sqlx::migrate::MigrateError::VersionMissing(7));
    assert!(matches!(error, sqlx::Error::Migrate(_)));
    assert!(error.source().is_some());
}
```

- [ ] **Step 2: Run the focused test and verify RED**

Run:

```bash
cargo test -p agentic-server-core migration_errors_preserve_their_type_and_source
```

Expected: compilation failure because `migration_error` does not exist.

- [ ] **Step 3: Implement typed conversion**

Add:

```rust
fn migration_error(error: sqlx::migrate::MigrateError) -> sqlx::Error {
    error.into()
}
```

Use `.map_err(migration_error)` in `run_embedded_migrations`.

- [ ] **Step 4: Run the focused test and verify GREEN**

Run:

```bash
cargo test -p agentic-server-core migration_errors_preserve_their_type_and_source
```

Expected: the regression test passes.

### Task 3: Cleanup, verification, and PR review responses

**Files:**
- Modify: `crates/agentic-server-core/src/executor/persist.rs`
- Modify only if review feedback requires it: files already listed above

**Interfaces:**
- Consumes: `ExecutorError` through a direct import
- Produces: verified PR head and review-thread responses

- [ ] **Step 1: Apply the import cleanup**

Import `ExecutorError` with `ExecutorResult` and construct `ExecutorError::Persistence` without its fully qualified
path.

- [ ] **Step 2: Format and run repository verification**

Run in order:

```bash
cargo fmt -- --check
cargo test
cargo clippy --all-targets -- -D warnings
uvx pre-commit run --all-files
git diff --check
```

Expected: every command exits zero.

- [ ] **Step 3: Run required reviews**

Run the read-only Claude review using the repository's `claude-review` skill, then the gstack pre-landing review.
Implement all actionable findings and repeat relevant tests/reviews until clean.

- [ ] **Step 4: Commit and push**

Create a signed-off conventional commit:

```bash
git add crates/agentic-server-core crates/agentic-server
git commit -s -m "fix: address PostgreSQL follow-up review"
git push origin codex/issue-102-postgres-pool-config
```

- [ ] **Step 5: Reply to and resolve review threads**

Reply with the implemented change on the import, backend, and migration-error threads. Resolve the informational `$1`
thread. On the concurrency thread, explain the stale-history race that survives a unique-index-only change and state
that the row lock remains scoped to storage sequence allocation. Do not claim full causal turn semantics.
