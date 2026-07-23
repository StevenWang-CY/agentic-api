//! Database schema management and migrations.

use std::env;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use sqlx::Connection;
use tracing::{debug, info};

use super::pool::DbPool;
use crate::config::DEFAULT_POSTGRES_MIGRATION_TIMEOUT_SECONDS;

type DbResult<T> = Result<T, sqlx::Error>;

const POSTGRES_SCHEMA_ADVISORY_LOCK: i64 = 7_194_963_546_799_751;
const POSTGRES_INTEGER_WIDENING_SQL: &str = "
    ALTER TABLE conversations
        ALTER COLUMN created_at TYPE BIGINT USING created_at::BIGINT;
    ALTER TABLE items
        ALTER COLUMN created_at TYPE BIGINT USING created_at::BIGINT,
        ALTER COLUMN seq TYPE BIGINT USING seq::BIGINT;
    ALTER TABLE responses
        ALTER COLUMN created_at TYPE BIGINT USING created_at::BIGINT;
";

async fn configure_postgres_migration_timeout(
    connection: &mut sqlx::AnyConnection,
    migration_timeout: Duration,
) -> DbResult<()> {
    let timeout_ms = format!("{}ms", migration_timeout.as_millis());
    sqlx::query("SELECT set_config('lock_timeout', $1, false)")
        .bind(&timeout_ms)
        .execute(&mut *connection)
        .await?;
    sqlx::query("SELECT set_config('statement_timeout', $1, false)")
        .bind(timeout_ms)
        .execute(connection)
        .await?;
    Ok(())
}

async fn widen_postgres_integer_columns(connection: &mut sqlx::AnyConnection) -> DbResult<()> {
    let mut transaction = connection.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(POSTGRES_SCHEMA_ADVISORY_LOCK)
        .execute(&mut *transaction)
        .await?;
    let narrow_column_count = postgres_narrow_integer_column_count(&mut *transaction).await?;
    if narrow_column_count > 0 {
        sqlx::raw_sql(POSTGRES_INTEGER_WIDENING_SQL)
            .execute(&mut *transaction)
            .await?;
    }
    transaction.commit().await
}

async fn postgres_narrow_integer_column_count<'e, E>(executor: E) -> DbResult<i64>
where
    E: sqlx::Executor<'e, Database = sqlx::Any>,
{
    sqlx::query_scalar(
        "SELECT COUNT(*) FROM information_schema.columns \
         WHERE table_schema = current_schema() AND data_type <> 'bigint' \
         AND ((table_name = 'conversations' AND column_name = 'created_at') \
           OR (table_name = 'items' AND column_name IN ('created_at', 'seq')) \
           OR (table_name = 'responses' AND column_name = 'created_at'))",
    )
    .fetch_one(executor)
    .await
}

fn validate_supervisor_integer_width(narrow_column_count: i64) -> DbResult<()> {
    if narrow_column_count == 0 {
        return Ok(());
    }
    Err(sqlx::Error::Configuration(
        "supervisor-managed PostgreSQL schema requires BIGINT compatibility upgrade; \
         apply the documented ALTER TABLE statements before setting AGENTIC_API_SCHEMA_READY"
            .into(),
    ))
}

async fn verify_supervisor_managed_postgres_schema(
    pool: &DbPool,
    postgres_migration_timeout: Duration,
) -> DbResult<()> {
    let mut connection = pool.acquire().await?;
    if connection.backend_name() != "PostgreSQL" {
        return Ok(());
    }

    if let Err(error) = configure_postgres_migration_timeout(&mut connection, postgres_migration_timeout).await {
        let _ = connection.close().await;
        return Err(error);
    }
    let compatibility_result = match postgres_narrow_integer_column_count(&mut *connection).await {
        Ok(narrow_column_count) => validate_supervisor_integer_width(narrow_column_count),
        Err(error) => Err(error),
    };
    let close_result = connection.close().await;
    compatibility_result?;
    close_result
}

async fn apply_postgres_compatibility(
    connection: &mut sqlx::AnyConnection,
    postgres_migration_timeout: Duration,
) -> DbResult<()> {
    configure_postgres_migration_timeout(connection, postgres_migration_timeout).await?;
    widen_postgres_integer_columns(connection).await
}

