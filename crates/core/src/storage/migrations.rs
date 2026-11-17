//! Embedded, ordered schema migrations tracked with `PRAGMA user_version`.

use rusqlite::Connection;

/// Migration scripts; index `i` upgrades the schema from version `i` to `i + 1`.
///
/// Pre-release there is exactly one migration: schema changes are made in place
/// in `0001_init.sql` and the dev database is reset. After the
/// first released version this becomes append-only (never edit a released one).
const MIGRATIONS: &[&str] = &[include_str!("migrations/0001_init.sql")];

/// Schema version after all migrations ran.
pub const SCHEMA_VERSION: u32 = {
    assert!(MIGRATIONS.len() < u32::MAX as usize);
    #[expect(
        clippy::cast_possible_truncation,
        reason = "bounded by the assertion above"
    )]
    let version = MIGRATIONS.len() as u32;
    version
};

/// Reads the current schema version.
pub fn current_version(conn: &Connection) -> rusqlite::Result<u32> {
    conn.pragma_query_value(None, "user_version", |row| row.get(0))
}

/// Applies all pending migrations, each in its own transaction.
pub fn migrate(conn: &mut Connection) -> rusqlite::Result<()> {
    let current = current_version(conn)?;
    for (version, sql) in (1..=SCHEMA_VERSION).zip(MIGRATIONS).skip(current as usize) {
        let tx = conn.transaction()?;
        tx.execute_batch(sql)?;
        tx.pragma_update(None, "user_version", version)?;
        tx.commit()?;
        tracing::info!(version, "applied database migration");
    }
    Ok(())
}
