//! What dreaming adds to long-form files: a line that cites the user's words (`append`), or an entry moved out of a
//! nearly full layer (`moveOut`). It only appends; it never rewrites a file. The caller writes them ([`notes`]).

use serde::Deserialize;

use super::{Message, Plan, Snapshot, cite};
use crate::files;
use crate::store::Author;
use crate::{check, tokens};

/// The most one note may hold, in estimated tokens.
pub const NOTE_CAP: usize = 300;

/// A line for a long-form file, cited like an add.
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

/// An entry moved out of a nearly full layer into a file of the same layer.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct MoveOut {
    pub id: i64,
    pub file: String,
    #[serde(default)]
    pub description: String,
}

/// A line for the caller to append. A new file starts with frontmatter holding `description`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Note {
    /// The bot whose folder holds the file; `None` for the shared one.
    pub bot: Option<String>,
    pub file: String,
    pub text: String,
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
        let layer = (!append.shared).then_some(snapshot.bot.as_str());
        let verdict = target(snapshot, layer, &append.file, &append.description)
            .and_then(|()| text(&append.text))
            .and_then(|()| cite(messages, &append.quote, &append.message).map(drop));
        match verdict {
            Ok(()) => kept.append.push(append),
            Err(why) => rejected.push(format!("append to {}: {why}", append.file)),
        }
    }
    for moved in moves {
        let verdict = (|| {
            if !snapshot.full {
                return Err("memory isn't full");
            }
            let entry = snapshot.entries.iter().find(|e| e.id == moved.id).ok_or("no such active entry")?;
            if entry.author == Author::User {
                return Err("the user wrote it");
            }
            target(snapshot, entry.bot.as_deref(), &moved.file, &moved.description)
        })();
        match verdict {
            Ok(()) => kept.move_out.push(moved),
            Err(why) => rejected.push(format!("move [{}]: {why}", moved.id)),
        }
    }
}

/// A file name the folders take; a new file also needs a description that passes the checks.
fn target(snapshot: &Snapshot, layer: Option<&str>, file: &str, description: &str) -> Result<(), &'static str> {
    if !files::valid_name(file) {
        return Err("the file name isn't lowercase-with-dashes.md");
    }
    let exists = snapshot.files.iter().any(|f| f.name == file && f.bot.as_deref() == layer);
    if !exists && (description.trim().is_empty() || check::content(description).is_err()) {
        return Err("a new file needs a description");
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

/// The lines an audited plan appends, in order: its appends, then the entries it moves out.
pub fn notes(snapshot: &Snapshot, plan: &Plan) -> Vec<Note> {
    let appended = plan.append.iter().map(|a| Note {
        bot: (!a.shared).then(|| snapshot.bot.clone()),
        file: a.file.clone(),
        text: a.text.trim().to_owned(),
        description: a.description.trim().to_owned(),
    });
    let moved = plan.move_out.iter().filter_map(|m| {
        let entry = snapshot.entries.iter().find(|e| e.id == m.id)?;
        Some(Note {
            bot: entry.bot.clone(),
            file: m.file.clone(),
            text: entry.text.clone(),
            description: m.description.trim().to_owned(),
        })
    });
    appended.chain(moved).collect()
}
