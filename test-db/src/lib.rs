//! Fast per-test MySQL databases for `cargo test`.
//!
//! `#[sqlx::test(migrations = "../migrations")]` ran all ~200 migrations (and
//! the seed) into a new database for every test, then dropped it: about a
//! second per test on a local server, far more against a remote one. Here:
//!
//! 1. The migrations (and the seed) run once into a template database named
//!    after a hash of their contents, so it is reused by every test binary and
//!    every run until a migration or the seed changes.
//! 2. A test runs on a copy of the template. Copies stay on the server between
//!    runs (`_test_db_c_<template>_<n>`): a run makes a new one only when more
//!    tests run at once than there are copies. Making copies (each one script
//!    of ~200 `CREATE TABLE`s and `INSERT ... SELECT`s) used to take most of a
//!    run of a few tests, since every run made its own.
//! 3. A test claims its copy with a MySQL named lock that it holds while it
//!    runs, so no two tests share one, whatever process, test binary or
//!    worktree they run in.
//! 4. Before a test uses a copy, the copy is made to match the template again:
//!    tables whose `CHECKSUM TABLE` or `AUTO_INCREMENT` differ from the
//!    template's are refilled, and a copy whose tables were dropped or added
//!    is made anew. The first failing tests of a process keep their copies
//!    locked, as the tests left them, until the process ends, and print the
//!    copies' names for inspection.
//! 5. Templates no run has used for a week are dropped with their copies.
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
const TEMPLATE_PREFIX: &str = "_test_db_tpl_";
const COPY_PREFIX: &str = "_test_db_c_";
/// Copies of the previous test-db, one set per test process (`<pid>_<n>`).
const PROCESS_COPY_PREFIX: &str = "_test_db_copy_";
/// When each template was last used (`templates`), to drop unused ones.
const CONTROL_DATABASE: &str = "_test_db";
const UNUSED_TEMPLATE_DAYS: u32 = 7;
/// Table definitions (and open tables) the server keeps: every copy's ~200
/// tables. With the default 2000, checking copies evicts the tables running
/// tests use, and their prepared statements fail ("Prepared statement needs
/// to be re-prepared").
const TABLE_CACHE: u32 = 20_000;
/// Failed tests' copies a process keeps locked for inspection: each holds a
/// connection, and a run where everything fails must not use them all up.
const KEPT_FAILURES: usize = 8;

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
    /// Copies this process's tests released: tried before the others.
    released: Mutex<Vec<usize>>,
}

impl TemplateState {
    const fn new() -> Self {
        Self {
            plan: OnceCell::const_new(),
            released: Mutex::new(Vec::new()),
        }
    }
}

/// What it takes to copy the template and to tell whether a copy still matches it.
struct Plan {
    template: String,
    /// Creates every table and copies its rows into the current database.
    script: String,
    tables: HashMap<String, TableState>,
    /// Views, triggers, routines and events (a test may create a trigger).
    other_objects: Vec<String>,
}

#[derive(PartialEq, Eq)]
struct TableState {
    checksum: Option<i64>,
    auto_increment: Option<u64>,
}

/// What the names of `template`'s copies start with.
fn copy_prefix(template: &str) -> String {
    let suffix = template.strip_prefix(TEMPLATE_PREFIX).unwrap_or(template);
    format!("{COPY_PREFIX}{suffix}_")
}

/// The name of `template`'s `index`th copy.
fn copy_name(template: &str, index: usize) -> String {
    format!("{}{index}", copy_prefix(template))
}

/// A copy a test holds, and the connection holding its lock.
struct Claim {
    database: String,
    index: usize,
    lock: Option<MySqlConnection>,
}

impl Drop for Claim {
    /// After a panic (the test failed) the lock's connection is leaked, so
    /// the copy stays locked, as the test left it, until the process ends;
    /// past [`KEPT_FAILURES`] the lock is let go with the connection.
    fn drop(&mut self) {
        static KEPT: AtomicUsize = AtomicUsize::new(0);
        if std::thread::panicking()
            && KEPT.fetch_add(1, Ordering::Relaxed) < KEPT_FAILURES
            && let Some(lock) = self.lock.take()
        {
            eprintln!(
                "test-db: the failed test's database `{}` stays as the test left it \
                 until the next run claims it",
                self.database
            );
            std::mem::forget(lock);
        }
    }
}

/// Runs one test on its own copy of `template`. Used by `#[test_db::test]`;
/// the test panics on failure like any `#[test]`.
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

            let mut claim = claim(&server, state, plan)
                .await
                .expect("claim a test database");
            let pool = MySqlPoolOptions::new()
                .connect_with(server.clone().database(&claim.database))
                .await
                .expect("connect to the test database");

            test(pool.clone()).await;
            pool.close().await;

            let mut lock = claim.lock.take().expect("the claim's lock");
            sqlx::query("SELECT RELEASE_LOCK(?)")
                .bind(&claim.database)
                .execute(&mut lock)
                .await
                .expect("release the test database");
            lock.close().await.expect("close the lock connection");
            state
                .released
                .lock()
                .expect("released copies")
                .push(claim.index);
        });
}

