use super::*;
use crate::files::{Author, Cause, Change};

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

fn remove(line: &str, quote: &str, message: &str) -> Remove {
    Remove { line: line.into(), shared: false, quote: quote.into(), message: message.into() }
}

/// Writes bot `a`'s MEMORY.md as `author` wrote it.
fn seed(conn: &Connection, author: Author, content: &str) {
    let change = Change {
        bot: Some("a"),
        name: MAIN,
        content: Some(content),
        author,
        author_bot: None,
        cause: Cause::Edit,
        message: None,
        mtime: 1,
        size: 1,
    };
    files::record(conn, &change, 1, 1).unwrap();
}

/// `facts` as MEMORY.md lines.
fn lines(facts: impl IntoIterator<Item = String>) -> String {
    facts.into_iter().map(|fact| format!("- {fact}\n")).collect::<Vec<_>>().concat()
}

/// What a run would write to bot `a`'s MEMORY.md.
fn main_after(edits: &Edits) -> &str {
    edits.files.iter().find(|e| e.name == MAIN).map_or("", |e| e.after.as_str())
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
    let (edits, _) = edits(&conn, &snapshot, &kept, &messages).unwrap();
    assert_eq!(main_after(&edits), "- Lives in Tainan\n");
    assert_eq!(
        edits.added,
        [Line { bot: Some("a".into()), text: "Lives in Tainan".into(), message: Some("u1".into()), rewrites: None }]
    );
}

#[test]
fn lines_written_on_the_computer_are_the_users() {
    let conn = db();
    let filler = lines((0..7).map(|i| format!("Filler {i}")));
    seed(&conn, Author::Computer, &format!("- Vegetarian since 2020\n{filler}"));
    let snapshot = snapshot(&conn, "a", false, "2026-10-17").unwrap();
    let messages = [user("u1", "I eat meat again these days")];
    let plan = Plan {
        remove: vec![remove("Vegetarian since 2020", "I eat meat again", "m1")],
        review: vec![Review { line: "Vegetarian since 2020".into(), shared: false, text: "Was vegetarian".into() }],
        ..Plan::default()
    };
    let (kept, rejected) = audit(&snapshot, &messages, plan);
    assert!(kept.is_empty() && rejected.iter().all(|r| r.contains("the user wrote it")), "{rejected:?}");
    assert!(
        prompt(&snapshot, &messages).1.contains("This bot's (\"shared\": false), written by the user:\n- Vegetarian")
    );
}

#[test]
fn a_run_writes_only_what_it_changed() {
    let conn = db();
    seed(&conn, Author::Bot, "- Has two cats\n- Lives in Taipei");
    let snapshot = snapshot(&conn, "a", false, "2026-10-17").unwrap();
    assert_eq!(edits(&conn, &snapshot, &Plan::default(), &[]).unwrap().0, Edits::default());
}

#[test]
fn a_forgotten_line_stays_out_of_notes_too() {
    let conn = db();
    store::bury(&conn, Some("a"), ["Sister Amy is pregnant"], ["u2"]).unwrap();
    let snapshot = snapshot(&conn, "a", false, "2026-10-17").unwrap();
    let messages = [user("u1", "my sister Amy is pregnant again"), user("u2", "Amy's baby is due in May")];
    let append = |text: &str, quote: &str, message: &str| Append {
        file: "people.md".into(),
        shared: false,
        text: text.into(),
        quote: quote.into(),
        message: message.into(),
        description: "The user's family".into(),
    };
    let plan = Plan {
        append: vec![
            append("Sister Amy is pregnant", "my sister Amy is pregnant", "m1"),
            append("Amy's baby is due in May 2027", "Amy's baby is due in May", "m2"),
        ],
        ..Plan::default()
    };
    let (kept, _) = audit(&snapshot, &messages, plan);
    assert_eq!(kept.append.len(), 2);
    let (edits, rejected) = edits(&conn, &snapshot, &kept, &messages).unwrap();
    assert!(edits.files.is_empty(), "{edits:?}");
    assert_eq!(rejected.iter().filter(|r| r.contains("forgotten")).count(), 2, "{rejected:?}");
}

