//! SQLite pool construction and migration running.
//!
//! # SQLite transaction behaviour
//!
//! Worth knowing, because it differs from Postgres and MySQL:
//!
//! * SQLite allows many concurrent readers, but only one writer at a time, database-wide.
//! * A plain `BEGIN` — which is what `pool.begin()` issues — starts a *deferred*
//!   transaction. It takes no lock when it opens, and acquires the write lock only when that
//!   transaction first writes.
//! * `busy_timeout` (5s, set below) makes a writer that finds the lock held wait for it
//!   instead of failing immediately with `SQLITE_BUSY`.
//!
//! Plan your transactions accordingly.

use std::path::Path;
use std::time::Duration;

use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};
use sqlx::SqlitePool;

/// Builds the connection pool, creating the database file (and its parent directory) if
/// it does not exist yet.
pub async fn connect(database_url: &str) -> Result<SqlitePool, sqlx::Error> {
    let options: SqliteConnectOptions = database_url.parse()?;

    // SQLite will happily create `booking.db` but not the `data/` directory holding it.
    if let Some(parent) = options.get_filename().parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).map_err(sqlx::Error::Io)?;
        }
    }

    let options = options
        .create_if_missing(true)
        // Write-ahead logging: readers do not block the writer.
        .journal_mode(SqliteJournalMode::Wal)
        // Wait for the write lock instead of failing immediately with SQLITE_BUSY.
        .busy_timeout(Duration::from_secs(5))
        .synchronous(SqliteSynchronous::Normal)
        .foreign_keys(true);

    SqlitePoolOptions::new()
        .max_connections(5)
        .acquire_timeout(Duration::from_secs(10))
        .connect_with(options)
        .await
}

/// Applies every `*.sql` file in `dir` that has not been applied yet.
///
/// Migrations are read from disk at runtime (rather than embedded with the `migrate!`
/// macro) so that adding `0002_bookings.sql` takes effect on the next restart with no
/// rebuild and no stale-cache surprises.
pub async fn migrate(pool: &SqlitePool, dir: &str) -> Result<(), sqlx::Error> {
    let path = Path::new(dir);
    if !path.is_dir() {
        return Err(sqlx::Error::Configuration(
            format!(
                "migrations directory {:?} not found — run from the workspace root, or set \
                 MIGRATIONS_DIR",
                path
            )
            .into(),
        ));
    }

    let migrator = sqlx::migrate::Migrator::new(path).await?;

    match migrator.run(pool).await {
        Ok(()) => Ok(()),
        Err(error) => Err(explain_migration_failure(error)),
    }
}

/// Turns sqlx's terser migration errors into something you can act on.
///
/// sqlx records a checksum of every migration it applies and refuses to run if one has since
/// changed. That is correct, and it is also the single most likely thing to stop this app
/// booting: you write `0002_bookings.sql`, start the app, realise you need another column, edit
/// the file, and it will not start. Renaming or renumbering an applied file does the same.
///
/// All of those have the same fix here, so say so rather than making anyone go and read the
/// sqlx docs mid-flow.
fn explain_migration_failure(error: sqlx::migrate::MigrateError) -> sqlx::Error {
    use sqlx::migrate::MigrateError;

    let hint = match &error {
        MigrateError::VersionMismatch(version) => Some(format!(
            "migration {version} was already applied and has since been edited"
        )),
        MigrateError::VersionMissing(version) => Some(format!(
            "migration {version} was already applied but its file is now missing or renamed"
        )),
        MigrateError::VersionTooOld(version, latest) => Some(format!(
            "migration {version} is numbered below the already-applied {latest}"
        )),
        MigrateError::Dirty(version) => {
            Some(format!("migration {version} was only partially applied"))
        }
        _ => None,
    };

    match hint {
        Some(hint) => sqlx::Error::Configuration(
            format!(
                "{hint}.\n\n\
                 Applied migrations are checksummed, so they cannot be changed in place. Wipe \
                 the database and reapply everything from scratch:\n\n    \
                 ./reset.sh\n\n\
                 That deletes data/ — which holds nothing but the seeded reference data and \
                 whatever you have booked while testing, all of which is recreated on the next \
                 start."
            )
            .into(),
        ),
        None => error.into(),
    }
}