async fn run_embedded_migrations(pool: &DbPool, postgres_migration_timeout: Duration) -> DbResult<()> {
    let mut connection = pool.acquire().await?;
    let is_postgres = connection.backend_name() == "PostgreSQL";
    if is_postgres {
        if let Err(error) = configure_postgres_migration_timeout(&mut connection, postgres_migration_timeout).await {
            let _ = connection.close().await;
            return Err(error);
        }
    }

    let migration_result = sqlx::migrate!("./migrations")
        .run(&mut *connection)
        .await
        .map_err(|error| sqlx::Error::Configuration(error.to_string().into()));
    let postgres_result = if migration_result.is_ok() && is_postgres {
        apply_postgres_compatibility(&mut connection, postgres_migration_timeout).await
    } else {
        Ok(())
    };
    let close_result = if is_postgres {
        connection.close().await
    } else {
        drop(connection);
        Ok(())
    };

    migration_result?;
    postgres_result?;
    close_result
}

fn is_marked_ready() -> bool {
    matches!(
        env::var("AGENTIC_API_SCHEMA_READY").as_deref(),
        Ok("1" | "true" | "t" | "yes" | "y" | "on")
    )
}

/// Database pool with per-pool schema readiness tracking.
///
/// Wraps `DbPool` and adds an `AtomicBool` flag to track schema initialization
/// per pool instance. This eliminates the issue of global state interfering
/// when multiple pools point to different databases.
pub struct PoolWithSchema {
    pool: Arc<DbPool>,
    schema_ready: AtomicBool,
    postgres_migration_timeout: Duration,
}

impl PoolWithSchema {
    /// Creates a new pool with schema tracking.
    #[must_use]
    pub fn new(pool: Arc<DbPool>) -> Self {
        Self::with_postgres_migration_timeout(pool, Duration::from_secs(DEFAULT_POSTGRES_MIGRATION_TIMEOUT_SECONDS))
    }

    /// Creates a new pool with schema tracking and a `PostgreSQL` migration timeout.
    #[must_use]
    pub fn with_postgres_migration_timeout(pool: Arc<DbPool>, postgres_migration_timeout: Duration) -> Self {
        Self {
            pool,
            schema_ready: AtomicBool::new(false),
            postgres_migration_timeout,
        }
    }

    /// Returns a reference to the underlying database pool.
    pub fn pool(&self) -> &Arc<DbPool> {
        &self.pool
    }

    /// Ensures database schema is ready by running pending migrations.
    ///
    /// Checks if migrations have already been applied via one of:
    /// 1. Per-pool flag (`schema_ready`)
    /// 2. `AGENTIC_API_SCHEMA_READY` environment variable
    ///
    /// If none of the above, runs all pending migrations from the `migrations/` directory.
    /// Supervisor-managed `PostgreSQL` schemas still verify required compatibility upgrades.
    ///
    /// # Errors
    ///
    /// Returns a [`sqlx::Error`] if migrations fail.
    pub async fn ensure_schema_ready(&self) -> DbResult<()> {
        self.ensure_schema_ready_with_marker(is_marked_ready()).await
    }

    async fn ensure_schema_ready_with_marker(&self, supervisor_managed: bool) -> DbResult<()> {
        if self.schema_ready.load(Ordering::SeqCst) {
            return Ok(());
        }

        if supervisor_managed {
            debug!("[schema] Migrations skipped — marked ready by supervisor.");
            verify_supervisor_managed_postgres_schema(self.pool.as_ref(), self.postgres_migration_timeout).await?;
            self.schema_ready.store(true, Ordering::SeqCst);
            return Ok(());
        }

        debug!("[schema] Running migrations...");
        run_embedded_migrations(self.pool.as_ref(), self.postgres_migration_timeout).await?;
        info!("[schema] DB schema ready.");
        self.schema_ready.store(true, Ordering::SeqCst);
        Ok(())
    }
}

/// Manages database schema initialization and migrations (deprecated).
///
/// This struct is kept for backward compatibility. New code should use
/// [`PoolWithSchema::ensure_schema_ready`] instead.
pub struct SchemaManager<'a> {
    pool: &'a DbPool,
}

impl<'a> SchemaManager<'a> {
    /// Creates a new schema manager for the given database pool (deprecated).
    #[must_use]
    pub fn new(pool: &'a DbPool) -> Self {
        Self { pool }
    }