#[test]
fn never_removes_the_users_words_or_more_than_a_quarter() {
    let conn = db();
    seed(&conn, Author::User, "- Lives in Taipei\n");
    let facts = lines((0..7).map(|i| format!("Fact number {i}")));
    let mut all = "- Lives in Taipei\n".to_owned();
    all.push_str(&facts);
    seed(&conn, Author::Bot, &all);
    let snapshot = snapshot(&conn, "a", false, "2026-10-17").unwrap();
    let messages = [user("u1", "forget all of those old facts please")];
    let quote = "forget all of those old facts";

    let (kept, rejected) =
        audit(&snapshot, &messages, Plan { remove: vec![remove("Lives in Taipei", quote, "m1")], ..Plan::default() });
    assert!(kept.is_empty() && rejected[0].contains("the user wrote it"), "{rejected:?}");

    let three = (0..3).map(|i| remove(&format!("Fact number {i}"), quote, "m1")).collect();
    let (kept, rejected) = audit(&snapshot, &messages, Plan { remove: three, ..Plan::default() });
    assert!(kept.is_empty() && rejected.last().unwrap().contains("more than a quarter"));

    let two = (0..2).map(|i| remove(&format!("Fact number {i}"), quote, "m1")).collect();
    let (kept, _) = audit(&snapshot, &messages, Plan { remove: two, ..Plan::default() });
    let (edits, _) = edits(&conn, &snapshot, &kept, &messages).unwrap();
    assert!(!main_after(&edits).contains("Fact number 0") && main_after(&edits).contains("Fact number 2"));
    assert_eq!(edits.removed.iter().map(|l| l.text.as_str()).collect::<Vec<_>>(), ["Fact number 0", "Fact number 1"]);
}

#[test]
fn a_replacement_takes_the_old_line_out_and_forgotten_lines_stay_out() {
    let conn = db();
    seed(&conn, Author::Bot, "- Lives in Taipei\n- Likes tea\n- Has two cats\n- Works late\n");
    let snapshot = snapshot(&conn, "a", false, "2026-10-17").unwrap();
    let messages =
        [user("u1", "We just moved to Tainan for good"), user("u2", "I quit coffee and my sister Mei visits monthly")];
    store::bury(&conn, Some("a"), ["Quit coffee"], ["u9"]).unwrap();
    let plan = Plan {
        add: vec![
            add("Lives in Tainan", "just moved to Tainan", "m1"),
            add("Quit coffee", "I quit coffee and my sister Mei", "m2"),
            add("Likes tea", "I quit coffee and my sister", "m2"),
        ],
        remove: vec![remove("Lives in Taipei", "We just moved to Tainan", "m1")],
        ..Plan::default()
    };
    let (kept, _) = audit(&snapshot, &messages, plan);
    let (edits, rejected) = edits(&conn, &snapshot, &kept, &messages).unwrap();
    assert_eq!(main_after(&edits), "- Likes tea\n- Has two cats\n- Works late\n- Lives in Tainan\n");
    assert!(rejected.iter().any(|r| r.contains("Quit coffee") && r.contains("forgotten")), "{rejected:?}");
    assert!(rejected.iter().any(|r| r.contains("Likes tea") && r.contains("already")), "{rejected:?}");
}

#[test]
fn a_line_copied_from_the_prompt_names_its_fact() {
    let conn = db();
    seed(&conn, Author::Bot, "- Lives in Hsinchu.\n");
    let snapshot = snapshot(&conn, "a", false, "2026-10-17").unwrap();
    let messages = [user("u1", "please forget where I live")];
    let (_, material) = prompt(&snapshot, &messages);
    let shown = material.lines().find(|l| l.contains("Hsinchu")).expect("the line is shown");
    let plan = Plan { remove: vec![remove(shown, "forget where I live", "m1")], ..Plan::default() };
    let (kept, rejected) = audit(&snapshot, &messages, plan);
    assert!(rejected.is_empty(), "{rejected:?}");
    let (edits, _) = edits(&conn, &snapshot, &kept, &messages).unwrap();
    assert_eq!(main_after(&edits), "");
}

#[test]
fn reviews_rewrite_dated_lines_in_the_past_tense_once() {
    let conn = db();
    let filler = lines((0..6).map(|i| format!("Filler {i}")));
    seed(&conn, Author::Dream, &format!("- Going to Singapore in July 2026\n- Likes tea\n{filler}"));
    let snapshot = snapshot(&conn, "a", false, "2026-10-17").unwrap();
    let review = |line: &str, text: &str| Review { line: line.into(), shared: false, text: text.into() };
    let plan = Plan {
        review: vec![
            review("Going to Singapore in July 2026", "Went to Singapore in July 2026"),
            review("Likes tea", "Liked tea"),
        ],
        ..Plan::default()
    };
    let (kept, rejected) = audit(&snapshot, &[], plan);
    assert_eq!(kept.review.len(), 1);
    assert!(rejected[0].contains("no date"));
    let (first, _) = edits(&conn, &snapshot, &kept, &[]).unwrap();
    assert!(main_after(&first).ends_with("- Went to Singapore in July 2026\n"), "{}", main_after(&first));
    assert!(!main_after(&first).contains("Going to"));
    let rewrite = &first.added[0];
    assert_eq!(rewrite.text, "Went to Singapore in July 2026");
    let source = snapshot.fact(false, "Going to Singapore in July 2026").cloned();
    assert_eq!((&rewrite.message, &rewrite.rewrites), (&source.as_ref().unwrap().message, &source));

    // The next day's review leaves its own rewrite alone, once the caller marked it written.
    mark_reviewed(&conn, Some("a"), &rewrite.text).unwrap();
    seed(&conn, Author::Dream, main_after(&first));
    let snapshot = super::snapshot(&conn, "a", false, "2026-10-18").unwrap();
    let again = Plan { review: vec![review("Went to Singapore in July 2026", "Went to Singapore")], ..Plan::default() };
    let (kept, _) = audit(&snapshot, &[], again);
    assert_eq!(edits(&conn, &snapshot, &kept, &[]).unwrap().0.files, [] as [Edit; 0]);
}

