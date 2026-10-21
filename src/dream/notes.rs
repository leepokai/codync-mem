//! What dreaming adds to notes: a line that cites the user's words (`append`), or a line moved out of a nearly full
//! `MEMORY.md` (`moveOut`). It only appends; it never rewrites a note.

use rusqlite::Connection;
use serde::Deserialize;
use std::collections::BTreeMap;

use super::{Edit, Message, Plan, Snapshot, cite};
use crate::files;
use crate::store::{self, unmarked};
use crate::{check, tokens};

/// The most one appended line may hold, in estimated tokens.
pub const NOTE_CAP: usize = 300;

/// A line for a note, cited like an add.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct Append {
    pub file: String,
    #[serde(default)]
    pub shared: bool,
    pub text: String,
    #[serde(default)]
    pub quote: String,
    #[serde(default)]
    pub message: String,
    /// When to read the file; needed when it is new.
    #[serde(default)]
    pub description: String,
}

/// A line moved out of a nearly full `MEMORY.md` into a note of the same layer.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct MoveOut {
    pub line: String,
    #[serde(default)]
    pub shared: bool,
    pub file: String,
    #[serde(default)]
    pub description: String,
}

/// Keeps the appends and moves the rules allow into `kept`, and says why the others were dropped.
pub(super) fn audit(
    snapshot: &Snapshot,
    messages: &[Message],
    appends: Vec<Append>,
    moves: Vec<MoveOut>,
    kept: &mut Plan,
    rejected: &mut Vec<String>,
) {
    for mut append in appends {
        append.shared &= snapshot.shared;
        let verdict = target(snapshot, append.shared, &append.file, &append.description)
            .and_then(|()| text(&append.text))
            .and_then(|()| cite(messages, &append.quote, &append.message).map(drop));
        match verdict {
            Ok(()) => kept.append.push(append),
            Err(why) => rejected.push(format!("append to {}: {why}", append.file)),
        }
    }
    for mut moved in moves {
        moved.shared &= snapshot.shared;
        let verdict = (|| {
            if !snapshot.full {
                return Err("MEMORY.md isn't full");
            }
            let fact = snapshot.fact(moved.shared, &moved.line).ok_or("no such line in MEMORY.md")?;
            if fact.author.is_users() {
                return Err("the user wrote it");
            }
            target(snapshot, moved.shared, &moved.file, &moved.description)
        })();
        match verdict {
            Ok(()) => kept.move_out.push(moved),
            Err(why) => rejected.push(format!("move \"{}\": {why}", moved.line)),
        }
    }
}

/// A note name the folders take; a new note also needs a description that passes the checks.
fn target(snapshot: &Snapshot, shared: bool, file: &str, description: &str) -> Result<(), &'static str> {
    if !files::valid_name(file) || file == files::MAIN {
        return Err("the file name isn't lowercase-with-dashes.md");
    }
    let layer = snapshot.layer_bot(shared);
    let exists = snapshot.files.iter().any(|f| f.name == file && f.bot.as_deref() == layer);
    if !exists && (description.trim().is_empty() || check::content(description).is_err()) {
        return Err("a new note needs a description");
    }
    Ok(())
}

fn text(text: &str) -> Result<(), &'static str> {
    if text.trim().is_empty() {
        return Err("the note is empty");
    }
    if tokens::estimate(text) > NOTE_CAP {
        return Err("the note is too long");
    }
    check::content(text).map_err(check::Refusal::message)
}

/// What a run writes to notes.
pub(super) struct Notes {
    pub(super) files: Vec<Edit>,
    /// The moves whose note took the line, for `MEMORY.md` to lose it.
    pub(super) moved: Vec<MoveOut>,
    pub(super) rejected: Vec<String>,
}

/// Each note's new content: its lines appended, a new note starting with frontmatter. A line the user had
/// forgotten, or learned from a message whose memory was, stays out; a note past its checks is left as it was.
pub(super) fn edits(conn: &Connection, snapshot: &Snapshot, plan: &Plan, messages: &[Message]) -> store::Result<Notes> {
    let mut out = Notes { files: Vec::new(), moved: Vec::new(), rejected: Vec::new() };
    // (layer, file) → (description, lines), in a stable order.
    let mut notes: BTreeMap<(Option<String>, String), (String, Vec<String>)> = BTreeMap::new();
    for append in &plan.append {
        let layer = snapshot.layer_bot(append.shared);
        let text = append.text.trim();
        if super::edits::forgotten(conn, snapshot, layer, text, super::message(messages, &append.message))? {
            out.rejected.push(format!("append to {}: the user had it forgotten", append.file));
            continue;
        }
        let key = (layer.map(str::to_owned), append.file.clone());
        let note = notes.entry(key).or_insert_with(|| (append.description.trim().to_owned(), Vec::new()));
        note.1.push(text.to_owned());
    }
    for moved in &plan.move_out {
        let key = (snapshot.layer_bot(moved.shared).map(str::to_owned), moved.file.clone());
        let note = notes.entry(key).or_insert_with(|| (moved.description.trim().to_owned(), Vec::new()));
        note.1.push(unmarked(&moved.line).to_owned());
    }
    for ((bot, name), (description, lines)) in notes {
        let before = files::current(conn, bot.as_deref(), &name)?;
        let mut after = match &before {
            Some(text) => format!("{}\n", text.trim_end()),
            None => format!("---\ndescription: {description}\n---\n"),
        };
        for line in &lines {
            let line = if line.starts_with(['-', '#', '*']) { line.clone() } else { format!("- {line}") };
            after.push_str(&line);
            after.push('\n');
        }
        match files::check_change(bot.as_deref(), &name, before.as_deref(), &after) {
            Ok(()) => {
                let into = |m: &&MoveOut| snapshot.layer_bot(m.shared) == bot.as_deref() && m.file == name;
                out.moved.extend(plan.move_out.iter().filter(into).cloned());
                out.files.push(Edit { bot, name, before, after });
            }
            Err(refusal) => out.rejected.push(format!("note {name}: {refusal}")),
        }
    }
    Ok(out)
}
