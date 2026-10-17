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
            Self::Refused(refusal) => Some(refusal),
            Self::NotFound | Self::UsersOwn => None,
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

/// Keeps `text` out of a layer (`None`: "yours") for bots and dreaming, as if it had been forgotten: the user
/// declined it.
pub fn forbid(conn: &Connection, bot: Option<&str>, text: &str) -> Result<()> {
    conn.execute(
        "INSERT OR IGNORE INTO memory_tombstones (scope, hash) VALUES (?1, ?2)",
        params![scope(bot), fingerprint(text)],
    )?;
    Ok(())
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

/// Removes what was kept for a deleted bot: its own layer, its tombstones and its dream log. What it wrote to
/// "yours" stays, since the user may still want it.
pub fn drop_bot(conn: &Connection, bot: &str) -> Result<()> {
    conn.execute("DELETE FROM memory_entries WHERE bot = ?1", [bot])?;
    conn.execute("DELETE FROM memory_tombstones WHERE scope = ?1", [scope(Some(bot))])?;
    conn.execute(
        "DELETE FROM memory_dream_changes WHERE dream IN (SELECT id FROM memory_dreams WHERE bot = ?1)",
        [bot],
    )?;
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
    Ok(conn
        .query_row(
            "SELECT 1 FROM memory_tombstones WHERE scope = ?1 AND hash = ?2",
            params![scope(bot), fingerprint(text)],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

fn bury(conn: &Connection, entry: &Entry) -> Result<()> {
    conn.execute(
        "INSERT OR IGNORE INTO memory_tombstones (scope, hash) VALUES (?1, ?2)",
        params![scope(entry.bot.as_deref()), fingerprint(&entry.text)],
    )?;
    Ok(())
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
mod tests {
    use super::*;

    fn db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        crate::schema::migrate(&conn).unwrap();
        conn
    }

    fn new(bot: Option<&str>, text: &str) -> New<'static> {
        New { bot: bot.map(|b| &*b.to_owned().leak()), text: text.to_owned().leak(), room: None, evidence: &[] }
    }

    #[test]
    fn bots_cannot_change_what_the_user_wrote() {
        let conn = db();
        let mine = add(&conn, Writer::User, new(Some("a"), "Likes tea"), 1, 1).unwrap();
        assert!(matches!(revise(&conn, Writer::Bot("a"), mine.id, "Likes coffee", 2, 2), Err(Error::UsersOwn)));
        assert!(matches!(archive(&conn, Writer::Dream("a"), mine.id, None, 2, 2), Err(Error::UsersOwn)));
        let theirs = add(&conn, Writer::Bot("a"), new(Some("a"), "Works late"), 3, 3).unwrap();
        assert_eq!(theirs.author, Author::Bot);
        assert_eq!(theirs.author_bot.as_deref(), Some("a"));
        assert_eq!(revise(&conn, Writer::Bot("a"), theirs.id, "Works late on Fridays", 4, 4).unwrap().rev, 4);
    }

    #[test]
    fn forgotten_entries_never_come_back_from_a_bot() {
        let conn = db();
        let entry = add(&conn, Writer::Bot("a"), new(None, "Lives in Taipei"), 1, 1).unwrap();
        forget(&conn, entry.id).unwrap();
        let again = add(&conn, Writer::Bot("b"), new(None, "lives in  taipei."), 2, 2);
        assert!(matches!(again, Err(Error::Refused(Refusal::Forgotten))));
        // The tombstone is per layer, and the user may always write it again.
        add(&conn, Writer::Bot("a"), new(Some("a"), "Lives in Taipei"), 3, 3).unwrap();
        add(&conn, Writer::User, new(None, "Lives in Taipei"), 4, 4).unwrap();
    }

    #[test]
    fn bot_writes_stop_at_the_cap_but_the_users_do_not() {
        let conn = db();
        let fact = "字".repeat(100);
        for i in 0..3 {
            add(&conn, Writer::Bot("a"), new(None, &format!("{fact}{i}")), i, i).unwrap();
        }
        assert_eq!(used(&conn, None).unwrap(), 303);
        let over = add(&conn, Writer::Bot("a"), new(None, &format!("{fact}x")), 9, 9);
        assert!(matches!(over, Err(Error::Refused(Refusal::Full))));
        add(&conn, Writer::User, new(None, &format!("{fact}y")), 10, 10).unwrap();
    }

    #[test]
    fn searches_both_layers_case_insensitively() {
        let conn = db();
        add(&conn, Writer::User, new(None, "Allergic to Peanuts"), 1, 1).unwrap();
        let old = add(&conn, Writer::User, new(Some("a"), "Uses peanut butter in recipes"), 2, 2).unwrap();
        archive(&conn, Writer::User, old.id, None, 3, 3).unwrap();
        add(&conn, Writer::User, new(Some("b"), "Peanut farm visit"), 4, 4).unwrap();
        let found: Vec<String> = search(&conn, "a", "PEANUT", 10).unwrap().into_iter().map(|e| e.text).collect();
        assert_eq!(found, ["Allergic to Peanuts", "Uses peanut butter in recipes"]);
        assert_eq!(search(&conn, "a", "100%", 10).unwrap(), [] as [Entry; 0]);
    }

    #[test]
    fn a_bot_cant_write_what_the_layer_already_holds() {
        let conn = db();
        add(&conn, Writer::User, new(Some("a"), "Likes tea"), 1, 1).unwrap();
        let again = add(&conn, Writer::Bot("a"), new(Some("a"), "likes  TEA."), 2, 2);
        assert!(matches!(again, Err(Error::Refused(Refusal::Duplicate))));
        assert!(admissible(&conn, Writer::Bot("a"), Some("a"), "Likes coffee").is_ok());
        assert!(admissible(&conn, Writer::Bot("a"), Some("a"), "my password is 1234").is_err());
    }

    #[test]
    fn forgetting_a_layer_leaves_the_others() {
        let conn = db();
        add(&conn, Writer::User, new(None, "Shared fact"), 1, 1).unwrap();
        add(&conn, Writer::User, new(Some("a"), "Bot fact"), 2, 2).unwrap();
        assert_eq!(forget_layer(&conn, Some("a")).unwrap().len(), 1);
        assert_eq!(layer(&conn, None).unwrap().len(), 1);
        assert_eq!(layer(&conn, Some("a")).unwrap(), [] as [Entry; 0]);
        assert_eq!(changed_since(&conn, 0).unwrap().len(), 1);
    }
}
