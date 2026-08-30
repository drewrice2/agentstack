use anyhow::{Context, bail};
use sqlx::{
    Executor, PgPool, Row,
    postgres::{PgConnectOptions, PgPoolOptions},
};
use std::collections::HashMap;

const POSTGRES_SCHEMA: &str = include_str!("../schema/postgres.sql");
pub const SCHEMA_MIGRATIONS_LOCK_ID: i64 = 3_196_597_405_180_601_817;

pub type DbPool = PgPool;
pub type DbRow = sqlx::postgres::PgRow;
pub type DbTransaction<'a> = sqlx::Transaction<'a, sqlx::Postgres>;

async fn connect_with_max(database_url: &str, max_connections: u32) -> anyhow::Result<DbPool> {
    let options: PgConnectOptions = database_url.parse()?;
    let pool = PgPoolOptions::new()
        .max_connections(max_connections)
        .connect_with(options)
        .await
        .context("failed to connect to Postgres")?;

    Ok(pool)
}

pub async fn connect_unverified(database_url: &str) -> anyhow::Result<DbPool> {
    connect_with_max(database_url, 10).await
}

pub async fn connect_and_migrate(database_url: &str) -> anyhow::Result<DbPool> {
    connect_and_migrate_with(database_url, 10).await
}

/// `connect_and_migrate` with an explicit pool size. Test fixtures create one
/// database (and pool) per test; the default 10-connection pool multiplied by
/// the harness's parallelism starves Postgres's default 100-connection limit,
/// so they pass a small cap instead.
pub async fn connect_and_migrate_with(
    database_url: &str,
    max_connections: u32,
) -> anyhow::Result<DbPool> {
    let pool = connect_with_max(database_url, max_connections).await?;

    install_schema_if_empty(&pool).await?;
    // Ordered for error precedence, not redundancy: a partial schema must
    // report missing tables, a legacy schema must surface the team_admin
    // migration guidance, and only then does the full contract run (which
    // re-checks tables as part of its sweep).
    verify_required_tables(&pool).await?;
    verify_team_role_constraint(&pool).await?;
    verify_schema_contract(&pool).await?;

    Ok(pool)
}

pub async fn connect_read_only(database_url: &str) -> anyhow::Result<DbPool> {
    let options: PgConnectOptions = database_url.parse()?;
    let pool = PgPoolOptions::new()
        .max_connections(1)
        .after_connect(|conn, _meta| {
            Box::pin(async move {
                conn.execute("SET default_transaction_read_only = on")
                    .await?;
                Ok(())
            })
        })
        .connect_with(options)
        .await
        .context("failed to connect to Postgres")?;

    Ok(pool)
}

async fn install_schema_if_empty(pool: &DbPool) -> anyhow::Result<()> {
    let mut tx = pool
        .begin()
        .await
        .context("failed to start Postgres schema install transaction")?;

    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(SCHEMA_MIGRATIONS_LOCK_ID)
        .execute(&mut *tx)
        .await
        .context("failed to acquire schema migration lock")?;

    let public_base_table_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*)
         FROM information_schema.tables
         WHERE table_schema = 'public'
           AND table_type = 'BASE TABLE'",
    )
    .fetch_one(&mut *tx)
    .await
    .context("failed to inspect Postgres schema")?;

    if public_base_table_count == 0 {
        sqlx::raw_sql(POSTGRES_SCHEMA)
            .execute(&mut *tx)
            .await
            .context("failed to install Postgres schema")?;
    }

    tx.commit()
        .await
        .context("failed to commit Postgres schema install transaction")?;

    Ok(())
}

pub async fn verify_schema_contract(pool: &DbPool) -> anyhow::Result<()> {
    verify_required_tables(pool).await?;
    verify_required_columns(pool).await?;
    verify_required_constraints(pool).await?;
    verify_required_indexes(pool).await?;
    verify_required_functions(pool).await?;
    verify_required_triggers(pool).await?;

    Ok(())
}

async fn verify_required_tables(pool: &DbPool) -> anyhow::Result<()> {
    let required = [
        "users",
        "schema_migrations",
        "orgs",
        "principals",
        "human_profiles",
        "external_identities",
        "org_members",
        "invites",
        "tokens",
        "ui_sessions",
        "browser_sessions",
        "oauth_login_states",
        "machine_principals",
        "teams",
        "team_memberships",
        "archives",
        "skills",
        "skill_versions",
        "skill_version_platform_tags",
        "stacks",
        "stack_items",
        "audit_log",
    ];
    let existing: Vec<String> = sqlx::query_scalar(
        "SELECT table_name
         FROM information_schema.tables
         WHERE table_schema = 'public'
           AND table_type = 'BASE TABLE'
           AND table_name = ANY($1)",
    )
    .bind(required)
    .fetch_all(pool)
    .await
    .context("failed to inspect required schema tables")?;
    let missing: Vec<&str> = required
        .iter()
        .copied()
        .filter(|name| !existing.iter().any(|found| found == name))
        .collect();

    bail_if_missing("table", &missing)
}

#[derive(Clone, Copy)]
struct ExpectedColumn {
    table: &'static str,
    name: &'static str,
    data_type: &'static str,
    nullable: bool,
    default: Option<&'static str>,
}

async fn verify_required_columns(pool: &DbPool) -> anyhow::Result<()> {
    let rows = sqlx::query(
        "SELECT c.relname AS table_name,
                a.attname AS column_name,
                pg_catalog.format_type(a.atttypid, a.atttypmod) AS data_type,
                NOT a.attnotnull AS nullable,
                pg_get_expr(d.adbin, d.adrelid) AS column_default
         FROM pg_attribute a
         JOIN pg_class c ON c.oid = a.attrelid
         JOIN pg_namespace n ON n.oid = c.relnamespace
         LEFT JOIN pg_attrdef d ON d.adrelid = a.attrelid AND d.adnum = a.attnum
         WHERE n.nspname = 'public'
           AND c.relkind = 'r'
           AND a.attnum > 0
           AND NOT a.attisdropped",
    )
    .fetch_all(pool)
    .await
    .context("failed to inspect required schema columns")?;
    let existing: HashMap<String, (String, bool, Option<String>)> = rows
        .into_iter()
        .map(|row| {
            let table: String = row.get("table_name");
            let column: String = row.get("column_name");
            (
                format!("{table}.{column}"),
                (
                    row.get("data_type"),
                    row.get("nullable"),
                    row.get("column_default"),
                ),
            )
        })
        .collect();

    let mut missing = Vec::new();
    let mut drifted = Vec::new();
    for expected in expected_columns() {
        let key = format!("{}.{}", expected.table, expected.name);
        let Some((data_type, nullable, default)) = existing.get(&key) else {
            missing.push(key);
            continue;
        };
        let expected_default = expected.default.map(str::to_string);
        if data_type != expected.data_type
            || *nullable != expected.nullable
            || default.as_ref() != expected_default.as_ref()
        {
            drifted.push(format!(
                "{key} expected type={}, nullable={}, default={:?}; found type={data_type}, nullable={nullable}, default={default:?}",
                expected.data_type, expected.nullable, expected.default
            ));
        }
    }

    bail_if_missing("column", &missing)?;
    bail_if_drifted("column", &drifted)
}

#[derive(Clone, Copy)]
struct ExpectedCatalogDef {
    key: &'static str,
    kind: Option<&'static str>,
    definition: &'static str,
}

