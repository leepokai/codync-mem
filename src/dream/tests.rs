use super::*;

fn db() -> Connection {
    let conn = Connection::open_in_memory().unwrap();
    crate::schema::migrate(&conn).unwrap();
    conn
}

fn user(id: &str, text: &str) -> Message {
    Message { id: id.into(), from_user: true, text: vec![text.into()], date: "2026-10-17".into() }
}

fn bot_reply(id: &str, text: &str) -> Message {
    Message { id: id.into(), from_user: false, text: vec![text.into()], date: "2026-10-17".into() }
}

fn add(text: &str, quote: &str, message: &str) -> Add {
    Add { text: text.into(), shared: false, quote: quote.into(), message: message.into() }
}

fn seed(conn: &Connection, writer: Writer<'_>, text: &str) -> Entry {
    store::add(conn, writer, New { bot: Some("a"), text, room: None, evidence: &[] }, 1, 1).unwrap()
}

#[test]
fn keeps_only_changes_backed_by_the_users_own_words() {
    let conn = db();
    let snapshot = snapshot(&conn, "a", false, "2026-10-17").unwrap();
    let messages =
        [user("u1", "I moved to Tainan last month"), bot_reply("b1", "Noted that you love durian"), user("u2", "yes")];
    let plan = Plan {
        add: vec![
            add("Lives in Tainan", "I moved to Tainan", "m1"),
            add("Loves durian", "you love durian", "m2"),
            add("Agreed to something", "yes", "m3"),
            add("Made up", "never said this", "m1"),
            add("Password is hunter22", "I moved to Tainan", "m1"),
        ],
        ..Plan::default()
    };
    let (kept, rejected) = audit(&snapshot, &messages, plan);
    assert_eq!(kept.add.iter().map(|a| a.text.as_str()).collect::<Vec<_>>(), ["Lives in Tainan"]);
    assert_eq!(rejected.len(), 4);
    let Outcome::Changed(dream) = apply(&conn, &snapshot, &kept, &messages, 5, 5).unwrap() else { panic!() };
    assert_eq!(dream.added[0].text, "Lives in Tainan");
    let entry = store::get(&conn, dream.added[0].id).unwrap().unwrap();
    assert_eq!((entry.author, entry.evidence), (Author::Dream, vec!["u1".to_owned()]));
}

#[test]
fn never_archives_the_users_words_or_more_than_a_quarter() {
    let conn = db();
    let mine = seed(&conn, Writer::User, "Lives in Taipei");
    let others: Vec<Entry> = (0..7).map(|i| seed(&conn, Writer::Bot("a"), &format!("Fact number {i}"))).collect();
    let snapshot = snapshot(&conn, "a", false, "2026-10-17").unwrap();
    let messages = [user("u1", "forget all of those old facts please")];
    let archive =
        |id| Archive { id, quote: "forget all of those old facts".into(), message: "m1".into(), replaced_by: None };

    let (kept, rejected) = audit(&snapshot, &messages, Plan { archive: vec![archive(mine.id)], ..Plan::default() });
    assert!(kept.is_empty() && rejected[0].contains("the user wrote it"));

    let three = others.iter().take(3).map(|e| archive(e.id)).collect();
    let (kept, rejected) = audit(&snapshot, &messages, Plan { archive: three, ..Plan::default() });
    assert!(kept.is_empty() && rejected.last().unwrap().contains("more than a quarter"));

    let two = others.iter().take(2).map(|e| archive(e.id)).collect();
    let (kept, _) = audit(&snapshot, &messages, Plan { archive: two, ..Plan::default() });
    assert_eq!(kept.archive.len(), 2);
}

