//! Fast per-test MySQL databases for `cargo test`.
//!
//! `#[sqlx::test(migrations = "../migrations")]` ran all ~200 migrations (and
//! the seed) into a new database for every test, then dropped it: about a
//! second per test on a local server, far more against a remote one. Here:
//!
//! 1. The migrations (and the seed) run once into a template database named
//!    after a hash of their contents, so it is reused by every test binary and
//!    every run until a migration or the seed changes.
//! 2. A test takes a database from a per-process pool of copies of the
//!    template. A new copy is one multi-statement script (`CREATE TABLE`s and
//!    `INSERT ... SELECT`s of the stored, not generated, columns).
//! 3. After a passing test the copy goes back to the pool once it matches the
//!    template again: tables whose `CHECKSUM TABLE` or `AUTO_INCREMENT` differ
//!    from the template's are refilled. A copy whose tables were dropped or
//!    added is dropped instead. A failing test leaves its database behind for
//!    inspection; the next run drops it.
//!
//! ```ignore
//! #[test_db::test]          // every migration
//! async fn a(pool: MySqlPool) {}
//!
//! #[test_db::test(seed)]    // every migration + api/seed/local_seed.sql
//! async fn b(pool: MySqlPool) {}
//! ```
//!
//! The server is the one in `DATABASE_URL` (see `.cargo/config.toml`).

use std::collections::HashMap;
use std::fmt::Write as _;
use std::future::Future;
use std::str::FromStr;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use sha2::{Digest, Sha256};
use sqlx::migrate::Migrator;
use sqlx::mysql::{MySqlConnectOptions, MySqlPoolOptions};
use sqlx::{AssertSqlSafe, Connection, Executor, MySqlConnection, MySqlPool, Row};
use tokio::sync::OnceCell;

pub use test_db_macros::test;

static MIGRATOR: Migrator = sqlx::migrate!("../migrations");
const SEED: &str = include_str!("../../api/seed/local_seed.sql");
const READY_TABLE: &str = "_test_db_ready";
const COPY_PREFIX: &str = "_test_db_copy_";
const TEMPLATE_PREFIX: &str = "_test_db_tpl_";

/// What the template database contains.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Template {
    /// Every migration in `migrations/`.
    Migrated,
    /// Every migration plus `api/seed/local_seed.sql`.
    Seeded,
}

impl Template {
    fn name(self) -> String {
        let mut hash = Sha256::new();
        for migration in MIGRATOR.iter() {
            hash.update(migration.version.to_le_bytes());
            hash.update(&*migration.checksum);
        }
        let prefix = match self {
            Self::Migrated => "m",
            Self::Seeded => {
                hash.update(SEED.as_bytes());
                "s"
            }
        };
        let digest = hash.finalize();
        let mut name = format!("{TEMPLATE_PREFIX}{prefix}_");
        for byte in &digest[..6] {
            write!(name, "{byte:02x}").expect("write to String");
        }
        name
    }

    fn state(self) -> &'static TemplateState {
        static MIGRATED: TemplateState = TemplateState::new();
        static SEEDED: TemplateState = TemplateState::new();
        match self {
            Self::Migrated => &MIGRATED,
            Self::Seeded => &SEEDED,
        }
    }
}

struct TemplateState {
    plan: OnceCell<Plan>,
    /// Copies that match the template and no running test uses.
    free: Mutex<Vec<String>>,
}

impl TemplateState {
    const fn new() -> Self {
        Self {
            plan: OnceCell::const_new(),
            free: Mutex::new(Vec::new()),
        }
    }
}

/// What it takes to copy the template and to tell whether a copy still matches it.
struct Plan {
    template: String,
    /// Creates every table and copies its rows into the current database.
    script: String,
    tables: HashMap<String, TableState>,
    /// Per table, its stored (not generated) columns: the copy's column list.
    columns: HashMap<String, String>,
    /// Views, triggers, routines and events (a test may create a trigger).
    other_objects: Vec<String>,
}

#[derive(PartialEq, Eq)]
struct TableState {
    checksum: Option<i64>,
    auto_increment: Option<u64>,
}

/// Runs one test on its own database copied from `template`. Used by
/// `#[test_db::test]`; the test panics on failure like any `#[test]`.
///
/// # Panics
///
/// When the database cannot be prepared, or when the test itself panics.
pub fn run<F, Fut>(template: Template, test: F)
where
    F: FnOnce(MySqlPool) -> Fut,
    Fut: Future<Output = ()>,
{
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build the test runtime")
        .block_on(async {
            let server = server_options();
            let state = template.state();
            let plan = state
                .plan
                .get_or_try_init(|| build_plan(&server, template))
                .await
                .expect("build the test database template");

            let database = checkout(&server, state, plan)
                .await
                .expect("create a test database");
            let pool = MySqlPoolOptions::new()
                .connect_with(server.clone().database(&database))
                .await
                .expect("connect to the test database");

            // A panic here skips the rest: the database stays for inspection.
            test(pool.clone()).await;
            pool.close().await;

            let reusable = restore(&server, plan, &database)
                .await
                .expect("reset the test database");
            if reusable {
                state.free.lock().expect("free list").push(database);
            }
        });
}

