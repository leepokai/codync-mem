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

#[test]
fn a_forgotten_memory_isnt_learned_again_from_the_same_message() {
    let conn = db();
    let from = |text: &'static str, message: &str| New { evidence: vec![message.to_owned()].leak(), ..new(None, text) };
    let mine = |text: &'static str, message: &str| New { bot: Some("a"), ..from(text, message) };
    let entry = add(&conn, Writer::Bot("a"), mine("我養了一隻柴犬叫麻糬", "m1"), 1, 1).unwrap();
    forget(&conn, entry.id).unwrap();
    let reworded = add(&conn, Writer::Dream("a"), from("養了一隻柴犬叫麻糬", "m1"), 2, 2);
    assert!(matches!(reworded, Err(Error::Refused(Refusal::Forgotten))), "nor in the other layer");
    // A later message may still say it, and the user may always write it.
    add(&conn, Writer::Dream("a"), from("養了一隻柴犬叫麻糬", "m2"), 3, 3).unwrap();
    forbid(&conn, None, "Works late", Some("m3")).unwrap();
    assert!(matches!(add(&conn, Writer::Bot("a"), from("Likes tea", "m3"), 4, 4), Err(Error::Refused(_))));
    add(&conn, Writer::User, from("Likes tea", "m3"), 5, 5).unwrap();
}

#[test]
fn dreaming_leaves_an_entry_the_user_brought_back() {
    let conn = db();
    let entry = add(&conn, Writer::Bot("a"), new(Some("a"), "Gets up at 7"), 1, 1).unwrap();
    archive(&conn, Writer::Bot("a"), entry.id, None, 2, 2).unwrap();
    bring_back(&conn, entry.id, 3, 3).unwrap();
    let again = archive(&conn, Writer::Dream("a"), entry.id, None, 4, 4);
    assert!(matches!(again, Err(Error::UsersOwn)));
    // The bot may still archive it when the user asks in chat.
    assert_eq!(archive(&conn, Writer::Bot("a"), entry.id, None, 5, 5).unwrap().state, State::Archived);
}