#[test]
fn a_replacement_links_the_old_entry_and_undo_reverses_the_run() {
    let conn = db();
    let old = seed(&conn, Writer::Bot("a"), "Lives in Taipei");
    let snapshot = snapshot(&conn, "a", false, "2026-10-17").unwrap();
    let messages = [user("u1", "We just moved to Tainan for good")];
    let plan = Plan {
        add: vec![add("Lives in Tainan", "just moved to Tainan", "m1")],
        archive: vec![Archive {
            id: old.id,
            quote: "We just moved to Tainan".into(),
            message: "m1".into(),
            replaced_by: Some(0),
        }],
        ..Plan::default()
    };
    let (kept, _) = audit(&snapshot, &messages, plan);
    let Outcome::Changed(dream) = apply(&conn, &snapshot, &kept, &messages, 5, 5).unwrap() else { panic!() };
    let archived = store::get(&conn, old.id).unwrap().unwrap();
    assert_eq!(archived.state, State::Archived);
    assert_eq!(archived.superseded_by, Some(dream.added[0].id));
    assert_eq!(recent(&conn, "a", 5).unwrap(), std::slice::from_ref(&dream));

    let undone = undo(&conn, dream.id, 6, 6).unwrap();
    assert!(undone.undone);
    assert_eq!(store::get(&conn, old.id).unwrap().unwrap().state, State::Active);
    assert!(store::get(&conn, dream.added[0].id).unwrap().is_none());
    // Forgotten by the undo, so the next run can't add it again.
    let again = store::add(
        &conn,
        Writer::Dream("a"),
        New { bot: Some("a"), text: "Lives in Tainan", room: None, evidence: &[] },
        7,
        7,
    );
    assert!(matches!(again, Err(Error::Refused(check::Refusal::Forgotten))));
}

#[test]
fn a_run_is_dropped_when_the_user_edited_memory_meanwhile() {
    let conn = db();
    let entry = seed(&conn, Writer::Bot("a"), "Likes tea");
    let snapshot = snapshot(&conn, "a", false, "2026-10-17").unwrap();
    store::revise(&conn, Writer::User, entry.id, "Likes green tea", 2, 2).unwrap();
    let messages = [user("u1", "my daughter turns six in May")];
    let (kept, _) = audit(
        &snapshot,
        &messages,
        Plan { add: vec![add("Daughter turns 6 in May", "my daughter turns six", "m1")], ..Plan::default() },
    );
    assert_eq!(apply(&conn, &snapshot, &kept, &messages, 3, 3).unwrap(), Outcome::Stale);
}

#[test]
fn reviews_rewrite_dated_entries_in_the_past_tense() {
    let conn = db();
    let trip = seed(&conn, Writer::Dream("a"), "Going to Singapore in July 2026");
    let tea = seed(&conn, Writer::Bot("a"), "Likes tea");
    for i in 0..6 {
        seed(&conn, Writer::Bot("a"), &format!("Filler {i}"));
    }
    let snapshot = snapshot(&conn, "a", false, "2026-10-17").unwrap();
    let plan = Plan {
        review: vec![
            Review { id: trip.id, text: "Went to Singapore in July 2026".into() },
            Review { id: tea.id, text: "Liked tea".into() },
        ],
        ..Plan::default()
    };
    let (kept, rejected) = audit(&snapshot, &[], plan);
    assert_eq!(kept.review.len(), 1);
    assert!(rejected[0].contains("no date"));
    let Outcome::Changed(dream) = apply(&conn, &snapshot, &kept, &[], 5, 5).unwrap() else { panic!() };
    assert_eq!(dream.added[0].text, "Went to Singapore in July 2026");
    assert_eq!(store::get(&conn, trip.id).unwrap().unwrap().superseded_by, Some(dream.added[0].id));

    // The next day's review leaves its own rewrite alone.
    let went = dream.added[0].id;
    let snapshot = super::snapshot(&conn, "a", false, "2026-10-18").unwrap();
    let again = Plan { review: vec![Review { id: went, text: "Went to Singapore".into() }], ..Plan::default() };
    let (kept, _) = audit(&snapshot, &[], again);
    assert_eq!(apply(&conn, &snapshot, &kept, &[], 6, 6).unwrap(), Outcome::Unchanged);
    assert_eq!(store::get(&conn, went).unwrap().unwrap().state, State::Active);
}

#[test]
fn parses_the_outermost_object_and_survives_chatter() {
    let raw = "Sure!\n```json\n{\"add\": [{\"text\": \"Likes tea\", \"quote\": \"I like tea a lot\", \"message\": \"m1\"}]}\n```";
    let plan = parse(raw).unwrap();
    assert_eq!(plan.add[0].text, "Likes tea");
    assert_eq!(plan.archive, [] as [Archive; 0]);
    assert_eq!(parse("no json here"), None);
}