fn server_options() -> MySqlConnectOptions {
    let url = std::env::var("DATABASE_URL").expect("DATABASE_URL must name the test MySQL server");
    MySqlConnectOptions::from_str(&url).expect("DATABASE_URL is a MySQL URL")
}

/// A free copy of the template, or a new one.
async fn checkout(
    server: &MySqlConnectOptions,
    state: &TemplateState,
    plan: &Plan,
) -> Result<String, sqlx::Error> {
    if let Some(database) = state.free.lock().expect("free list").pop() {
        return Ok(database);
    }
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let database = format!(
        "{COPY_PREFIX}{}_{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    );
    let mut conn = MySqlConnection::connect_with(server).await?;
    sqlx::raw_sql(AssertSqlSafe(format!(
        "DROP DATABASE IF EXISTS `{database}`; CREATE DATABASE `{database}`; USE `{database}`; {}",
        plan.script
    )))
    .execute(&mut conn)
    .await?;
    conn.close().await?;
    Ok(database)
}

/// Makes `database` match the template again. Returns false (and drops the
/// database) when its tables, views, triggers or routines are not the template's.
async fn restore(
    server: &MySqlConnectOptions,
    plan: &Plan,
    database: &str,
) -> Result<bool, sqlx::Error> {
    let mut conn = MySqlConnection::connect_with(server).await?;
    let tables = table_states(&mut conn, database).await?;

    let same_tables =
        tables.len() == plan.tables.len() && tables.keys().all(|t| plan.tables.contains_key(t));
    if !same_tables || other_objects(&mut conn, database).await? != plan.other_objects {
        conn.execute(AssertSqlSafe(format!("DROP DATABASE `{database}`")))
            .await?;
        conn.close().await?;
        return Ok(false);
    }

    let mut script = format!("USE `{database}`; SET FOREIGN_KEY_CHECKS = 0;");
    let mut dirty = false;
    for (table, state) in &tables {
        let expected = &plan.tables[table];
        if state == expected {
            continue;
        }
        dirty = true;
        write!(
            script,
            "DELETE FROM `{table}`; INSERT INTO `{table}` ({columns}) SELECT {columns} FROM `{}`.`{table}`;",
            plan.template,
            columns = plan.columns[table],
        )
        .expect("write to String");
        if let Some(next) = expected.auto_increment {
            write!(script, "ALTER TABLE `{table}` AUTO_INCREMENT = {next};")
                .expect("write to String");
        }
    }
    if dirty {
        script.push_str("SET FOREIGN_KEY_CHECKS = 1;");
        sqlx::raw_sql(AssertSqlSafe(script))
            .execute(&mut conn)
            .await?;
    }
    conn.close().await?;
    Ok(true)
}

/// Names of everything in `database` that is not a base table.
async fn other_objects(
    conn: &mut MySqlConnection,
    database: &str,
) -> Result<Vec<String>, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT CONCAT('view ', table_name) FROM information_schema.tables \
           WHERE table_schema = ? AND table_type <> 'BASE TABLE' \
         UNION ALL SELECT CONCAT('trigger ', trigger_name) FROM information_schema.triggers \
           WHERE trigger_schema = ? \
         UNION ALL SELECT CONCAT('routine ', routine_name) FROM information_schema.routines \
           WHERE routine_schema = ? \
         UNION ALL SELECT CONCAT('event ', event_name) FROM information_schema.events \
           WHERE event_schema = ? \
         ORDER BY 1",
    )
    .bind(database)
    .bind(database)
    .bind(database)
    .bind(database)
    .fetch_all(conn)
    .await
}

/// Checksum and next `AUTO_INCREMENT` of every table in `database`.
async fn table_states(
    conn: &mut MySqlConnection,
    database: &str,
) -> Result<HashMap<String, TableState>, sqlx::Error> {
    // information_schema caches AUTO_INCREMENT for a day by default.
    conn.execute("SET SESSION information_schema_stats_expiry = 0")
        .await?;
    let rows: Vec<(String, Option<u64>)> = sqlx::query_as(
        "SELECT table_name, auto_increment FROM information_schema.tables \
         WHERE table_schema = ? AND table_type = 'BASE TABLE' AND table_name <> ?",
    )
    .bind(database)
    .bind(READY_TABLE)
    .fetch_all(&mut *conn)
    .await?;
    if rows.is_empty() {
        return Ok(HashMap::new());
    }

    let list = rows
        .iter()
        .map(|(table, _)| format!("`{database}`.`{table}`"))
        .collect::<Vec<_>>()
        .join(", ");
    let checksums: HashMap<String, Option<i64>> = conn
        .fetch_all(AssertSqlSafe(format!("CHECKSUM TABLE {list}")))
        .await?
        .into_iter()
        .map(|row| {
            let qualified: String = row.get(0);
            let table = qualified
                .split_once('.')
                .map_or(qualified.clone(), |(_, table)| table.to_string());
            (table, row.get(1))
        })
        .collect();

    Ok(rows
        .into_iter()
        .map(|(table, auto_increment)| {
            let checksum = checksums.get(&table).copied().flatten();
            (
                table,
                TableState {
                    checksum,
                    auto_increment,
                },
            )
        })
        .collect())
}

