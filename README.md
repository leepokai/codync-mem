# codync-mem

Memory for coding-agent bots, from [Codync](https://github.com/leepokai/Codync).

- **It records the person, not the work.** Project knowledge stays in the harness's own files (`AGENTS.md`,
  `CLAUDE.md`); the full conversation stays in your transcript.
- **Two layers.** "Yours" is shared by every bot; each bot also has its own. Both have token caps, and the active
  entries go into a session's opening in full.
- **One store with rules for who writes it.** The user writes directly. A bot writes through its tool, which
  follows the bot's permission mode. Every write except the user's own passes deterministic checks: no hidden
  characters, prompt-carrying links, planted instructions or secrets, and nothing the user deleted comes back.

The crate decides; the caller acts. It starts no processes, opens no network connections, calls no models and
spawns no threads. It keeps its own tables on the caller's SQLite connection and writes inside the caller's
transaction, so a memory change commits together with whatever the caller records alongside it.

## Status

0.x, developed inside Codync and mirrored here read-only. The API changes with Codync. Report issues here;
send pull requests to [Codync's `dev` branch](https://github.com/leepokai/Codync) (`host/crates/memory`).

## Use

```rust
use codync_mem::rusqlite::Connection;

let mut conn = Connection::open_in_memory()?;
codync_mem::schema::migrate(&conn)?;
```

The crate re-exports the `rusqlite` it builds against: use that one, because a build can link only one
`rusqlite` to `sqlite3`.

## Modules

- `store`: entries in two layers, the user's and a bot's writes, archive, forget (with tombstones), search.
- `check`: the deterministic checks every write but the user's own passes.
- `opening`: the memory a session opens with, within the caps, rendered for the instructions.
- `files`: long-form notes, one Markdown file per topic. You keep the files; the crate keeps every version
  (content, author, cause), checks a bot's change, diffs it, and renders the index an opening lists.
- `dream`: between turns, a job for the user's own model (`prompt`), the rules that audit its answer (`audit`,
  quotes of the user's own words only), and the logged, undoable write (`apply`), plus the lines it adds to notes.
- `evidence`: what counts as the user's own words (not pasted, quoted or forwarded text).
- `compaction`: when to summarize a conversation ahead of time, the loop guard, the keep-list and the handoff to
  a fresh conversation.
- `tokens`: the token estimate the caps count in (one per CJK character, one per four others).

## License

Apache-2.0