async fn verify_required_constraints(pool: &DbPool) -> anyhow::Result<()> {
    let rows = sqlx::query(
        "SELECT c.relname || '.' || con.conname AS key,
                con.contype::text AS kind,
                pg_get_constraintdef(con.oid) AS definition
         FROM pg_constraint con
         JOIN pg_class c ON c.oid = con.conrelid
         JOIN pg_namespace n ON n.oid = c.relnamespace
         WHERE n.nspname = 'public'",
    )
    .fetch_all(pool)
    .await
    .context("failed to inspect required schema constraints")?;
    let existing: HashMap<String, (String, String)> = rows
        .into_iter()
        .map(|row| {
            (
                row.get("key"),
                (
                    row.get("kind"),
                    normalize_catalog_sql(&row.get::<String, _>("definition")),
                ),
            )
        })
        .collect();

    let mut missing = Vec::new();
    let mut drifted = Vec::new();
    for expected in expected_constraints() {
        let Some((kind, definition)) = existing.get(expected.key) else {
            missing.push(expected.key);
            continue;
        };
        let expected_definition = normalize_catalog_sql(expected.definition);
        if Some(kind.as_str()) != expected.kind || definition != &expected_definition {
            drifted.push(format!(
                "{} expected type={:?} def={}; found type={kind} def={definition}",
                expected.key, expected.kind, expected_definition
            ));
        }
    }

    bail_if_missing("constraint", &missing)?;
    bail_if_drifted("constraint", &drifted)
}

async fn verify_required_indexes(pool: &DbPool) -> anyhow::Result<()> {
    let rows = sqlx::query(
        "SELECT indexname, indexdef
         FROM pg_indexes
         WHERE schemaname = 'public'",
    )
    .fetch_all(pool)
    .await
    .context("failed to inspect required schema indexes")?;
    let existing: HashMap<String, String> = rows
        .into_iter()
        .map(|row| {
            (
                row.get("indexname"),
                normalize_catalog_sql(&row.get::<String, _>("indexdef")),
            )
        })
        .collect();

    let mut missing = Vec::new();
    let mut drifted = Vec::new();
    for expected in expected_indexes() {
        let Some(definition) = existing.get(expected.key) else {
            missing.push(expected.key);
            continue;
        };
        let expected_definition = normalize_catalog_sql(expected.definition);
        if definition != &expected_definition {
            drifted.push(format!(
                "{} expected {}; found {definition}",
                expected.key, expected_definition
            ));
        }
    }

    bail_if_missing("index", &missing)?;
    bail_if_drifted("index", &drifted)
}

#[derive(Clone, Copy)]
struct ExpectedFunction {
    name: &'static str,
    returns: &'static str,
    arg_count: i16,
}

async fn verify_required_functions(pool: &DbPool) -> anyhow::Result<()> {
    let rows = sqlx::query(
        "SELECT p.proname,
                p.pronargs::int2 AS arg_count,
                pg_get_function_result(p.oid) AS returns
         FROM pg_proc p
         JOIN pg_namespace n ON n.oid = p.pronamespace
         WHERE n.nspname = 'public'
           AND p.pronargs = 0",
    )
    .fetch_all(pool)
    .await
    .context("failed to inspect required schema functions")?;
    let existing: HashMap<String, (i16, String)> = rows
        .into_iter()
        .map(|row| {
            (
                row.get("proname"),
                (row.get("arg_count"), row.get("returns")),
            )
        })
        .collect();

    let mut missing = Vec::new();
    let mut drifted = Vec::new();
    for expected in expected_functions() {
        let Some((arg_count, returns)) = existing.get(expected.name) else {
            missing.push(expected.name);
            continue;
        };
        if *arg_count != expected.arg_count || returns != expected.returns {
            drifted.push(format!(
                "{} expected {} arg(s) returning {}; found {} arg(s) returning {returns}",
                expected.name, expected.arg_count, expected.returns, arg_count
            ));
        }
    }

    bail_if_missing("function", &missing)?;
    bail_if_drifted("function", &drifted)
}

async fn verify_required_triggers(pool: &DbPool) -> anyhow::Result<()> {
    let rows = sqlx::query(
        "SELECT c.relname || '.' || t.tgname AS key,
                t.tgenabled::text AS enabled,
                pg_get_triggerdef(t.oid) AS definition
         FROM pg_trigger t
         JOIN pg_class c ON c.oid = t.tgrelid
         JOIN pg_namespace n ON n.oid = c.relnamespace
         WHERE n.nspname = 'public'
           AND NOT t.tgisinternal",
    )
    .fetch_all(pool)
    .await
    .context("failed to inspect required schema triggers")?;
    let existing: HashMap<String, (String, String)> = rows
        .into_iter()
        .map(|row| {
            (
                row.get("key"),
                (
                    row.get("enabled"),
                    normalize_catalog_sql(&row.get::<String, _>("definition")),
                ),
            )
        })
        .collect();

    let mut missing = Vec::new();
    let mut drifted = Vec::new();
    for expected in expected_triggers() {
        let Some((enabled, definition)) = existing.get(expected.key) else {
            missing.push(expected.key);
            continue;
        };
        let expected_definition = normalize_catalog_sql(expected.definition);
        if enabled != "O" || definition != &expected_definition {
            drifted.push(format!(
                "{} expected enabled=O def={}; found enabled={enabled} def={definition}",
                expected.key, expected_definition
            ));
        }
    }

    bail_if_missing("trigger", &missing)?;
    bail_if_drifted("trigger", &drifted)
}