/// Builds the template if needed and reads what copying it takes.
async fn build_plan(server: &MySqlConnectOptions, template: Template) -> Result<Plan, sqlx::Error> {
    let name = template.name();
    let mut conn = MySqlConnection::connect_with(server).await?;
    drop_dead_copies(&mut conn).await?;
    ensure_template(&mut conn, template, &name).await?;

    let tables = table_states(&mut conn, &name).await?;
    let other_objects = other_objects(&mut conn, &name).await?;
    // The copy script only copies tables.
    assert!(
        other_objects.is_empty(),
        "migrations created {other_objects:?}; test-db copies only tables"
    );
    let mut names: Vec<&String> = tables.keys().collect();
    names.sort();

    let mut script = String::from("SET FOREIGN_KEY_CHECKS = 0;\n");
    let mut columns = HashMap::new();
    for table in names {
        let row = conn
            .fetch_one(AssertSqlSafe(format!(
                "SHOW CREATE TABLE `{name}`.`{table}`"
            )))
            .await?;
        let ddl: String = row.try_get(1)?;
        writeln!(script, "{ddl};").expect("write to String");
        // A generated column cannot be inserted into.
        let stored: Vec<String> = sqlx::query_scalar(
            "SELECT CAST(CONCAT('`', COLUMN_NAME, '`') AS CHAR) FROM information_schema.COLUMNS
              WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ? AND GENERATION_EXPRESSION = ''
              ORDER BY ORDINAL_POSITION",
        )
        .bind(&name)
        .bind(table)
        .fetch_all(&mut conn)
        .await?;
        let list = stored.join(", ");
        writeln!(
            script,
            "INSERT INTO `{table}` ({list}) SELECT {list} FROM `{name}`.`{table}`;"
        )
        .expect("write to String");
        columns.insert(table.clone(), list);
    }
    script.push_str("SET FOREIGN_KEY_CHECKS = 1;\n");
    conn.close().await?;

    Ok(Plan {
        template: name,
        script,
        tables,
        columns,
        other_objects,
    })
}

/// Creates the template database unless a finished one with this name exists.
/// A MySQL named lock keeps parallel test binaries (or worktrees sharing the
/// server) from building it twice.
async fn ensure_template(
    conn: &mut MySqlConnection,
    template: Template,
    name: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query("SELECT GET_LOCK(?, 600)")
        .bind(name)
        .execute(&mut *conn)
        .await?;

    let ready: Option<i32> = sqlx::query_scalar(
        "SELECT 1 FROM information_schema.tables WHERE table_schema = ? AND table_name = ?",
    )
    .bind(name)
    .bind(READY_TABLE)
    .fetch_optional(&mut *conn)
    .await?;

    if ready.is_none() {
        sqlx::raw_sql(AssertSqlSafe(format!(
            "DROP DATABASE IF EXISTS `{name}`; CREATE DATABASE `{name}`; USE `{name}`"
        )))
        .execute(&mut *conn)
        .await?;
        MIGRATOR
            .run(&mut *conn)
            .await
            .map_err(|error| match error {
                sqlx::migrate::MigrateError::Execute(error) => error,
                other => sqlx::Error::Protocol(other.to_string()),
            })?;
        if template == Template::Seeded {
            sqlx::raw_sql(SEED).execute(&mut *conn).await?;
        }
        conn.execute(AssertSqlSafe(format!(
            "CREATE TABLE `{READY_TABLE}` (id INT)"
        )))
        .await?;
    }

    sqlx::query("SELECT RELEASE_LOCK(?)")
        .bind(name)
        .execute(&mut *conn)
        .await?;
    Ok(())
}

/// Drops copies left by test processes that are no longer running (failed
/// tests, or every copy of a finished run).
async fn drop_dead_copies(conn: &mut MySqlConnection) -> Result<(), sqlx::Error> {
    let databases: Vec<String> = sqlx::query_scalar(
        "SELECT schema_name FROM information_schema.schemata WHERE schema_name LIKE ?",
    )
    .bind(format!("{}%", COPY_PREFIX.replace('_', "\\_")))
    .fetch_all(&mut *conn)
    .await?;
    for database in databases {
        let pid = database[COPY_PREFIX.len()..]
            .split('_')
            .next()
            .and_then(|pid| pid.parse::<u32>().ok());
        if pid.is_some_and(process_is_running) {
            continue;
        }
        conn.execute(AssertSqlSafe(format!(
            "DROP DATABASE IF EXISTS `{database}`"
        )))
        .await?;
    }
    Ok(())
}

fn process_is_running(pid: u32) -> bool {
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}