#[test]
fn parses_the_outermost_object_and_survives_chatter() {
    let raw = "Sure!\n```json\n{\"add\": [{\"text\": \"Likes tea\", \"quote\": \"I like tea a lot\", \"message\": \"m1\"}]}\n```";
    let plan = parse(raw).unwrap();
    assert_eq!(plan.add[0].text, "Likes tea");
    assert_eq!(plan.remove, [] as [Remove; 0]);
    assert_eq!(parse("no json here"), None);
}

#[test]
fn the_prompt_numbers_messages_and_marks_the_users_lines() {
    let conn = db();
    seed(&conn, Author::User, "- Lives in Taipei\n");
    let snapshot = snapshot(&conn, "a", true, "2026-10-17").unwrap();
    let (system, material) = prompt(&snapshot, &[user("u1", "hello there friend"), bot_reply("b1", "hi")]);
    assert!(system.contains("Today is 2026-10-17"));
    assert!(
        material.contains("This bot's (\"shared\": false), written by the user:\n- Lives in Taipei\n"),
        "{material}"
    );
    assert!(material.contains("[m1] User, 2026-10-17:\nhello there friend"));
    assert!(material.contains("[m2] Bot"));
    assert!(material.contains("Notes now:\n(none)"));
    assert!(!system.contains("moveOut"), "only offered when memory is nearly full");
}

#[test]
fn notes_cite_the_user_and_lines_move_out_only_when_full() {
    let conn = db();
    seed(&conn, Author::Bot, "- Mei's teacher is Ms. Lin\n");
    let messages = [user("u1", "Mei started piano lessons on Tuesdays")];
    let append = |file: &str, description: &str| Append {
        file: file.into(),
        shared: true,
        text: "Mei has piano lessons on Tuesdays".into(),
        quote: "Mei started piano lessons".into(),
        message: "m1".into(),
        description: description.into(),
    };
    let snapshot = snapshot(&conn, "a", false, "2026-10-17").unwrap();
    let plan = Plan {
        append: vec![
            append("people.md", "The user's family"),
            append("People.md", "x"),
            append("new.md", ""),
            append("MEMORY.md", "x"),
        ],
        move_out: vec![MoveOut {
            line: "Mei's teacher is Ms. Lin".into(),
            shared: false,
            file: "people.md".into(),
            description: String::new(),
        }],
        ..Plan::default()
    };
    let (kept, rejected) = audit(&snapshot, &messages, plan);
    assert_eq!(kept.append.len(), 1);
    assert!(!kept.append[0].shared, "a bot without the shared layer keeps notes to itself");
    assert_eq!(rejected.iter().filter(|r| r.contains("lowercase")).count(), 2, "{rejected:?}");
    assert!(rejected.iter().any(|r| r.contains("description")) && rejected.iter().any(|r| r.contains("isn't full")));

    let (edits, _) = edits(&conn, &snapshot, &kept, &messages).unwrap();
    assert_eq!(
        edits.files,
        [Edit {
            bot: Some("a".into()),
            name: "people.md".into(),
            before: None,
            after: "---\ndescription: The user's family\n---\n- Mei has piano lessons on Tuesdays\n".into(),
        }]
    );
}