fn server_options() -> MySqlConnectOptions {
    let url = std::env::var("DATABASE_URL").expect("DATABASE_URL must name the test MySQL server");
    MySqlConnectOptions::from_str(&url).expect("DATABASE_URL is a MySQL URL")
}

/// Locks the first free copy (the ones this process released first) and
/// makes it match the template, or makes a new copy when all are taken.
async fn claim(
    server: &MySqlConnectOptions,
    state: &TemplateState,
    plan: &Plan,
) -> Result<Claim, sqlx::Error> {
    let mut lock = MySqlConnection::connect_with(server).await?;
    let mut next = 0;
    loop {
        let released = state.released.lock().expect("released copies").pop();
        let index = released.unwrap_or_else(|| {
            next += 1;
            next - 1
        });
        let database = copy_name(&plan.template, index);
        let locked: Option<i64> = sqlx::query_scalar("SELECT GET_LOCK(?, 0)")
            .bind(&database)
            .fetch_one(&mut lock)
            .await?;
        if locked != Some(1) {
            continue;
        }
        if !restore(&mut lock, plan, &database).await? {
            create(&mut lock, plan, &database).await?;
        }
        return Ok(Claim {
            database,
            index,
            lock: Some(lock),
        });
    }
}

/// Replaces `database` with a new copy of the template.
async fn create(
    conn: &mut MySqlConnection,
    plan: &Plan,
    database: &str,
) -> Result<(), sqlx::Error> {
    sqlx::raw_sql(AssertSqlSafe(format!(
        "DROP DATABASE IF EXISTS `{database}`; CREATE DATABASE `{database}`; USE `{database}`; {}",
        plan.script
    )))
    .execute(conn)
    .await?;
    Ok(())
}

/// Makes `database` match the template again. Returns false when it does not
/// exist or its tables, views, triggers or routines are not the template's.
async fn restore(
    conn: &mut MySqlConnection,
    plan: &Plan,
    database: &str,
) -> Result<bool, sqlx::Error> {
    let tables = table_states(conn, database).await?;
    let same_tables =
        tables.len() == plan.tables.len() && tables.keys().all(|t| plan.tables.contains_key(t));
    if !same_tables || other_objects(conn, database).await? != plan.other_objects {
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
            "DELETE FROM `{table}`; INSERT INTO `{table}` SELECT * FROM `{}`.`{table}`;",
            plan.template
        )
        .expect("write to String");
        if let Some(next) = expected.auto_increment {
            write!(script, "ALTER TABLE `{table}` AUTO_INCREMENT = {next};")
                .expect("write to String");
        }
    }
    if dirty {
        script.push_str("SET FOREIGN_KEY_CHECKS = 1;");
        sqlx::raw_sql(AssertSqlSafe(script)).execute(conn).await?;
    }
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
    raise_table_caches(&mut conn).await;
    drop_process_copies(&mut conn).await?;
    ensure_template(&mut conn, template, &name).await?;
    drop_unused_templates(&mut conn).await?;

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
    for table in names {
        let row = conn
            .fetch_one(AssertSqlSafe(format!(
                "SHOW CREATE TABLE `{name}`.`{table}`"
            )))
            .await?;
        let ddl: String = row.try_get(1)?;
        writeln!(script, "{ddl};").expect("write to String");
        writeln!(
            script,
            "INSERT INTO `{table}` SELECT * FROM `{name}`.`{table}`;"
        )
        .expect("write to String");
    }
    script.push_str("SET FOREIGN_KEY_CHECKS = 1;\n");
    conn.close().await?;

    Ok(Plan {
        template: name,
        script,
        tables,
        other_objects,
    })
}

/// Creates the template database unless a finished one with this name
/// exists, and records that it is in use. A MySQL named lock keeps parallel
/// test binaries (or worktrees sharing the server) from building it twice.
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

    sqlx::raw_sql(AssertSqlSafe(format!(
        "CREATE DATABASE IF NOT EXISTS `{CONTROL_DATABASE}`; \
         CREATE TABLE IF NOT EXISTS `{CONTROL_DATABASE}`.templates \
           (name VARCHAR(64) PRIMARY KEY, used_at DATETIME NOT NULL)"
    )))
    .execute(&mut *conn)
    .await?;
    sqlx::query(AssertSqlSafe(format!(
        "INSERT INTO `{CONTROL_DATABASE}`.templates (name, used_at) VALUES (?, UTC_TIMESTAMP()) \
         ON DUPLICATE KEY UPDATE used_at = UTC_TIMESTAMP()"
    )))
    .bind(name)
    .execute(&mut *conn)
    .await?;

    sqlx::query("SELECT RELEASE_LOCK(?)")
        .bind(name)
        .execute(&mut *conn)
        .await?;
    Ok(())
}

