//! The log of dreaming's runs: what each added, archived and appended, and undoing one.

use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;

use crate::store::{self, Error, State};

/// One logged run.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Dream {
    pub id: i64,
    pub bot: String,
    /// Unix milliseconds.
    pub at: i64,
    pub undone: bool,
    pub added: Vec<Change>,
    pub archived: Vec<Change>,
    /// The file versions it wrote.
    pub files: Vec<FileChange>,
}

/// A long-form file a run appended to.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct FileChange {
    pub name: String,
    pub shared: bool,
    pub version: i64,
}

/// An entry a run added or archived. Entries the user forgot since are left out.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Change {
    pub id: i64,
    pub text: String,
}

pub fn get(conn: &Connection, id: i64) -> store::Result<Option<Dream>> {
    let Some((bot, at, undone)) = conn
        .query_row("SELECT bot, at, undone FROM memory_dreams WHERE id = ?1", [id], |row| {
            Ok((row.get::<_, String>(0)?, row.get(1)?, row.get(2)?))
        })
        .optional()?
    else {
        return Ok(None);
    };
    let mut stmt = conn.prepare(
        "SELECT c.change, e.id, e.text FROM memory_dream_changes c JOIN memory_entries e ON e.id = c.entry
         WHERE c.dream = ?1 ORDER BY e.id",
    )?;
    let mut dream = Dream { id, bot, at, undone, added: Vec::new(), archived: Vec::new(), files: Vec::new() };
    for row in
        stmt.query_map([id], |row| Ok((row.get::<_, String>(0)?, Change { id: row.get(1)?, text: row.get(2)? })))?
    {
        let (kind, change) = row?;
        if kind == "added" { dream.added.push(change) } else { dream.archived.push(change) }
    }
    let mut stmt = conn.prepare(
        "SELECT v.name, v.bot = '', v.id FROM memory_dream_files d JOIN memory_file_versions v ON v.id = d.version
         WHERE d.dream = ?1 ORDER BY v.id",
    )?;
    for row in
        stmt.query_map([id], |row| Ok(FileChange { name: row.get(0)?, shared: row.get(1)?, version: row.get(2)? }))?
    {
        dream.files.push(row?);
    }
    Ok(Some(dream))
}

/// Logs a file version a run's note wrote.
pub fn record_file(conn: &Connection, dream: i64, version: i64) -> store::Result<()> {
    conn.execute("INSERT INTO memory_dream_files (dream, version) VALUES (?1, ?2)", params![dream, version])?;
    Ok(())
}

/// Drops a run that, once its notes were written or failed, changed nothing.
pub fn discard_if_empty(conn: &Connection, dream: i64) -> store::Result<()> {
    if get(conn, dream)?.is_some_and(|d| d.added.is_empty() && d.archived.is_empty() && d.files.is_empty()) {
        conn.execute("DELETE FROM memory_dreams WHERE id = ?1", [dream])?;
    }
    Ok(())
}

/// The bot's latest runs, newest first.
pub fn recent(conn: &Connection, bot: &str, limit: usize) -> store::Result<Vec<Dream>> {
    let mut stmt = conn.prepare("SELECT id FROM memory_dreams WHERE bot = ?1 ORDER BY at DESC, id DESC LIMIT ?2")?;
    let ids: Vec<i64> = stmt
        .query_map(params![bot, i64::try_from(limit).unwrap_or(i64::MAX)], |row| row.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    ids.into_iter().filter_map(|id| get(conn, id).transpose()).collect()
}

/// Reverses a run: what it added is forgotten (so it isn't learned again), what it archived comes back. The files
/// it appended to are the caller's to put back (`Dream::files`).
pub fn undo(conn: &Connection, id: i64, now: i64, rev: i64) -> store::Result<Dream> {
    let dream = get(conn, id)?.ok_or(Error::NotFound)?;
    if dream.undone {
        return Ok(dream);
    }
    for change in &dream.added {
        store::forget(conn, change.id)?;
    }
    for change in &dream.archived {
        if store::get(conn, change.id)?.is_some_and(|e| e.state == State::Archived) {
            store::bring_back(conn, change.id, now, rev)?;
        }
    }
    conn.execute("UPDATE memory_dreams SET undone = 1 WHERE id = ?1", [id])?;
    get(conn, id)?.ok_or(Error::NotFound)
}