#[test]
fn a_full_memory_moves_detail_out_and_the_log_keeps_the_run() {
    let conn = db();
    let mut content = String::new();
    while crate::tokens::estimate(&content) * 10 < files::cap(Some("a")) * 9 {
        let next = lines([format!("Filler fact number {} about the user's week", content.lines().count())]);
        content.push_str(&next);
    }
    seed(&conn, Author::Bot, &content);
    let snapshot = snapshot(&conn, "a", false, "2026-10-17").unwrap();
    assert!(snapshot.full);
    let line = "Filler fact number 0 about the user's week";
    let plan = Plan {
        move_out: vec![MoveOut {
            line: line.into(),
            shared: false,
            file: "week.md".into(),
            description: "The user's week".into(),
        }],
        ..Plan::default()
    };
    let (kept, rejected) = audit(&snapshot, &[], plan);
    assert!(rejected.is_empty(), "{rejected:?}");
    let (edits, _) = edits(&conn, &snapshot, &kept, &[]).unwrap();
    assert!(!main_after(&edits).contains(line));
    assert_eq!(edits.files[0].name, "week.md", "the note is written before MEMORY.md loses the line");
    let week = edits.files.iter().find(|e| e.name == "week.md").unwrap();
    assert_eq!(week.after, format!("---\ndescription: The user's week\n---\n- {line}\n"));

    // A note that can't take the line leaves it in MEMORY.md.
    let crowded = format!("---\ndescription: The user's week\n---\n{}", "字".repeat(files::FILE_CAP - 10));
    let note = Change {
        bot: Some("a"),
        name: "week.md",
        content: Some(&crowded),
        author: Author::Bot,
        author_bot: Some("a"),
        cause: Cause::Edit,
        message: None,
        mtime: 1,
        size: 1,
    };
    files::record(&conn, &note, 2, 2).unwrap();
    let crowded_snapshot = super::snapshot(&conn, "a", false, "2026-10-17").unwrap();
    let (kept, _) = audit(&crowded_snapshot, &[], kept);
    let (stuck, rejected) = super::edits(&conn, &crowded_snapshot, &kept, &[]).unwrap();
    assert!(stuck.files.is_empty() && stuck.removed.is_empty(), "{stuck:?}");
    assert!(rejected.iter().any(|r| r.contains("week.md")), "{rejected:?}");

    let dream = begin(&conn, "a", 5).unwrap();
    record_line(&conn, dream, false, line).unwrap();
    let mut change = Change {
        bot: Some("a"),
        name: "week.md",
        content: Some(&week.after),
        author: Author::Dream,
        author_bot: Some("a"),
        cause: Cause::Dream,
        message: None,
        mtime: 1,
        size: 1,
    };
    let note = files::record(&conn, &change, 6, 6).unwrap();
    record_file(&conn, dream, note.id).unwrap();
    change.name = MAIN;
    change.content = Some(main_after(&edits));
    let main = files::record(&conn, &change, 6, 6).unwrap();
    record_file(&conn, dream, main.id).unwrap();
    let logged = get(&conn, dream).unwrap().unwrap();
    assert_eq!(logged.archived[0].text, line);
    assert_eq!(
        logged.files,
        [FileChange { name: "week.md".into(), shared: false, version: note.id }],
        "MEMORY.md shows as lines"
    );
    assert_eq!(recent(&conn, "a", 5).unwrap(), [logged]);

    let empty = begin(&conn, "a", 7).unwrap();
    discard_if_empty(&conn, empty).unwrap();
    assert!(get(&conn, empty).unwrap().is_none(), "a run that wrote nothing is no run");
}

#[test]
fn a_line_memory_md_keeps_doesnt_go_into_a_note_too() {
    let conn = db();
    let mut content = "- Trip on 2026-10-01\n".to_owned();
    while crate::tokens::estimate(&content) <= files::cap(Some("a")) {
        content.push_str(&lines([format!("Filler fact number {} about the user's week", content.lines().count())]));
    }
    seed(&conn, Author::Bot, &content);
    let snapshot = snapshot(&conn, "a", false, "2026-10-17").unwrap();
    let line = "Filler fact number 1 about the user's week";
    let plan = Plan {
        // The rewrite grows the file, already over its cap, by more than the move takes out of it.
        review: vec![Review {
            line: "Trip on 2026-10-01".into(),
            shared: false,
            text: "Went on a long trip on 2026-10-01 with the whole family, to the mountains and the sea and back"
                .into(),
        }],
        move_out: vec![MoveOut {
            line: line.into(),
            shared: false,
            file: "week.md".into(),
            description: "The user's week".into(),
        }],
        ..Plan::default()
    };
    let (kept, rejected) = audit(&snapshot, &[], plan);
    assert!(rejected.is_empty(), "{rejected:?}");
    let (edits, rejected) = edits(&conn, &snapshot, &kept, &[]).unwrap();
    assert!(edits.files.is_empty(), "{edits:?}");
    assert!(rejected.iter().any(|r| r.contains(line) && r.contains("wasn't changed")), "{rejected:?}");
}