/// Drops the templates no run has used for [`UNUSED_TEMPLATE_DAYS`] days,
/// and their copies no test holds. A template of an older test-db (not in
/// the `templates` table) counts as used now.
async fn drop_unused_templates(conn: &mut MySqlConnection) -> Result<(), sqlx::Error> {
    sqlx::query(AssertSqlSafe(format!(
        "INSERT IGNORE INTO `{CONTROL_DATABASE}`.templates (name, used_at) \
         SELECT schema_name, UTC_TIMESTAMP() FROM information_schema.schemata \
          WHERE schema_name LIKE ?"
    )))
    .bind(like_prefix(TEMPLATE_PREFIX))
    .execute(&mut *conn)
    .await?;
    let unused: Vec<String> = sqlx::query_scalar(AssertSqlSafe(format!(
        "SELECT name FROM `{CONTROL_DATABASE}`.templates \
          WHERE used_at < UTC_TIMESTAMP() - INTERVAL {UNUSED_TEMPLATE_DAYS} DAY"
    )))
    .fetch_all(&mut *conn)
    .await?;
    for template in unused {
        for copy in databases_like(conn, &like_prefix(&copy_prefix(&template))).await? {
            if lock_now(conn, &copy).await? {
                drop_database(conn, &copy).await?;
                release(conn, &copy).await?;
            }
        }
        if lock_now(conn, &template).await? {
            drop_database(conn, &template).await?;
            sqlx::query(AssertSqlSafe(format!(
                "DELETE FROM `{CONTROL_DATABASE}`.templates WHERE name = ?"
            )))
            .bind(&template)
            .execute(&mut *conn)
            .await?;
            release(conn, &template).await?;
        }
    }
    Ok(())
}

/// Raises the server's table caches to [`TABLE_CACHE`] (they reset when it
/// restarts). A user without the privilege keeps the server's own sizes.
async fn raise_table_caches(conn: &mut MySqlConnection) {
    let raised = async {
        let (definitions, open): (u64, u64) =
            sqlx::query_as("SELECT @@GLOBAL.table_definition_cache, @@GLOBAL.table_open_cache")
                .fetch_one(&mut *conn)
                .await?;
        for (variable, size) in [
            ("table_definition_cache", definitions),
            ("table_open_cache", open),
        ] {
            if size < u64::from(TABLE_CACHE) {
                conn.execute(AssertSqlSafe(format!(
                    "SET GLOBAL {variable} = {TABLE_CACHE}"
                )))
                .await?;
            }
        }
        Ok::<_, sqlx::Error>(())
    }
    .await;
    if let Err(error) = raised {
        eprintln!(
            "test-db: the table caches stay small ({error}): tests may fail with \
             \"Prepared statement needs to be re-prepared\""
        );
    }
}

/// Drops the copies of the previous test-db (one set per test process)
/// whose process is no longer running.
async fn drop_process_copies(conn: &mut MySqlConnection) -> Result<(), sqlx::Error> {
    for database in databases_like(conn, &like_prefix(PROCESS_COPY_PREFIX)).await? {
        let pid = database[PROCESS_COPY_PREFIX.len()..]
            .split('_')
            .next()
            .and_then(|pid| pid.parse::<u32>().ok());
        if pid.is_some_and(process_is_running) {
            continue;
        }
        drop_database(conn, &database).await?;
    }
    Ok(())
}

/// A `LIKE` pattern matching every name that starts with `prefix`.
fn like_prefix(prefix: &str) -> String {
    format!("{}%", prefix.replace('_', "\\_"))
}

async fn databases_like(
    conn: &mut MySqlConnection,
    pattern: &str,
) -> Result<Vec<String>, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT schema_name FROM information_schema.schemata WHERE schema_name LIKE ?",
    )
    .bind(pattern)
    .fetch_all(conn)
    .await
}

/// Takes the named lock `name` if no other session holds it.
async fn lock_now(conn: &mut MySqlConnection, name: &str) -> Result<bool, sqlx::Error> {
    let locked: Option<i64> = sqlx::query_scalar("SELECT GET_LOCK(?, 0)")
        .bind(name)
        .fetch_one(conn)
        .await?;
    Ok(locked == Some(1))
}

async fn release(conn: &mut MySqlConnection, name: &str) -> Result<(), sqlx::Error> {
    sqlx::query("SELECT RELEASE_LOCK(?)")
        .bind(name)
        .execute(conn)
        .await?;
    Ok(())
}

async fn drop_database(conn: &mut MySqlConnection, database: &str) -> Result<(), sqlx::Error> {
    conn.execute(AssertSqlSafe(format!(
        "DROP DATABASE IF EXISTS `{database}`"
    )))
    .await?;
    Ok(())
}

fn process_is_running(pid: u32) -> bool {
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}
