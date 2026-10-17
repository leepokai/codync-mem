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

## License

Apache-2.0
