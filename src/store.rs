//! Entries: adding, revising, archiving, forgetting and reading memory.
//!
//! Every function takes the caller's connection (or transaction) plus the time and `rev` to stamp, so the change
//! commits with whatever the caller records alongside it.

use rusqlite::{Connection, OptionalExtension, Row, params};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::fmt::Write as _;

use crate::check::{self, Refusal};
use crate::tokens;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum State {
    Active,
    /// Out of the opening, still searchable.
    Archived,
}

/// Who wrote an entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Author {
    User,
    Bot,
    Dream,
    /// Brought over from an earlier memory store.
    Imported,
}

/// Who makes a change, which decides the checks it passes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Writer<'a> {
    User,
    Bot(&'a str),
    Dream(&'a str),
    Import,
}

impl Writer<'_> {
    fn author(self) -> Author {
        match self {
            Self::User => Author::User,
            Self::Bot(_) => Author::Bot,
            Self::Dream(_) => Author::Dream,
            Self::Import => Author::Imported,
        }
    }

    fn bot(self) -> Option<String> {
        match self {
            Self::Bot(bot) | Self::Dream(bot) => Some(bot.to_owned()),
            Self::User | Self::Import => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Entry {
    pub id: i64,
    /// The bot whose own layer holds it; `None` for the shared "yours" layer.
    pub bot: Option<String>,
    pub state: State,
    pub text: String,
    pub author: Author,
    /// The bot that wrote it, for bot and dream writes.
    pub author_bot: Option<String>,
    pub pinned: bool,
    /// The entry that replaced this archived one.
    pub superseded_by: Option<i64>,
    /// The group room it was learned in.
    pub room: Option<String>,
    /// Transcript message ids it was learned from.
    pub evidence: Vec<String>,
    /// Unix milliseconds.
    pub created_at: i64,
    pub updated_at: i64,
    pub rev: i64,
}

#[derive(Debug)]
pub enum Error {
    Refused(Refusal),
    NotFound,
    /// A bot or dreaming tried to change an entry the user wrote.
    UsersOwn,
    Sqlite(rusqlite::Error),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Refused(refusal) => refusal.fmt(f),
            Self::NotFound => f.write_str("No such memory."),
            Self::UsersOwn => f.write_str("The user wrote this memory, so only the user can change it."),
            Self::Sqlite(e) => write!(f, "memory store: {e}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Sqlite(e) => Some(e),
            // Display already says why: as a source too, the reason would print twice in a chain.
            Self::Refused(_) | Self::NotFound | Self::UsersOwn => None,
        }
    }
}

impl From<rusqlite::Error> for Error {
    fn from(e: rusqlite::Error) -> Self {
        Self::Sqlite(e)
    }
}

impl From<Refusal> for Error {
    fn from(refusal: Refusal) -> Self {
        Self::Refused(refusal)
    }
}

pub type Result<T> = std::result::Result<T, Error>;

/// A new entry.
#[derive(Clone, Copy, Debug)]
pub struct New<'a> {
    /// The bot whose own layer gets it; `None` for "yours".
    pub bot: Option<&'a str>,
    pub text: &'a str,
    pub room: Option<&'a str>,
    pub evidence: &'a [String],
}

/// The cap of a layer, in estimated tokens.
pub fn cap(bot: Option<&str>) -> usize {
    if bot.is_some() { tokens::BOT_CAP } else { tokens::YOURS_CAP }
}

pub fn add(conn: &Connection, writer: Writer<'_>, new: New<'_>, now: i64, rev: i64) -> Result<Entry> {
    admissible(conn, writer, new.bot, new.text)?;
    if let Writer::Bot(bot) | Writer::Dream(bot) = writer
        && spent(conn, bot, new.evidence)?
    {
        return Err(Refusal::Forgotten.into());
    }
    let text = new.text.trim();
    conn.execute(
        "INSERT INTO memory_entries (bot, state, text, author, author_bot, room, evidence, created_at, updated_at, rev)
         VALUES (?1, 'active', ?2, ?3, ?4, ?5, ?6, ?7, ?7, ?8)",
        params![new.bot, text, author_name(writer.author()), writer.bot(), new.room, new.evidence.join(","), now, rev],
    )?;
    get(conn, conn.last_insert_rowid())?.ok_or(Error::NotFound)
}

/// Whether [`add`] would take `text` into a layer (`None`: "yours") from `writer`; nothing is written. Lets a
/// caller refuse at once what it would only write later, such as a bot's proposal waiting for the user.
pub fn admissible(conn: &Connection, writer: Writer<'_>, bot: Option<&str>, text: &str) -> Result<()> {
    let text = text.trim();
    if text.is_empty() {
        return Err(Refusal::Empty.into());
    }
    if writer != Writer::User {
        match check::text(text) {
            // Moved memory was written under older, looser limits; it stays whole.
            Err(Refusal::TooLong) if writer == Writer::Import => {}
            checked => checked?,
        }
        if forgotten(conn, bot, text)? {
            return Err(Refusal::Forgotten.into());
        }
        if duplicate(conn, bot, text)? {
            return Err(Refusal::Duplicate.into());
        }
    }
    // An import may run past the cap: the opening takes what fits, and the importer archives the rest.
    if matches!(writer, Writer::Bot(_) | Writer::Dream(_)) && used(conn, bot)? + tokens::estimate(text) > cap(bot) {
        return Err(Refusal::Full.into());
    }
    Ok(())
}

pub fn revise(conn: &Connection, writer: Writer<'_>, id: i64, text: &str, now: i64, rev: i64) -> Result<Entry> {
    let entry = changeable(conn, writer, id)?;
    let text = text.trim();
    if text.is_empty() {
        return Err(Refusal::Empty.into());
    }
    if writer != Writer::User {
        check::text(text)?;
        if entry.state == State::Active {
            let others = used(conn, entry.bot.as_deref())? - tokens::estimate(&entry.text);
            if others + tokens::estimate(text) > cap(entry.bot.as_deref()) {
                return Err(Refusal::Full.into());
            }
        }
    }
    conn.execute(
        "UPDATE memory_entries SET text = ?2, updated_at = ?3, rev = ?4 WHERE id = ?1",
        params![id, text, now, rev],
    )?;
    get(conn, id)?.ok_or(Error::NotFound)
}

/// Takes an entry out of the opening, naming what replaced it.
pub fn archive(
    conn: &Connection,
    writer: Writer<'_>,
    id: i64,
    superseded_by: Option<i64>,
    now: i64,
    rev: i64,
) -> Result<Entry> {
    changeable(conn, writer, id)?;
    set_state(conn, id, State::Archived, superseded_by, now, rev)
}

/// Names the entry that replaced an archived one.
pub fn link(conn: &Connection, archived: i64, by: i64) -> Result<()> {
    conn.execute("UPDATE memory_entries SET superseded_by = ?2 WHERE id = ?1", params![archived, by])?;
    Ok(())
}

/// Brings an archived entry back into the opening.
pub fn activate(conn: &Connection, id: i64, now: i64, rev: i64) -> Result<Entry> {
    get(conn, id)?.ok_or(Error::NotFound)?;
    set_state(conn, id, State::Active, None, now, rev)
}

/// The user brought an archived entry back: it is active again, and dreaming won't archive it again.
pub fn bring_back(conn: &Connection, id: i64, now: i64, rev: i64) -> Result<Entry> {
    let entry = activate(conn, id, now, rev)?;
    tombstone(conn, entry.bot.as_deref(), std::iter::once(kept(id)))?;
    Ok(entry)
}

pub fn pin(conn: &Connection, id: i64, pinned: bool, now: i64, rev: i64) -> Result<Entry> {
    let changed = conn.execute(
        "UPDATE memory_entries SET pinned = ?2, updated_at = ?3, rev = ?4 WHERE id = ?1",
        params![id, pinned, now, rev],
    )?;
    if changed == 0 {
        return Err(Error::NotFound);
    }
    get(conn, id)?.ok_or(Error::NotFound)
}

/// Deletes an entry and leaves a tombstone, so no bot or dream can write it again.
pub fn forget(conn: &Connection, id: i64) -> Result<Entry> {
    let entry = get(conn, id)?.ok_or(Error::NotFound)?;
    bury(conn, &entry)?;
    conn.execute("DELETE FROM memory_entries WHERE id = ?1", [id])?;
    Ok(entry)
}

/// Keeps `text`, and memory drawn from the `message` it came from, out of a layer (`None`: "yours") for bots and
/// dreaming, as if it had been forgotten: the user declined it.
pub fn forbid(conn: &Connection, bot: Option<&str>, text: &str, message: Option<&str>) -> Result<()> {
    tombstone(conn, bot, std::iter::once(fingerprint(text)).chain(message.map(cited)))
}

/// Forgets every entry in one layer (`None`: "yours"). Returns what was deleted.
pub fn forget_layer(conn: &Connection, bot: Option<&str>) -> Result<Vec<Entry>> {
    let entries = layer(conn, bot)?;
    for entry in &entries {
        bury(conn, entry)?;
    }
    conn.execute("DELETE FROM memory_entries WHERE bot IS ?1", [bot])?;
    Ok(entries)
}

/// Removes what was kept for a deleted bot: its own layer and files, its tombstones and its dream log. What it
/// wrote to "yours" stays, since the user may still want it.
pub fn drop_bot(conn: &Connection, bot: &str) -> Result<()> {
    crate::files::forget_layer(conn, Some(bot))?;
    conn.execute("DELETE FROM memory_entries WHERE bot = ?1", [bot])?;
    conn.execute("DELETE FROM memory_tombstones WHERE scope = ?1", [scope(Some(bot))])?;
    for table in ["memory_dream_changes", "memory_dream_files"] {
        conn.execute(
            &format!("DELETE FROM {table} WHERE dream IN (SELECT id FROM memory_dreams WHERE bot = ?1)"),
            [bot],
        )?;
    }
    conn.execute("DELETE FROM memory_dreams WHERE bot = ?1", [bot])?;
    Ok(())
}

pub fn get(conn: &Connection, id: i64) -> Result<Option<Entry>> {
    Ok(conn.query_row(&format!("{SELECT} WHERE id = ?1"), [id], entry).optional()?)
}

/// Every entry in one layer (`None`: "yours"), pinned first, then the most recently changed.
pub fn layer(conn: &Connection, bot: Option<&str>) -> Result<Vec<Entry>> {
    let mut stmt = conn.prepare(&format!("{SELECT} WHERE bot IS ?1 ORDER BY pinned DESC, updated_at DESC, id DESC"))?;
    Ok(stmt.query_map([bot], entry)?.collect::<rusqlite::Result<_>>()?)
}

/// Entries changed after `rev`, for catching a client up.
pub fn changed_since(conn: &Connection, rev: i64) -> Result<Vec<Entry>> {
    let mut stmt = conn.prepare(&format!("{SELECT} WHERE rev > ?1 ORDER BY rev"))?;
    Ok(stmt.query_map([rev], entry)?.collect::<rusqlite::Result<_>>()?)
}

/// Active and archived entries one bot can see whose text contains `query`, ignoring case.
// ponytail: a LIKE scan; memory stays small (capped active layers, a few hundred archived entries). Add an FTS5
// trigram index if archives grow into the thousands.
pub fn search(conn: &Connection, bot: &str, query: &str, limit: usize) -> Result<Vec<Entry>> {
    let pattern = format!("%{}%", query.trim().replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_"));
    let mut stmt = conn.prepare(&format!(
        "{SELECT} WHERE (bot = ?1 OR bot IS NULL) AND text LIKE ?2 ESCAPE '\\'
         ORDER BY state = 'archived', updated_at DESC LIMIT ?3"
    ))?;
    let limit = i64::try_from(limit).unwrap_or(i64::MAX);
    Ok(stmt.query_map(params![bot, pattern, limit], entry)?.collect::<rusqlite::Result<_>>()?)
}

/// Estimated tokens the active entries of a layer use.
pub fn used(conn: &Connection, bot: Option<&str>) -> Result<usize> {
    let mut stmt = conn.prepare("SELECT text FROM memory_entries WHERE bot IS ?1 AND state = 'active'")?;
    let texts = stmt.query_map([bot], |row| row.get::<_, String>(0))?;
    texts.map(|text| Ok(tokens::estimate(&text?))).sum()
}

fn changeable(conn: &Connection, writer: Writer<'_>, id: i64) -> Result<Entry> {
    let entry = get(conn, id)?.ok_or(Error::NotFound)?;
    if matches!(writer, Writer::Bot(_) | Writer::Dream(_)) && entry.author == Author::User {
        return Err(Error::UsersOwn);
    }
    if matches!(writer, Writer::Dream(_)) && marked(conn, entry.bot.as_deref(), &kept(id))? {
        return Err(Error::UsersOwn);
    }
    Ok(entry)
}

fn set_state(
    conn: &Connection,
    id: i64,
    state: State,
    superseded_by: Option<i64>,
    now: i64,
    rev: i64,
) -> Result<Entry> {
    conn.execute(
        "UPDATE memory_entries SET state = ?2, superseded_by = ?3, updated_at = ?4, rev = ?5 WHERE id = ?1",
        params![id, state_name(state), superseded_by, now, rev],
    )?;
    get(conn, id)?.ok_or(Error::NotFound)
}

fn duplicate(conn: &Connection, bot: Option<&str>, text: &str) -> Result<bool> {
    let mut stmt = conn.prepare("SELECT text FROM memory_entries WHERE bot IS ?1")?;
    let texts = stmt.query_map([bot], |row| row.get::<_, String>(0))?;
    let wanted = fingerprint(text);
    for existing in texts {
        if fingerprint(&existing?) == wanted {
            return Ok(true);
        }
    }
    Ok(false)
}

fn forgotten(conn: &Connection, bot: Option<&str>, text: &str) -> Result<bool> {
    marked(conn, bot, &fingerprint(text))
}

pub(crate) fn marked(conn: &Connection, bot: Option<&str>, mark: &str) -> Result<bool> {
    Ok(conn
        .query_row("SELECT 1 FROM memory_tombstones WHERE scope = ?1 AND hash = ?2", params![scope(bot), mark], |_| {
            Ok(())
        })
        .optional()?
        .is_some())
}

/// A mark for an entry the user brought back, which dreaming leaves active.
fn kept(id: i64) -> String {
    format!("kept:{id}")
}

/// Tombstones the entry's text and the messages it was learned from, so a reworded copy of it isn't learned again
/// from the same words.
fn bury(conn: &Connection, entry: &Entry) -> Result<()> {
    let marks = std::iter::once(fingerprint(&entry.text)).chain(entry.evidence.iter().map(|m| cited(m)));
    tombstone(conn, entry.bot.as_deref(), marks)
}

pub(crate) fn tombstone(conn: &Connection, bot: Option<&str>, marks: impl Iterator<Item = String>) -> Result<()> {
    for mark in marks {
        conn.execute(
            "INSERT OR IGNORE INTO memory_tombstones (scope, hash) VALUES (?1, ?2)",
            params![scope(bot), mark],
        )?;
    }
    Ok(())
}

/// A tombstone for a message rather than a text: no bot or dream may draw memory for the layer from it again.
fn cited(message: &str) -> String {
    format!("message:{message}")
}

/// Whether the user turned down memory from one of these messages in a layer `bot` uses: the message is spent for
/// both, so a fact undone in one layer doesn't come back in the other.
fn spent(conn: &Connection, bot: &str, evidence: &[String]) -> Result<bool> {
    for message in evidence {
        let found = conn
            .query_row(
                "SELECT 1 FROM memory_tombstones WHERE scope IN (?1, ?2) AND hash = ?3",
                params![scope(None), scope(Some(bot)), cited(message)],
                |_| Ok(()),
            )
            .optional()?;
        if found.is_some() {
            return Ok(true);
        }
    }
    Ok(false)
}

fn scope(bot: Option<&str>) -> String {
    bot.map_or_else(|| "yours".to_owned(), |bot| format!("bot:{bot}"))
}

/// Only a hash of forgotten text is kept: case, spacing and trailing punctuation don't make it new.
fn fingerprint(text: &str) -> String {
    let normal: String =
        text.split_whitespace().collect::<Vec<_>>().join(" ").trim_end_matches(['.', '。', '!', '！']).to_lowercase();
    Sha256::digest(normal.as_bytes()).iter().fold(String::with_capacity(64), |mut hex, byte| {
        let _ = write!(hex, "{byte:02x}");
        hex
    })
}

const SELECT: &str = "SELECT id, bot, state, text, author, author_bot, pinned, superseded_by, room, evidence,
    created_at, updated_at, rev FROM memory_entries";

fn entry(row: &Row<'_>) -> rusqlite::Result<Entry> {
    let evidence: String = row.get(9)?;
    Ok(Entry {
        id: row.get(0)?,
        bot: row.get(1)?,
        state: parse_state(&row.get::<_, String>(2)?),
        text: row.get(3)?,
        author: parse_author(&row.get::<_, String>(4)?),
        author_bot: row.get(5)?,
        pinned: row.get(6)?,
        superseded_by: row.get(7)?,
        room: row.get(8)?,
        evidence: evidence.split(',').filter(|id| !id.is_empty()).map(str::to_owned).collect(),
        created_at: row.get(10)?,
        updated_at: row.get(11)?,
        rev: row.get(12)?,
    })
}

fn state_name(state: State) -> &'static str {
    match state {
        State::Active => "active",
        State::Archived => "archived",
    }
}

fn parse_state(name: &str) -> State {
    match name {
        "archived" => State::Archived,
        _ => State::Active,
    }
}

fn author_name(author: Author) -> &'static str {
    match author {
        Author::User => "user",
        Author::Bot => "bot",
        Author::Dream => "dream",
        Author::Imported => "imported",
    }
}

fn parse_author(name: &str) -> Author {
    match name {
        "bot" => Author::Bot,
        "dream" => Author::Dream,
        "imported" => Author::Imported,
        _ => Author::User,
    }
}

#[cfg(test)]
mod tests;