fn normalize_catalog_sql(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn bail_if_missing<T: AsRef<str>>(kind: &str, missing: &[T]) -> anyhow::Result<()> {
    if missing.is_empty() {
        return Ok(());
    }

    let names: Vec<&str> = missing.iter().map(AsRef::as_ref).collect();
    bail!(
        "Postgres schema contract missing required {kind}(s): {}; apply schema/postgres.sql to a fresh database",
        names.join(", ")
    );
}

fn bail_if_drifted<T: AsRef<str>>(kind: &str, drifted: &[T]) -> anyhow::Result<()> {
    if drifted.is_empty() {
        return Ok(());
    }

    let messages: Vec<&str> = drifted.iter().map(AsRef::as_ref).collect();
    bail!(
        "Postgres schema contract drifted required {kind}(s): {}; apply schema/postgres.sql to a fresh database",
        messages.join("; ")
    );
}

fn expected_columns() -> Vec<ExpectedColumn> {
    vec![
        col("users", "id", "text", false, None),
        col("users", "email", "text", false, None),
        col("users", "name", "text", true, None),
        col("users", "is_server_admin", "boolean", false, Some("false")),
        col(
            "users",
            "created_at",
            "timestamp with time zone",
            false,
            Some("now()"),
        ),
        col(
            "users",
            "updated_at",
            "timestamp with time zone",
            false,
            Some("now()"),
        ),
        col("schema_migrations", "id", "text", false, None),
        col("schema_migrations", "checksum_sha256", "text", false, None),
        col(
            "schema_migrations",
            "applied_at",
            "timestamp with time zone",
            false,
            Some("now()"),
        ),
        col("schema_migrations", "applied_by_build", "text", true, None),
        col("schema_migrations", "execution_ms", "bigint", false, None),
        col("orgs", "id", "text", false, None),
        col("orgs", "slug", "text", false, None),
        col("orgs", "name", "text", false, None),
        col(
            "orgs",
            "created_at",
            "timestamp with time zone",
            false,
            Some("now()"),
        ),
        col(
            "orgs",
            "updated_at",
            "timestamp with time zone",
            false,
            Some("now()"),
        ),
        col("principals", "id", "text", false, None),
        col("principals", "principal_type", "text", false, None),
        col("principals", "display_name", "text", false, None),
        col(
            "principals",
            "is_server_admin",
            "boolean",
            false,
            Some("false"),
        ),
        col(
            "principals",
            "disabled_at",
            "timestamp with time zone",
            true,
            None,
        ),
        col(
            "principals",
            "created_at",
            "timestamp with time zone",
            false,
            Some("now()"),
        ),
        col(
            "principals",
            "updated_at",
            "timestamp with time zone",
            false,
            Some("now()"),
        ),
        col("human_profiles", "principal_id", "text", false, None),
        col("human_profiles", "user_id", "text", false, None),
        col("human_profiles", "email", "text", false, None),
        col("human_profiles", "name", "text", true, None),
        col(
            "human_profiles",
            "created_at",
            "timestamp with time zone",
            false,
            Some("now()"),
        ),
        col(
            "human_profiles",
            "updated_at",
            "timestamp with time zone",
            false,
            Some("now()"),
        ),
        col("external_identities", "id", "text", false, None),
        col("external_identities", "principal_id", "text", false, None),
        col("external_identities", "provider", "text", false, None),
        col("external_identities", "issuer", "text", false, None),
        col("external_identities", "subject", "text", false, None),
        col("external_identities", "email", "text", false, None),
        col(
            "external_identities",
            "email_verified",
            "boolean",
            false,
            None,
        ),
        col("external_identities", "name", "text", true, None),
        col(
            "external_identities",
            "created_at",
            "timestamp with time zone",
            false,
            Some("now()"),
        ),
        col(
            "external_identities",
            "last_seen_at",
            "timestamp with time zone",
            true,
            None,
        ),
        col("org_members", "org_id", "text", false, None),
        col("org_members", "user_id", "text", false, None),
        col("org_members", "role", "text", false, None),
        col(
            "org_members",
            "created_at",
            "timestamp with time zone",
            false,
            Some("now()"),
        ),
        col(
            "org_members",
            "updated_at",
            "timestamp with time zone",
            false,
            Some("now()"),
        ),
        col("invites", "id", "text", false, None),
        col("invites", "email", "text", false, None),
        col("invites", "org_id", "text", false, None),
        col("invites", "role", "text", false, None),
        col("invites", "invited_by_principal_id", "text", true, None),
        col("invites", "accepted_by_principal_id", "text", true, None),
        col("invites", "revoked_by_principal_id", "text", true, None),
        col(
            "invites",
            "created_at",
            "timestamp with time zone",
            false,
            Some("now()"),
        ),
        col(
            "invites",
            "expires_at",
            "timestamp with time zone",
            false,
            None,
        ),
        col(
            "invites",
            "accepted_at",
            "timestamp with time zone",
            true,
            None,
        ),
        col(
            "invites",
            "revoked_at",
            "timestamp with time zone",
            true,
            None,
        ),
        col("tokens", "id", "text", false, None),
        col("tokens", "user_id", "text", true, None),
        col("tokens", "principal_id", "text", false, None),
        col("tokens", "label", "text", false, None),
        col("tokens", "token_hash", "text", false, None),
        col("tokens", "token_kind", "text", false, Some("'user'::text")),
        col(
            "tokens",
            "scopes",
            "jsonb",
            false,
            Some("'[\"registry:*\"]'::jsonb"),
        ),
        col("tokens", "created_by_principal_id", "text", true, None),
        col(
            "tokens",
            "created_at",
            "timestamp with time zone",
            false,
            Some("now()"),
        ),
        col(
            "tokens",
            "expires_at",
            "timestamp with time zone",
            true,
            None,
        ),
        col(
            "tokens",
            "last_used_at",
            "timestamp with time zone",
            true,
            None,
        ),
        col(
            "tokens",
            "revoked_at",
            "timestamp with time zone",
            true,
            None,
        ),
        col("ui_sessions", "id", "text", false, None),
        col("ui_sessions", "token_id", "text", false, None),
        col("ui_sessions", "session_hash", "text", false, None),
        col(
            "ui_sessions",
            "created_at",
            "timestamp with time zone",
            false,
            Some("now()"),
        ),
        col(
            "ui_sessions",
            "expires_at",
            "timestamp with time zone",
            false,
            None,
        ),
        col(
            "ui_sessions",
            "last_used_at",
            "timestamp with time zone",
            true,
            None,
        ),
        col(
            "ui_sessions",
            "revoked_at",
            "timestamp with time zone",
            true,
            None,
        ),
        col("browser_sessions", "id", "text", false, None),
        col("browser_sessions", "principal_id", "text", false, None),
        col("browser_sessions", "session_hash", "text", false, None),
        col(
            "browser_sessions",
            "created_at",
            "timestamp with time zone",
            false,
            Some("now()"),
        ),
        col(
            "browser_sessions",
            "expires_at",
            "timestamp with time zone",
            false,
            None,
        ),
        col(
            "browser_sessions",
            "last_used_at",
            "timestamp with time zone",
            true,
            None,
        ),
        col(
            "browser_sessions",
            "revoked_at",
            "timestamp with time zone",
            true,
            None,
        ),
        col("oauth_login_states", "id", "text", false, None),
        col("oauth_login_states", "state_hash", "text", false, None),
        col("oauth_login_states", "nonce_hash", "text", false, None),
        col(
            "oauth_login_states",
            "code_verifier_secret",
            "text",
            false,
            None,
        ),
        col(
            "oauth_login_states",
            "redirect_after_path",
            "text",
            true,
            None,
        ),
        col(
            "oauth_login_states",
            "created_at",
            "timestamp with time zone",
            false,
            Some("now()"),
        ),
        col(
            "oauth_login_states",
            "expires_at",
            "timestamp with time zone",
            false,
            None,
        ),
        col(
            "oauth_login_states",
            "consumed_at",
            "timestamp with time zone",
            true,
            None,
        ),
        col("machine_principals", "id", "text", false, None),
        col("machine_principals", "principal_id", "text", false, None),
        col("machine_principals", "org_id", "text", false, None),
        col("machine_principals", "slug", "text", false, None),
        col("machine_principals", "display_name", "text", false, None),
        col(
            "machine_principals",
            "owner_principal_id",
            "text",
            true,
            None,
        ),
        col(
            "machine_principals",
            "created_by_principal_id",
            "text",
            true,
            None,
        ),
        col(
            "machine_principals",
            "disabled_at",
            "timestamp with time zone",
            true,
            None,
        ),
        col(
            "machine_principals",
            "created_at",
            "timestamp with time zone",
            false,
            Some("now()"),
        ),
        col(
            "machine_principals",
            "updated_at",
            "timestamp with time zone",
            false,
            Some("now()"),
        ),
        col("teams", "id", "text", false, None),
        col("teams", "org_id", "text", false, None),
        col("teams", "slug", "text", false, None),
        col("teams", "name", "text", false, None),
        col(
            "teams",
            "created_at",
            "timestamp with time zone",
            false,
            Some("now()"),
        ),
        col(
            "teams",
            "updated_at",
            "timestamp with time zone",
            false,
            Some("now()"),
        ),
        col("team_memberships", "team_id", "text", false, None),
        col("team_memberships", "org_id", "text", false, None),
        col("team_memberships", "user_id", "text", false, None),
        col("team_memberships", "role", "text", false, None),
        col(
            "team_memberships",
            "created_at",
            "timestamp with time zone",
            false,
            Some("now()"),
        ),
        col(
            "team_memberships",
            "updated_at",
            "timestamp with time zone",
            false,
            Some("now()"),
        ),
        col("archives", "id", "text", false, None),
        col("archives", "hash_algorithm", "text", false, None),
        col("archives", "hash_hex", "text", false, None),
        col("archives", "storage_key", "text", false, None),
        col("archives", "size_bytes", "bigint", false, None),
        col(
            "archives",
            "created_at",
            "timestamp with time zone",
            false,
            Some("now()"),
        ),
        col("skills", "id", "text", false, None),
        col("skills", "org_id", "text", false, None),
        col("skills", "name", "text", false, None),
        col("skills", "description", "text", false, None),
        col("skills", "visibility", "text", false, None),
        col("skills", "team_id", "text", true, None),
        col("skills", "owner_user_id", "text", false, None),
        col("skills", "current_version_id", "text", true, None),
        col("skills", "next_version_number", "bigint", false, Some("1")),
        col(
            "skills",
            "created_at",
            "timestamp with time zone",
            false,
            Some("now()"),
        ),
        col(
            "skills",
            "updated_at",
            "timestamp with time zone",
            false,
            Some("now()"),
        ),
        col("skill_versions", "id", "text", false, None),
        col("skill_versions", "skill_id", "text", false, None),
        col("skill_versions", "version_number", "bigint", false, None),
        col("skill_versions", "archive_id", "text", false, None),
        col("skill_versions", "description", "text", false, None),
        col(
            "skill_versions",
            "published_by_user_id",
            "text",
            false,
            None,
        ),
        col(
            "skill_versions",
            "created_at",
            "timestamp with time zone",
            false,
            Some("now()"),
        ),
        col(
            "skill_versions",
            "status",
            "text",
            false,
            Some("'candidate'::text"),
        ),
        col("skill_versions", "approved_by_user_id", "text", true, None),
        col(
            "skill_versions",
            "approved_at",
            "timestamp with time zone",
            true,
            None,
        ),
        col("skill_versions", "yanked_by_user_id", "text", true, None),
        col(
            "skill_versions",
            "yanked_at",
            "timestamp with time zone",
            true,
            None,
        ),
        col("skill_versions", "yank_reason", "text", true, None),
        col(
            "skill_versions",
            "deprecated_by_user_id",
            "text",
            true,
            None,
        ),
        col(
            "skill_versions",
            "deprecated_at",
            "timestamp with time zone",
            true,
            None,
        ),
        col("skill_versions", "deprecation_reason", "text", true, None),
        col(
            "skill_version_platform_tags",
            "skill_version_id",
            "text",
            false,
            None,
        ),
        col("skill_version_platform_tags", "tag", "text", false, None),
        col("stacks", "id", "text", false, None),
        col("stacks", "org_id", "text", false, None),
        col("stacks", "slug", "text", false, None),
        col("stacks", "name", "text", false, None),
        col("stacks", "description", "text", false, None),
        col("stacks", "visibility", "text", false, None),
        col("stacks", "team_id", "text", true, None),
        col("stacks", "owner_user_id", "text", false, None),
        col(
            "stacks",
            "created_at",
            "timestamp with time zone",
            false,
            Some("now()"),
        ),
        col(
            "stacks",
            "updated_at",
            "timestamp with time zone",
            false,
            Some("now()"),
        ),
        col("stack_items", "id", "text", false, None),
        col("stack_items", "stack_id", "text", false, None),
        col("stack_items", "skill_id", "text", false, None),
        col("stack_items", "version_policy", "text", false, None),
        col("stack_items", "pinned_version_id", "text", true, None),
        col("stack_items", "position", "bigint", false, None),
        col("stack_items", "added_by_user_id", "text", false, None),
        col(
            "stack_items",
            "added_at",
            "timestamp with time zone",
            false,
            Some("now()"),
        ),
        col("audit_log", "id", "text", false, None),
        col("audit_log", "org_id", "text", true, None),
        col("audit_log", "actor_user_id", "text", true, None),
        col("audit_log", "actor_principal_id", "text", true, None),
        col("audit_log", "actor_type", "text", true, None),
        col("audit_log", "action", "text", false, None),
        col("audit_log", "resource_type", "text", false, None),
        col("audit_log", "resource_id", "text", true, None),
        col("audit_log", "resource_ref", "text", true, None),
        col("audit_log", "metadata", "jsonb", false, Some("'{}'::jsonb")),
        col(
            "audit_log",
            "created_at",
            "timestamp with time zone",
            false,
            Some("now()"),
        ),
    ]
}

const fn col(
    table: &'static str,
    name: &'static str,
    data_type: &'static str,
    nullable: bool,
    default: Option<&'static str>,
) -> ExpectedColumn {
    ExpectedColumn {
        table,
        name,
        data_type,
        nullable,
        default,
    }
}

fn expected_functions() -> Vec<ExpectedFunction> {
    vec![
        func("set_updated_at"),
        func("require_current_version_approved"),
        func("prevent_current_version_unapprove"),
        func("advance_skill_next_version_number"),
        func("require_skill_version_publisher_member"),
        func("require_stack_item_adder_member"),
    ]
}

const fn func(name: &'static str) -> ExpectedFunction {
    ExpectedFunction {
        name,
        returns: "trigger",
        arg_count: 0,
    }
}

fn expected_indexes() -> Vec<ExpectedCatalogDef> {
    vec![
        def(
            "idx_principals_type",
            "CREATE INDEX idx_principals_type ON public.principals USING btree (principal_type)",
        ),
        def(
            "idx_human_profiles_user_id",
            "CREATE INDEX idx_human_profiles_user_id ON public.human_profiles USING btree (user_id)",
        ),
        def(
            "idx_external_identities_principal_id",
            "CREATE INDEX idx_external_identities_principal_id ON public.external_identities USING btree (principal_id)",
        ),
        def(
            "idx_invites_org_email",
            "CREATE INDEX idx_invites_org_email ON public.invites USING btree (org_id, email) WHERE ((accepted_at IS NULL) AND (revoked_at IS NULL))",
        ),
        def(
            "idx_org_members_user_id",
            "CREATE INDEX idx_org_members_user_id ON public.org_members USING btree (user_id)",
        ),
        def(
            "idx_tokens_user_id",
            "CREATE INDEX idx_tokens_user_id ON public.tokens USING btree (user_id)",
        ),
        def(
            "idx_tokens_principal_id",
            "CREATE INDEX idx_tokens_principal_id ON public.tokens USING btree (principal_id)",
        ),
        def(
            "idx_tokens_active_user_id",
            "CREATE INDEX idx_tokens_active_user_id ON public.tokens USING btree (user_id) WHERE (revoked_at IS NULL)",
        ),
        def(
            "idx_ui_sessions_token_id",
            "CREATE INDEX idx_ui_sessions_token_id ON public.ui_sessions USING btree (token_id)",
        ),
        def(
            "idx_ui_sessions_active_hash",
            "CREATE INDEX idx_ui_sessions_active_hash ON public.ui_sessions USING btree (session_hash) WHERE (revoked_at IS NULL)",
        ),
        def(
            "idx_browser_sessions_principal_id",
            "CREATE INDEX idx_browser_sessions_principal_id ON public.browser_sessions USING btree (principal_id)",
        ),
        def(
            "idx_browser_sessions_active_hash",
            "CREATE INDEX idx_browser_sessions_active_hash ON public.browser_sessions USING btree (session_hash) WHERE (revoked_at IS NULL)",
        ),
        def(
            "idx_oauth_login_states_expires",
            "CREATE INDEX idx_oauth_login_states_expires ON public.oauth_login_states USING btree (expires_at)",
        ),
        def(
            "idx_machine_principals_org_id",
            "CREATE INDEX idx_machine_principals_org_id ON public.machine_principals USING btree (org_id)",
        ),
        def(
            "idx_machine_principals_principal_id",
            "CREATE INDEX idx_machine_principals_principal_id ON public.machine_principals USING btree (principal_id)",
        ),
        def(
            "idx_teams_org_id",
            "CREATE INDEX idx_teams_org_id ON public.teams USING btree (org_id)",
        ),
        def(
            "idx_team_memberships_user_id",
            "CREATE INDEX idx_team_memberships_user_id ON public.team_memberships USING btree (user_id)",
        ),
        def(
            "idx_team_memberships_org_user",
            "CREATE INDEX idx_team_memberships_org_user ON public.team_memberships USING btree (org_id, user_id)",
        ),
        def(
            "idx_skills_org_updated",
            "CREATE INDEX idx_skills_org_updated ON public.skills USING btree (org_id, updated_at DESC)",
        ),
        def(
            "idx_skills_owner_user_id",
            "CREATE INDEX idx_skills_owner_user_id ON public.skills USING btree (owner_user_id)",
        ),
        def(
            "idx_skills_team_id",
            "CREATE INDEX idx_skills_team_id ON public.skills USING btree (team_id)",
        ),
        def(
            "idx_skills_visibility",
            "CREATE INDEX idx_skills_visibility ON public.skills USING btree (visibility)",
        ),
        def(
            "idx_skill_versions_skill_version_desc",
            "CREATE INDEX idx_skill_versions_skill_version_desc ON public.skill_versions USING btree (skill_id, version_number DESC)",
        ),
        def(
            "idx_skill_versions_archive_id",
            "CREATE INDEX idx_skill_versions_archive_id ON public.skill_versions USING btree (archive_id)",
        ),
        def(
            "idx_skill_versions_published_by_user_id",
            "CREATE INDEX idx_skill_versions_published_by_user_id ON public.skill_versions USING btree (published_by_user_id)",
        ),
        def(
            "idx_skill_versions_status",
            "CREATE INDEX idx_skill_versions_status ON public.skill_versions USING btree (status)",
        ),
        def(
            "idx_skill_version_platform_tags_tag",
            "CREATE INDEX idx_skill_version_platform_tags_tag ON public.skill_version_platform_tags USING btree (tag)",
        ),
        def(
            "idx_stacks_org_updated",
            "CREATE INDEX idx_stacks_org_updated ON public.stacks USING btree (org_id, updated_at DESC)",
        ),
        def(
            "idx_stacks_owner_user_id",
            "CREATE INDEX idx_stacks_owner_user_id ON public.stacks USING btree (owner_user_id)",
        ),
        def(
            "idx_stacks_team_id",
            "CREATE INDEX idx_stacks_team_id ON public.stacks USING btree (team_id)",
        ),
        def(
            "idx_stack_items_stack_id",
            "CREATE INDEX idx_stack_items_stack_id ON public.stack_items USING btree (stack_id, \"position\")",
        ),
        def(
            "idx_stack_items_skill_id",
            "CREATE INDEX idx_stack_items_skill_id ON public.stack_items USING btree (skill_id)",
        ),
        def(
            "idx_audit_log_org_created",
            "CREATE INDEX idx_audit_log_org_created ON public.audit_log USING btree (org_id, created_at DESC)",
        ),
        def(
            "idx_audit_log_actor_created",
            "CREATE INDEX idx_audit_log_actor_created ON public.audit_log USING btree (actor_user_id, created_at DESC)",
        ),
        def(
            "idx_audit_log_actor_principal_created",
            "CREATE INDEX idx_audit_log_actor_principal_created ON public.audit_log USING btree (actor_principal_id, created_at DESC)",
        ),
        def(
            "idx_audit_log_resource",
            "CREATE INDEX idx_audit_log_resource ON public.audit_log USING btree (resource_type, resource_id, created_at DESC)",
        ),
    ]
}

fn expected_triggers() -> Vec<ExpectedCatalogDef> {
    vec![
        def(
            "users.users_set_updated_at",
            "CREATE TRIGGER users_set_updated_at BEFORE UPDATE ON public.users FOR EACH ROW EXECUTE FUNCTION set_updated_at()",
        ),
        def(
            "orgs.orgs_set_updated_at",
            "CREATE TRIGGER orgs_set_updated_at BEFORE UPDATE ON public.orgs FOR EACH ROW EXECUTE FUNCTION set_updated_at()",
        ),
        def(
            "principals.principals_set_updated_at",
            "CREATE TRIGGER principals_set_updated_at BEFORE UPDATE ON public.principals FOR EACH ROW EXECUTE FUNCTION set_updated_at()",
        ),
        def(
            "human_profiles.human_profiles_set_updated_at",
            "CREATE TRIGGER human_profiles_set_updated_at BEFORE UPDATE ON public.human_profiles FOR EACH ROW EXECUTE FUNCTION set_updated_at()",
        ),
        def(
            "org_members.org_members_set_updated_at",
            "CREATE TRIGGER org_members_set_updated_at BEFORE UPDATE ON public.org_members FOR EACH ROW EXECUTE FUNCTION set_updated_at()",
        ),
        def(
            "machine_principals.machine_principals_set_updated_at",
            "CREATE TRIGGER machine_principals_set_updated_at BEFORE UPDATE ON public.machine_principals FOR EACH ROW EXECUTE FUNCTION set_updated_at()",
        ),
        def(
            "teams.teams_set_updated_at",
            "CREATE TRIGGER teams_set_updated_at BEFORE UPDATE ON public.teams FOR EACH ROW EXECUTE FUNCTION set_updated_at()",
        ),
        def(
            "team_memberships.team_memberships_set_updated_at",
            "CREATE TRIGGER team_memberships_set_updated_at BEFORE UPDATE ON public.team_memberships FOR EACH ROW EXECUTE FUNCTION set_updated_at()",
        ),
        def(
            "skills.skills_set_updated_at",
            "CREATE TRIGGER skills_set_updated_at BEFORE UPDATE ON public.skills FOR EACH ROW EXECUTE FUNCTION set_updated_at()",
        ),
        def(
            "stacks.stacks_set_updated_at",
            "CREATE TRIGGER stacks_set_updated_at BEFORE UPDATE ON public.stacks FOR EACH ROW EXECUTE FUNCTION set_updated_at()",
        ),
        def(
            "skills.skills_current_version_check",
            "CREATE TRIGGER skills_current_version_check BEFORE INSERT OR UPDATE OF current_version_id ON public.skills FOR EACH ROW EXECUTE FUNCTION require_current_version_approved()",
        ),
        def(
            "skill_versions.skill_versions_current_status_update_check",
            "CREATE TRIGGER skill_versions_current_status_update_check BEFORE UPDATE OF status ON public.skill_versions FOR EACH ROW EXECUTE FUNCTION prevent_current_version_unapprove()",
        ),
        def(
            "skill_versions.skill_versions_advance_next_version_number",
            "CREATE TRIGGER skill_versions_advance_next_version_number AFTER INSERT ON public.skill_versions FOR EACH ROW EXECUTE FUNCTION advance_skill_next_version_number()",
        ),
        def(
            "skill_versions.skill_versions_publisher_member_check",
            "CREATE TRIGGER skill_versions_publisher_member_check BEFORE INSERT OR UPDATE OF skill_id, published_by_user_id ON public.skill_versions FOR EACH ROW EXECUTE FUNCTION require_skill_version_publisher_member()",
        ),
        def(
            "stack_items.stack_items_adder_member_check",
            "CREATE TRIGGER stack_items_adder_member_check BEFORE INSERT OR UPDATE OF stack_id, added_by_user_id ON public.stack_items FOR EACH ROW EXECUTE FUNCTION require_stack_item_adder_member()",
        ),
    ]
}

const fn def(key: &'static str, definition: &'static str) -> ExpectedCatalogDef {
    ExpectedCatalogDef {
        key,
        kind: None,
        definition,
    }
}

const fn con(
    key: &'static str,
    kind: &'static str,
    definition: &'static str,
) -> ExpectedCatalogDef {
    ExpectedCatalogDef {
        key,
        kind: Some(kind),
        definition,
    }
}

fn expected_constraints() -> Vec<ExpectedCatalogDef> {
    vec![
        con("users.users_pkey", "p", "PRIMARY KEY (id)"),
        con("users.users_email_key", "u", "UNIQUE (email)"),
        con(
            "users.users_email_check",
            "c",
            "CHECK ((email = lower(email)))",
        ),
        con(
            "users.users_email_check1",
            "c",
            "CHECK (((POSITION(('@'::text) IN (email)) > 1) AND (POSITION(('@'::text) IN (email)) < length(email))))",
        ),
        con(
            "schema_migrations.schema_migrations_pkey",
            "p",
            "PRIMARY KEY (id)",
        ),
        con(
            "schema_migrations.schema_migrations_id_check",
            "c",
            "CHECK ((id ~ '^[0-9]{8}_[a-z0-9_]+$'::text))",
        ),
        con(
            "schema_migrations.schema_migrations_checksum_sha256_check",
            "c",
            "CHECK ((checksum_sha256 ~ '^[0-9a-f]{64}$'::text))",
        ),
        con(
            "schema_migrations.schema_migrations_execution_ms_check",
            "c",
            "CHECK ((execution_ms >= 0))",
        ),
        con("orgs.orgs_pkey", "p", "PRIMARY KEY (id)"),
        con("orgs.orgs_slug_key", "u", "UNIQUE (slug)"),
        con(
            "orgs.orgs_slug_check",
            "c",
            "CHECK ((slug ~ '^[a-z0-9][a-z0-9_-]*$'::text))",
        ),
        con("principals.principals_pkey", "p", "PRIMARY KEY (id)"),
        con(
            "principals.principals_principal_type_check",
            "c",
            "CHECK ((principal_type = ANY (ARRAY['human'::text, 'machine'::text])))",
        ),
        con(
            "principals.principals_id_check",
            "c",
            "CHECK ((id ~ '^prn_[0-9a-f]{32}$'::text))",
        ),
        con(
            "principals.principals_display_name_check",
            "c",
            "CHECK ((btrim(display_name) <> ''::text))",
        ),
        con(
            "human_profiles.human_profiles_pkey",
            "p",
            "PRIMARY KEY (principal_id)",
        ),
        con(
            "human_profiles.human_profiles_principal_id_fkey",
            "f",
            "FOREIGN KEY (principal_id) REFERENCES principals(id) ON DELETE CASCADE",
        ),
        con(
            "human_profiles.human_profiles_user_id_fkey",
            "f",
            "FOREIGN KEY (user_id) REFERENCES users(id) ON DELETE CASCADE",
        ),
        con(
            "human_profiles.human_profiles_user_id_key",
            "u",
            "UNIQUE (user_id)",
        ),
        con(
            "human_profiles.human_profiles_email_key",
            "u",
            "UNIQUE (email)",
        ),
        con(
            "human_profiles.human_profiles_email_check",
            "c",
            "CHECK ((email = lower(email)))",
        ),
        con(
            "human_profiles.human_profiles_email_check1",
            "c",
            "CHECK (((POSITION(('@'::text) IN (email)) > 1) AND (POSITION(('@'::text) IN (email)) < length(email))))",
        ),
        con(
            "external_identities.external_identities_pkey",
            "p",
            "PRIMARY KEY (id)",
        ),
        con(
            "external_identities.external_identities_principal_id_fkey",
            "f",
            "FOREIGN KEY (principal_id) REFERENCES principals(id) ON DELETE CASCADE",
        ),
        con(
            "external_identities.external_identities_issuer_subject_key",
            "u",
            "UNIQUE (issuer, subject)",
        ),
        con(
            "external_identities.external_identities_id_check",
            "c",
            "CHECK ((id ~ '^ext_[0-9a-f]{32}$'::text))",
        ),
        con(
            "external_identities.external_identities_provider_check",
            "c",
            "CHECK ((provider ~ '^[a-z0-9][a-z0-9_-]*$'::text))",
        ),
        con(
            "external_identities.external_identities_issuer_check",
            "c",
            "CHECK ((btrim(issuer) <> ''::text))",
        ),
        con(
            "external_identities.external_identities_subject_check",
            "c",
            "CHECK ((btrim(subject) <> ''::text))",
        ),
        con(
            "external_identities.external_identities_email_check",
            "c",
            "CHECK ((email = lower(email)))",
        ),
        con(
            "external_identities.external_identities_email_check1",
            "c",
            "CHECK (((POSITION(('@'::text) IN (email)) > 1) AND (POSITION(('@'::text) IN (email)) < length(email))))",
        ),
        con(
            "org_members.org_members_pkey",
            "p",
            "PRIMARY KEY (org_id, user_id)",
        ),
        con(
            "org_members.org_members_org_id_fkey",
            "f",
            "FOREIGN KEY (org_id) REFERENCES orgs(id) ON DELETE CASCADE",
        ),
        con(
            "org_members.org_members_user_id_fkey",
            "f",
            "FOREIGN KEY (user_id) REFERENCES users(id) ON DELETE CASCADE",
        ),
        con(
            "org_members.org_members_role_check",
            "c",
            "CHECK ((role = ANY (ARRAY['org_admin'::text, 'publisher'::text, 'reader'::text])))",
        ),
        con("invites.invites_pkey", "p", "PRIMARY KEY (id)"),
        con(
            "invites.invites_org_id_fkey",
            "f",
            "FOREIGN KEY (org_id) REFERENCES orgs(id) ON DELETE CASCADE",
        ),
        con(
            "invites.invites_invited_by_principal_id_fkey",
            "f",
            "FOREIGN KEY (invited_by_principal_id) REFERENCES principals(id) ON DELETE SET NULL",
        ),
        con(
            "invites.invites_accepted_by_principal_id_fkey",
            "f",
            "FOREIGN KEY (accepted_by_principal_id) REFERENCES principals(id) ON DELETE SET NULL",
        ),
        con(
            "invites.invites_revoked_by_principal_id_fkey",
            "f",
            "FOREIGN KEY (revoked_by_principal_id) REFERENCES principals(id) ON DELETE SET NULL",
        ),
        con(
            "invites.invites_role_check",
            "c",
            "CHECK ((role = ANY (ARRAY['org_admin'::text, 'publisher'::text, 'reader'::text])))",
        ),
        con(
            "invites.invites_id_check",
            "c",
            "CHECK ((id ~ '^inv_[0-9a-f]{32}$'::text))",
        ),
        con(
            "invites.invites_email_check",
            "c",
            "CHECK ((email = lower(email)))",
        ),
        con(
            "invites.invites_email_check1",
            "c",
            "CHECK (((POSITION(('@'::text) IN (email)) > 1) AND (POSITION(('@'::text) IN (email)) < length(email))))",
        ),
        con(
            "invites.invites_check",
            "c",
            "CHECK ((expires_at > created_at))",
        ),
        con(
            "invites.invites_check1",
            "c",
            "CHECK (((accepted_at IS NULL) = (accepted_by_principal_id IS NULL)))",
        ),
        con(
            "invites.invites_check2",
            "c",
            "CHECK (((revoked_at IS NULL) = (revoked_by_principal_id IS NULL)))",
        ),
        con(
            "invites.invites_check3",
            "c",
            "CHECK (((accepted_at IS NULL) OR (revoked_at IS NULL)))",
        ),
        con("tokens.tokens_pkey", "p", "PRIMARY KEY (id)"),
        con(
            "tokens.tokens_user_id_fkey",
            "f",
            "FOREIGN KEY (user_id) REFERENCES users(id) ON DELETE CASCADE",
        ),
        con(
            "tokens.tokens_principal_id_fkey",
            "f",
            "FOREIGN KEY (principal_id) REFERENCES principals(id) ON DELETE CASCADE",
        ),
        con(
            "tokens.tokens_created_by_principal_id_fkey",
            "f",
            "FOREIGN KEY (created_by_principal_id) REFERENCES principals(id) ON DELETE SET NULL",
        ),
        con("tokens.tokens_token_hash_key", "u", "UNIQUE (token_hash)"),
        con(
            "tokens.tokens_token_hash_check",
            "c",
            "CHECK ((token_hash ~ '^[0-9a-f]{64}$'::text))",
        ),
        con(
            "tokens.tokens_token_kind_check",
            "c",
            "CHECK ((token_kind = ANY (ARRAY['user'::text, 'machine'::text])))",
        ),
        con(
            "tokens.tokens_scopes_check",
            "c",
            "CHECK ((jsonb_typeof(scopes) = 'array'::text))",
        ),
        con(
            "tokens.tokens_user_kind_check",
            "c",
            "CHECK (((token_kind = 'user'::text) = (user_id IS NOT NULL)))",
        ),
        con("ui_sessions.ui_sessions_pkey", "p", "PRIMARY KEY (id)"),
        con(
            "ui_sessions.ui_sessions_token_id_fkey",
            "f",
            "FOREIGN KEY (token_id) REFERENCES tokens(id) ON DELETE CASCADE",
        ),
        con(
            "ui_sessions.ui_sessions_session_hash_key",
            "u",
            "UNIQUE (session_hash)",
        ),
        con(
            "ui_sessions.ui_sessions_session_hash_check",
            "c",
            "CHECK ((session_hash ~ '^[0-9a-f]{64}$'::text))",
        ),
        con(
            "browser_sessions.browser_sessions_pkey",
            "p",
            "PRIMARY KEY (id)",
        ),
        con(
            "browser_sessions.browser_sessions_principal_id_fkey",
            "f",
            "FOREIGN KEY (principal_id) REFERENCES principals(id) ON DELETE CASCADE",
        ),
        con(
            "browser_sessions.browser_sessions_session_hash_key",
            "u",
            "UNIQUE (session_hash)",
        ),
        con(
            "browser_sessions.browser_sessions_id_check",
            "c",
            "CHECK ((id ~ '^brs_[0-9a-f]{32}$'::text))",
        ),
        con(
            "browser_sessions.browser_sessions_session_hash_check",
            "c",
            "CHECK ((session_hash ~ '^[0-9a-f]{64}$'::text))",
        ),
        con(
            "browser_sessions.browser_sessions_check",
            "c",
            "CHECK ((expires_at > created_at))",
        ),
        con(
            "oauth_login_states.oauth_login_states_pkey",
            "p",
            "PRIMARY KEY (id)",
        ),
        con(
            "oauth_login_states.oauth_login_states_state_hash_key",
            "u",
            "UNIQUE (state_hash)",
        ),
        con(
            "oauth_login_states.oauth_login_states_id_check",
            "c",
            "CHECK ((id ~ '^ols_[0-9a-f]{32}$'::text))",
        ),
        con(
            "oauth_login_states.oauth_login_states_state_hash_check",
            "c",
            "CHECK ((state_hash ~ '^[0-9a-f]{64}$'::text))",
        ),
        con(
            "oauth_login_states.oauth_login_states_nonce_hash_check",
            "c",
            "CHECK ((nonce_hash ~ '^[0-9a-f]{64}$'::text))",
        ),
        con(
            "oauth_login_states.oauth_login_states_code_verifier_secret_check",
            "c",
            "CHECK ((btrim(code_verifier_secret) <> ''::text))",
        ),
        con(
            "oauth_login_states.oauth_login_states_check",
            "c",
            "CHECK ((expires_at > created_at))",
        ),
        con(
            "machine_principals.machine_principals_pkey",
            "p",
            "PRIMARY KEY (id)",
        ),
        con(
            "machine_principals.machine_principals_principal_id_fkey",
            "f",
            "FOREIGN KEY (principal_id) REFERENCES principals(id) ON DELETE CASCADE",
        ),
        con(
            "machine_principals.machine_principals_org_id_fkey",
            "f",
            "FOREIGN KEY (org_id) REFERENCES orgs(id) ON DELETE CASCADE",
        ),
        con(
            "machine_principals.machine_principals_owner_principal_id_fkey",
            "f",
            "FOREIGN KEY (owner_principal_id) REFERENCES principals(id) ON DELETE SET NULL",
        ),
        con(
            "machine_principals.machine_principals_created_by_principal_id_fkey",
            "f",
            "FOREIGN KEY (created_by_principal_id) REFERENCES principals(id) ON DELETE SET NULL",
        ),
        con(
            "machine_principals.machine_principals_principal_id_key",
            "u",
            "UNIQUE (principal_id)",
        ),
        con(
            "machine_principals.machine_principals_org_id_slug_key",
            "u",
            "UNIQUE (org_id, slug)",
        ),
        con(
            "machine_principals.machine_principals_id_check",
            "c",
            "CHECK ((id ~ '^mch_[0-9a-f]{32}$'::text))",
        ),
        con(
            "machine_principals.machine_principals_slug_check",
            "c",
            "CHECK ((slug ~ '^[a-z0-9][a-z0-9_-]*$'::text))",
        ),
        con(
            "machine_principals.machine_principals_display_name_check",
            "c",
            "CHECK ((btrim(display_name) <> ''::text))",
        ),
        con("teams.teams_pkey", "p", "PRIMARY KEY (id)"),
        con(
            "teams.teams_org_id_fkey",
            "f",
            "FOREIGN KEY (org_id) REFERENCES orgs(id) ON DELETE CASCADE",
        ),
        con("teams.teams_org_id_slug_key", "u", "UNIQUE (org_id, slug)"),
        con("teams.teams_id_org_id_key", "u", "UNIQUE (id, org_id)"),
        con(
            "teams.teams_slug_check",
            "c",
            "CHECK ((slug ~ '^[a-z0-9][a-z0-9_-]*$'::text))",
        ),
        con(
            "team_memberships.team_memberships_pkey",
            "p",
            "PRIMARY KEY (team_id, user_id)",
        ),
        con(
            "team_memberships.team_memberships_team_id_org_id_fkey",
            "f",
            "FOREIGN KEY (team_id, org_id) REFERENCES teams(id, org_id) ON DELETE CASCADE",
        ),
        con(
            "team_memberships.team_memberships_org_id_user_id_fkey",
            "f",
            "FOREIGN KEY (org_id, user_id) REFERENCES org_members(org_id, user_id) ON DELETE CASCADE",
        ),
        con(
            "team_memberships.team_memberships_role_check",
            "c",
            "CHECK ((role = ANY (ARRAY['member'::text, 'team_admin'::text, 'lead'::text])))",
        ),
        con("archives.archives_pkey", "p", "PRIMARY KEY (id)"),
        con(
            "archives.archives_hash_algorithm_hash_hex_key",
            "u",
            "UNIQUE (hash_algorithm, hash_hex)",
        ),
        con(
            "archives.archives_storage_key_key",
            "u",
            "UNIQUE (storage_key)",
        ),
        con(
            "archives.archives_hash_algorithm_check",
            "c",
            "CHECK ((hash_algorithm = 'sha256'::text))",
        ),
        con(
            "archives.archives_size_bytes_check",
            "c",
            "CHECK (((size_bytes > 0) AND (size_bytes <= 52428800)))",
        ),
        con(
            "archives.archives_hash_hex_check",
            "c",
            "CHECK ((hash_hex ~ '^[0-9a-f]{64}$'::text))",
        ),
        con("skills.skills_pkey", "p", "PRIMARY KEY (id)"),
        con(
            "skills.skills_org_id_fkey",
            "f",
            "FOREIGN KEY (org_id) REFERENCES orgs(id) ON DELETE CASCADE",
        ),
        con(
            "skills.skills_owner_user_id_fkey",
            "f",
            "FOREIGN KEY (owner_user_id) REFERENCES users(id)",
        ),
        con(
            "skills.skills_org_id_owner_user_id_fkey",
            "f",
            "FOREIGN KEY (org_id, owner_user_id) REFERENCES org_members(org_id, user_id)",
        ),
        con(
            "skills.skills_team_id_org_id_fkey",
            "f",
            "FOREIGN KEY (team_id, org_id) REFERENCES teams(id, org_id)",
        ),
        con(
            "skills.skills_current_version_fk",
            "f",
            "FOREIGN KEY (current_version_id, id) REFERENCES skill_versions(id, skill_id)",
        ),
        con(
            "skills.skills_org_id_name_key",
            "u",
            "UNIQUE (org_id, name)",
        ),
        con("skills.skills_id_org_id_key", "u", "UNIQUE (id, org_id)"),
        con(
            "skills.skills_next_version_number_check",
            "c",
            "CHECK ((next_version_number > 0))",
        ),
        con(
            "skills.skills_visibility_check",
            "c",
            "CHECK ((visibility = ANY (ARRAY['private'::text, 'org'::text, 'team'::text])))",
        ),
        con(
            "skills.skills_name_check",
            "c",
            "CHECK ((name ~ '^[a-z0-9][a-z0-9_-]*$'::text))",
        ),
        con(
            "skills.skills_check",
            "c",
            "CHECK (((visibility = 'team'::text) = (team_id IS NOT NULL)))",
        ),
        con(
            "skill_versions.skill_versions_pkey",
            "p",
            "PRIMARY KEY (id)",
        ),
        con(
            "skill_versions.skill_versions_skill_id_fkey",
            "f",
            "FOREIGN KEY (skill_id) REFERENCES skills(id) ON DELETE CASCADE",
        ),
        con(
            "skill_versions.skill_versions_archive_id_fkey",
            "f",
            "FOREIGN KEY (archive_id) REFERENCES archives(id)",
        ),
        con(
            "skill_versions.skill_versions_published_by_user_id_fkey",
            "f",
            "FOREIGN KEY (published_by_user_id) REFERENCES users(id)",
        ),
        con(
            "skill_versions.skill_versions_approved_by_user_id_fkey",
            "f",
            "FOREIGN KEY (approved_by_user_id) REFERENCES users(id)",
        ),
        con(
            "skill_versions.skill_versions_yanked_by_user_id_fkey",
            "f",
            "FOREIGN KEY (yanked_by_user_id) REFERENCES users(id)",
        ),
        con(
            "skill_versions.skill_versions_deprecated_by_user_id_fkey",
            "f",
            "FOREIGN KEY (deprecated_by_user_id) REFERENCES users(id)",
        ),
        con(
            "skill_versions.skill_versions_skill_id_version_number_key",
            "u",
            "UNIQUE (skill_id, version_number)",
        ),
        con(
            "skill_versions.skill_versions_id_skill_id_key",
            "u",
            "UNIQUE (id, skill_id)",
        ),
        con(
            "skill_versions.skill_versions_version_number_check",
            "c",
            "CHECK ((version_number > 0))",
        ),
        con(
            "skill_versions.skill_versions_status_check",
            "c",
            "CHECK ((status = ANY (ARRAY['candidate'::text, 'approved'::text, 'rejected'::text])))",
        ),
        con(
            "skill_versions.skill_versions_check",
            "c",
            "CHECK (((status = 'approved'::text) = (approved_at IS NOT NULL)))",
        ),
        con(
            "skill_versions.skill_versions_check1",
            "c",
            "CHECK (((approved_at IS NULL) = (approved_by_user_id IS NULL)))",
        ),
        con(
            "skill_versions.skill_versions_check2",
            "c",
            "CHECK ((((yanked_at IS NULL) AND (yanked_by_user_id IS NULL) AND (yank_reason IS NULL)) OR ((yanked_at IS NOT NULL) AND (yanked_by_user_id IS NOT NULL) AND (yank_reason IS NOT NULL))))",
        ),
        con(
            "skill_versions.skill_versions_check3",
            "c",
            "CHECK ((((deprecated_at IS NULL) AND (deprecated_by_user_id IS NULL) AND (deprecation_reason IS NULL)) OR ((deprecated_at IS NOT NULL) AND (deprecated_by_user_id IS NOT NULL) AND (deprecation_reason IS NOT NULL))))",
        ),
        con(
            "skill_version_platform_tags.skill_version_platform_tags_pkey",
            "p",
            "PRIMARY KEY (skill_version_id, tag)",
        ),
        con(
            "skill_version_platform_tags.skill_version_platform_tags_skill_version_id_fkey",
            "f",
            "FOREIGN KEY (skill_version_id) REFERENCES skill_versions(id) ON DELETE CASCADE",
        ),
        con(
            "skill_version_platform_tags.skill_version_platform_tags_tag_check",
            "c",
            "CHECK ((tag ~ '^[a-z0-9][a-z0-9._-]*$'::text))",
        ),
        con("stacks.stacks_pkey", "p", "PRIMARY KEY (id)"),
        con(
            "stacks.stacks_org_id_fkey",
            "f",
            "FOREIGN KEY (org_id) REFERENCES orgs(id) ON DELETE CASCADE",
        ),
        con(
            "stacks.stacks_owner_user_id_fkey",
            "f",
            "FOREIGN KEY (owner_user_id) REFERENCES users(id)",
        ),
        con(
            "stacks.stacks_org_id_owner_user_id_fkey",
            "f",
            "FOREIGN KEY (org_id, owner_user_id) REFERENCES org_members(org_id, user_id)",
        ),
        con(
            "stacks.stacks_team_id_org_id_fkey",
            "f",
            "FOREIGN KEY (team_id, org_id) REFERENCES teams(id, org_id)",
        ),
        con(
            "stacks.stacks_org_id_slug_key",
            "u",
            "UNIQUE (org_id, slug)",
        ),
        con("stacks.stacks_id_org_id_key", "u", "UNIQUE (id, org_id)"),
        con(
            "stacks.stacks_slug_check",
            "c",
            "CHECK ((slug ~ '^[a-z0-9][a-z0-9_-]*$'::text))",
        ),
        con(
            "stacks.stacks_visibility_check",
            "c",
            "CHECK ((visibility = ANY (ARRAY['private'::text, 'org'::text, 'team'::text])))",
        ),
        con(
            "stacks.stacks_check",
            "c",
            "CHECK (((visibility = 'team'::text) = (team_id IS NOT NULL)))",
        ),
        con("stack_items.stack_items_pkey", "p", "PRIMARY KEY (id)"),
        con(
            "stack_items.stack_items_stack_id_fkey",
            "f",
            "FOREIGN KEY (stack_id) REFERENCES stacks(id) ON DELETE CASCADE",
        ),
        con(
            "stack_items.stack_items_skill_id_fkey",
            "f",
            "FOREIGN KEY (skill_id) REFERENCES skills(id)",
        ),
        con(
            "stack_items.stack_items_added_by_user_id_fkey",
            "f",
            "FOREIGN KEY (added_by_user_id) REFERENCES users(id)",
        ),
        con(
            "stack_items.stack_items_pinned_version_id_skill_id_fkey",
            "f",
            "FOREIGN KEY (pinned_version_id, skill_id) REFERENCES skill_versions(id, skill_id)",
        ),
        con(
            "stack_items.stack_items_stack_id_skill_id_key",
            "u",
            "UNIQUE (stack_id, skill_id)",
        ),
        con(
            "stack_items.stack_items_stack_id_position_key",
            "u",
            "UNIQUE (stack_id, \"position\")",
        ),
        con(
            "stack_items.stack_items_version_policy_check",
            "c",
            "CHECK ((version_policy = ANY (ARRAY['current'::text, 'pinned'::text])))",
        ),
        con(
            "stack_items.stack_items_position_check",
            "c",
            "CHECK ((\"position\" > 0))",
        ),
        con(
            "stack_items.stack_items_check",
            "c",
            "CHECK (((version_policy = 'pinned'::text) = (pinned_version_id IS NOT NULL)))",
        ),
        con("audit_log.audit_log_pkey", "p", "PRIMARY KEY (id)"),
        con(
            "audit_log.audit_log_org_id_fkey",
            "f",
            "FOREIGN KEY (org_id) REFERENCES orgs(id) ON DELETE SET NULL",
        ),
        con(
            "audit_log.audit_log_actor_user_id_fkey",
            "f",
            "FOREIGN KEY (actor_user_id) REFERENCES users(id) ON DELETE SET NULL",
        ),
        con(
            "audit_log.audit_log_actor_principal_id_fkey",
            "f",
            "FOREIGN KEY (actor_principal_id) REFERENCES principals(id) ON DELETE SET NULL",
        ),
        con(
            "audit_log.audit_log_actor_type_check",
            "c",
            "CHECK ((actor_type = ANY (ARRAY['human'::text, 'machine'::text])))",
        ),
        con(
            "audit_log.audit_log_resource_type_check",
            "c",
            "CHECK ((resource_type = ANY (ARRAY['org'::text, 'user'::text, 'token'::text, 'team'::text, 'skill'::text, 'stack'::text])))",
        ),
        con(
            "audit_log.audit_log_metadata_check",
            "c",
            "CHECK ((jsonb_typeof(metadata) = 'object'::text))",
        ),
    ]
}

async fn verify_team_role_constraint(pool: &DbPool) -> anyhow::Result<()> {
    let definition: Option<String> = sqlx::query_scalar(
        "SELECT pg_get_constraintdef(pg_constraint.oid)
         FROM pg_constraint
         JOIN pg_class ON pg_class.oid = pg_constraint.conrelid
         JOIN pg_namespace ON pg_namespace.oid = pg_class.relnamespace
         WHERE pg_namespace.nspname = 'public'
           AND pg_class.relname = 'team_memberships'
           AND pg_constraint.conname = 'team_memberships_role_check'",
    )
    .fetch_optional(pool)
    .await
    .context("failed to inspect team membership role constraint")?;

    let Some(definition) = definition else {
        bail!("missing team_memberships_role_check constraint; apply schema/postgres.sql");
    };
    if !definition.contains("team_admin") {
        bail!(
            "team_memberships_role_check does not allow `team_admin`; apply schema/20260517_team_admin_role.sql before starting this server"
        );
    }
    Ok(())
}