    /// Runs migrations without checking any flag.
    ///
    /// # Errors
    ///
    /// Returns a [`sqlx::Error`] if migrations fail.
    pub async fn run_migrations(&self) -> DbResult<()> {
        debug!("[schema] Running migrations...");
        run_embedded_migrations(
            self.pool,
            Duration::from_secs(DEFAULT_POSTGRES_MIGRATION_TIMEOUT_SECONDS),
        )
        .await?;
        info!("[schema] DB schema ready.");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_env_var_pattern() {
        let test_values = vec![
            ("1", true),
            ("true", true),
            ("t", true),
            ("yes", true),
            ("y", true),
            ("on", true),
            ("0", false),
            ("false", false),
            ("f", false),
            ("no", false),
            ("n", false),
            ("off", false),
            ("", false),
        ];

        for (val, expected) in test_values {
            let matches = matches!(
                Ok::<&str, String>(val).as_deref(),
                Ok("1" | "true" | "t" | "yes" | "y" | "on")
            );
            assert_eq!(matches, expected, "Mismatch for value '{val}'");
        }
    }

    #[tokio::test]
    async fn test_pool_with_schema_ready() {
        let pool = crate::storage::pool::create_pool(Some("sqlite://?mode=memory"))
            .await
            .expect("failed to create pool");

        let pool_with_schema = PoolWithSchema::new(pool);

        // First call should run migrations
        let result = pool_with_schema.ensure_schema_ready().await;
        assert!(result.is_ok(), "ensure_schema_ready failed: {result:?}");

        // Flag should now be set
        assert!(pool_with_schema.schema_ready.load(Ordering::SeqCst));

        // Second call should return immediately without doing work
        let result = pool_with_schema.ensure_schema_ready().await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_multiple_pools_independent() {
        // Create two in-memory pools
        let pool1 = crate::storage::pool::create_pool(Some("sqlite://?mode=memory"))
            .await
            .expect("failed to create pool1");

        let pool2 = crate::storage::pool::create_pool(Some("sqlite://?mode=memory"))
            .await
            .expect("failed to create pool2");

        let pwc1 = PoolWithSchema::new(pool1);
        let pwc2 = PoolWithSchema::new(pool2);

        // Initialize both
        pwc1.ensure_schema_ready().await.expect("pool1 failed");
        pwc2.ensure_schema_ready().await.expect("pool2 failed");

        // Both should be marked ready independently
        assert!(pwc1.schema_ready.load(Ordering::SeqCst));
        assert!(pwc2.schema_ready.load(Ordering::SeqCst));

        // Subsequent calls should succeed without re-running migrations
        pwc1.ensure_schema_ready().await.expect("pool1 repeat failed");
        pwc2.ensure_schema_ready().await.expect("pool2 repeat failed");
    }

    #[tokio::test]
    #[ignore = "requires TEST_POSTGRES_URL pointing to an isolated PostgreSQL database"]
    #[allow(
        clippy::too_many_lines,
        reason = "keeps the complete migration lifecycle in one integration test"
    )]
    async fn postgres_integer_widening_upgrade_preserves_existing_state() {
        let database_url = std::env::var("TEST_POSTGRES_URL").expect("TEST_POSTGRES_URL must be set");
        let pool = crate::storage::pool::create_pool(Some(&database_url))
            .await
            .expect("create PostgreSQL upgrade-test pool");
        let schema = format!("upgrade_{}", uuid::Uuid::now_v7().simple());
        let mut connection = pool.acquire().await.expect("acquire PostgreSQL upgrade connection");
        sqlx::query(&format!("CREATE SCHEMA {schema}"))
            .execute(&mut *connection)
            .await
            .expect("create isolated upgrade schema");
        sqlx::query(&format!("SET search_path TO {schema}"))
            .execute(&mut *connection)
            .await
            .expect("select isolated upgrade schema");
        for migration in [
            include_str!("../../migrations/0001_initial.sql"),
            include_str!("../../migrations/0002_add_placeholders.sql"),
            include_str!("../../migrations/0003_index_conversation_sequence.sql"),
        ] {
            sqlx::raw_sql(migration)
                .execute(&mut *connection)
                .await
                .expect("apply portable migration");
        }
        sqlx::query("INSERT INTO conversations (id, created_at, metadata) VALUES ($1, $2, $3)")
            .bind("conv_upgrade")
            .bind(1_704_067_200_i64)
            .bind("{\"source\":\"upgrade\"}")
            .execute(&mut *connection)
            .await
            .expect("seed conversation");
        sqlx::query("INSERT INTO items (id, data, created_at, conversation_id, seq) VALUES ($1, $2, $3, $4, $5)")
            .bind("item_upgrade")
            .bind("{}")
            .bind(1_704_067_200_i64)
            .bind("conv_upgrade")
            .bind(0_i64)
            .execute(&mut *connection)
            .await
            .expect("seed item");
        sqlx::query(
            "INSERT INTO responses \
             (id, conversation_id, history_item_ids, metadata, created_at) VALUES ($1, $2, $3, $4, $5)",
        )
        .bind("resp_upgrade")
        .bind("conv_upgrade")
        .bind("[\"item_upgrade\"]")
        .bind("{\"source\":\"upgrade\"}")
        .bind(1_704_067_200_i64)
        .execute(&mut *connection)
        .await
        .expect("seed response");

        let narrow_column_count = postgres_narrow_integer_column_count(&mut *connection)
            .await
            .expect("inspect pre-upgrade PostgreSQL columns");
        assert_eq!(narrow_column_count, 4);
        assert!(validate_supervisor_integer_width(narrow_column_count).is_err());

        let supervisor_schema_name = schema.clone();
        let supervisor_pool = sqlx::any::AnyPoolOptions::new()
            .max_connections(1)
            .after_connect(move |connection, _metadata| {
                let supervisor_schema_name = supervisor_schema_name.clone();
                Box::pin(async move {
                    sqlx::query("SELECT set_config('search_path', $1, false)")
                        .bind(supervisor_schema_name)
                        .execute(connection)
                        .await?;
                    Ok(())
                })
            })
            .connect(&database_url)
            .await
            .expect("create supervisor-managed PostgreSQL pool");
        let supervisor_schema =
            PoolWithSchema::with_postgres_migration_timeout(Arc::new(supervisor_pool), Duration::from_secs(5));
        let supervisor_error = supervisor_schema
            .ensure_schema_ready_with_marker(true)
            .await
            .expect_err("narrow supervisor-managed schema should fail compatibility check");
        assert!(supervisor_error.to_string().contains("BIGINT compatibility upgrade"));
        assert!(!supervisor_schema.schema_ready.load(Ordering::SeqCst));

        apply_postgres_compatibility(&mut connection, Duration::from_secs(5))
            .await
            .expect("widen PostgreSQL integer columns");
        supervisor_schema
            .ensure_schema_ready_with_marker(true)
            .await
            .expect("widened supervisor-managed schema should pass compatibility check");
        assert!(supervisor_schema.schema_ready.load(Ordering::SeqCst));

        let future_timestamp = i64::from(i32::MAX) + 1;
        sqlx::query("UPDATE conversations SET created_at = $1 WHERE id = $2")
            .bind(future_timestamp)
            .bind("conv_upgrade")
            .execute(&mut *connection)
            .await
            .expect("write timestamp beyond PostgreSQL INT4 range");
        let linked_state: (i64, String, String) = sqlx::query_as(
            "SELECT conversations.created_at, items.id, responses.id \
             FROM conversations \
             JOIN items ON items.conversation_id = conversations.id \
             JOIN responses ON responses.conversation_id = conversations.id \
             WHERE conversations.id = $1",
        )
        .bind("conv_upgrade")
        .fetch_one(&mut *connection)
        .await
        .expect("load migrated linked state");
        assert_eq!(
            linked_state,
            (future_timestamp, "item_upgrade".to_owned(), "resp_upgrade".to_owned())
        );
        let bigint_columns: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM information_schema.columns \
             WHERE table_schema = $1 AND data_type = 'bigint' \
             AND ((table_name = 'conversations' AND column_name = 'created_at') \
               OR (table_name = 'items' AND column_name IN ('created_at', 'seq')) \
               OR (table_name = 'responses' AND column_name = 'created_at'))",
        )
        .bind(&schema)
        .fetch_one(&mut *connection)
        .await
        .expect("inspect widened PostgreSQL columns");
        assert_eq!(bigint_columns, 4);
        assert!(validate_supervisor_integer_width(4 - bigint_columns).is_ok());
        let foreign_key_error =
            sqlx::query("INSERT INTO items (id, data, created_at, conversation_id) VALUES ($1, $2, $3, $4)")
                .bind("item_invalid")
                .bind("{}")
                .bind(future_timestamp)
                .bind("conv_missing")
                .execute(&mut *connection)
                .await;
        assert!(foreign_key_error.is_err());

        sqlx::query("SET search_path TO public")
            .execute(&mut *connection)
            .await
            .expect("restore public schema");
        supervisor_schema.pool.close().await;
        sqlx::query(&format!("DROP SCHEMA {schema} CASCADE"))
            .execute(&mut *connection)
            .await
            .expect("drop isolated upgrade schema");
        drop(connection);
        pool.close().await;
    }
}