#[test]
fn the_prompt_numbers_messages_and_marks_the_users_entries() {
    let conn = db();
    seed(&conn, Writer::User, "Lives in Taipei");
    let snapshot = snapshot(&conn, "a", true, "2026-10-17").unwrap();
    let (system, material) = prompt(&snapshot, &[user("u1", "hello there friend"), bot_reply("b1", "hi")]);
    assert!(system.contains("Today is 2026-10-17"));
    assert!(material.contains("(this bot, written by the user) Lives in Taipei"));
    assert!(material.contains("[m1] User, 2026-10-17:\nhello there friend"));
    assert!(material.contains("[m2] Bot"));
    assert!(material.contains("Long-form files now:\n(none)"));
    assert!(!system.contains("moveOut"), "only offered when memory is nearly full");
}

#[test]
fn notes_for_files_cite_the_user_and_entries_move_out_only_when_full() {
    let conn = db();
    let messages = [user("u1", "Mei started piano lessons on Tuesdays")];
    let append = |file: &str, description: &str| Append {
        file: file.into(),
        shared: true,
        text: "Mei has piano lessons on Tuesdays".into(),
        quote: "Mei started piano lessons".into(),
        message: "m1".into(),
        description: description.into(),
    };
    let fact = seed(&conn, Writer::Bot("a"), "Mei's teacher is Ms. Lin");
    let snapshot = snapshot(&conn, "a", false, "2026-10-17").unwrap();
    let plan = Plan {
        append: vec![append("people.md", "The user's family"), append("People.md", "x"), append("new.md", "")],
        move_out: vec![MoveOut { id: fact.id, file: "people.md".into(), description: String::new() }],
        ..Plan::default()
    };
    let (kept, rejected) = audit(&snapshot, &messages, plan);
    assert_eq!(kept.append.len(), 1);
    assert!(!kept.append[0].shared, "a bot without the shared layer keeps notes to itself");
    assert!(rejected.iter().any(|r| r.contains("lowercase")) && rejected.iter().any(|r| r.contains("description")));
    assert!(rejected.iter().any(|r| r.contains("isn't full")));

    let Outcome::Changed(dream) = apply(&conn, &snapshot, &kept, &messages, 5, 5).unwrap() else { panic!() };
    let notes = notes(&snapshot, &kept);
    assert_eq!(
        notes,
        [Note {
            bot: Some("a".into()),
            file: "people.md".into(),
            text: "Mei has piano lessons on Tuesdays".into(),
            description: "The user's family".into(),
        }]
    );
    // The caller wrote it, or didn't.
    discard_if_empty(&conn, dream.id).unwrap();
    assert!(get(&conn, dream.id).unwrap().is_none(), "a run whose notes all failed is no run");
}

#[test]
fn a_full_layer_moves_detail_out() {
    let conn = db();
    let mut filler = Vec::new();
    while store::used(&conn, Some("a")).unwrap() * 10 < store::cap(Some("a")) * 9 {
        filler.push(seed(
            &conn,
            Writer::Bot("a"),
            &format!("Filler fact number {} about the user's week", filler.len()),
        ));
    }
    let snapshot = snapshot(&conn, "a", false, "2026-10-17").unwrap();
    assert!(snapshot.full);
    let plan = Plan {
        move_out: vec![MoveOut { id: filler[0].id, file: "week.md".into(), description: "The user's week".into() }],
        ..Plan::default()
    };
    let (kept, rejected) = audit(&snapshot, &[], plan);
    assert!(rejected.is_empty(), "{rejected:?}");
    let Outcome::Changed(dream) = apply(&conn, &snapshot, &kept, &[], 5, 5).unwrap() else { panic!() };
    assert_eq!(dream.archived[0].id, filler[0].id);
    assert_eq!(notes(&snapshot, &kept)[0].text, filler[0].text);
    let version = files::record(
        &conn,
        &files::Change {
            bot: Some("a"),
            name: "week.md",
            content: Some("---\ndescription: The user's week\n---\nFiller"),
            author: files::Author::Dream,
            author_bot: Some("a"),
            cause: files::Cause::Dream,
            mtime: 1,
            size: 1,
        },
        6,
        6,
    )
    .unwrap();
    record_file(&conn, dream.id, version.id).unwrap();
    let logged = get(&conn, dream.id).unwrap().unwrap();
    assert_eq!(logged.files, [FileChange { name: "week.md".into(), shared: false, version: version.id }]);
}
