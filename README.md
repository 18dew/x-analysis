# Xanalysis — local X bookmarks archive, browser, and agent data source

A small Rust service that:

1. **Authenticates with X** using your own developer app from
   [developer.x.com](https://developer.x.com) (OAuth 2.0 + PKCE — no password
   sharing, tokens stay on your machine).
2. **Syncs every post you've bookmarked** into a local SQLite database, and
   writes agent-friendly exports (`bookmarks.jsonl` + `bookmarks.md`).
3. **Serves a local web UI** at `http://localhost:8080` to browse, search, and
   inspect all bookmarked posts — including media, quote-posts, polls, and
   engagement metrics.
4. **Finds topics by domain**: tell it a domain (e.g. `arxiv.org`) or a word
   (e.g. `rust`) and it lists the topics living inside the matching posts —
   keywords, hashtags, top links, authors — exportable as JSON/CSV/Markdown.
   Search just your bookmarks, or all of X live.

```
┌────────────┐   OAuth 2.0 PKCE   ┌─────────────────────────────┐
│ x.com  ─────────────────────────▶ ~/.xanalysis/tokens.json     │
│ (X API v2)│   GET /users/me     │ ~/.xanalysis/data/          │
│           │   GET /users/:id/   │   bookmarks.db   (SQLite)    │
│           │      bookmarks      │   bookmarks.jsonl (for LLM/ │
└────────────┘                    │                   agents)   │
                                  │   bookmarks.md   (digest)   │
      Browser ──────────────────▶ │   web UI + REST API (axum)  │
      CLI (clap) ───────────────▶ └─────────────────────────────┘
```

---

## Quickstart

```bash
cargo build --release

export PATH="$PWD/target/release:$PATH"

xanalysis init      # creates ~/.xanalysis/config.toml (edit it — see below)
xanalysis login     # opens the browser to authorize the app
xanalysis sync      # downloads all bookmarks + writes exports
xanalysis serve     # UI at http://localhost:8080
```

Everything lives under `~/.xanalysis/` (override with `XANALYSIS_HOME`):

| Path | What it is |
|---|---|
| `~/.xanalysis/config.toml` | Configuration (client ID/secret, port, …) |
| `~/.xanalysis/tokens.json` | OAuth tokens (`0600`, never leaves this machine) |
| `~/.xanalysis/data/bookmarks.db` | SQLite database (source of truth) |
| `~/.xanalysis/data/bookmarks.jsonl` | One denormalized JSON object per bookmark — **feed this to agents** |
| `~/.xanalysis/data/bookmarks.md` | Human/LLM-readable Markdown digest |

## Setup: your X developer app (one-time)

X only exposes bookmarks through its API, so you need your own (free)
developer app. It takes ~5 minutes:

1. Sign in at <https://developer.x.com> and open the
   **Developer Console → Projects & Apps → Create App**.
2. In the app's **User authentication settings**, click **Set up**:
   - **App permissions**: `Read`
   - **Type of App**: `Web App, Automated App or Bot` (a *confidential
     client* — you'll get a Client Secret) or `Native App` (*public client*,
     no secret — both work with Xanalysis)
   - **Callback URI / Redirect URL**:
     ```
     http://localhost:8080/callback
     ```
     (must match `callback_url` in `config.toml` **exactly**)
   - **Website URL**: anything, e.g. `http://localhost:8080`
3. Open the app's **Keys and tokens** tab and copy the **OAuth 2.0 Client ID**
   (and **Client Secret** for confidential clients).
4. Put them in `~/.xanalysis/config.toml`:

   ```toml
   client_id = "YOUR_OAUTH2_CLIENT_ID"
   client_secret = "YOUR_OAUTH2_CLIENT_SECRET"   # omit for public clients
   ```

   …or export `X_CLIENT_ID` / `X_CLIENT_SECRET` instead.

5. Run `xanalysis login` and authorize in the browser. That's it.

Scopes requested: `bookmark.read`, `tweet.read`, `users.read`,
`offline.access` (refresh tokens, so you stay logged in).

## Usage

```
xanalysis init                 # create config dir + template
xanalysis login                # OAuth 2.0 PKCE in the browser
xanalysis sync [--full]        # download bookmarks + regenerate exports
xanalysis serve [--no-open]     # web UI + REST API on localhost:8080
xanalysis status               # auth / storage / sync status
xanalysis export [--format jsonl|md] [--out PATH]
xanalysis logout               # delete stored tokens
```

You can also trigger a sync and a login from the web UI (Sync button /
"Connect X account" banner).

### Sync behavior & rate limits

- The bookmarks endpoint allows **~15 requests / 15 min** per user with up to
  100 posts per page (~1,500 bookmarks per 15-minute window). Xanalysis
  detects `429` responses, waits for the window to reset, and continues —
  first syncs of large libraries just take a while.
- **First sync** (or `--full`): pages through *all* bookmarks and marks ones
  you've un-bookmarked as removed.
- **Later syncs**: stop early once everything is caught up (typically 1–4
  requests).
- Access tokens expire after ~2 hours; refresh tokens are rotated
  automatically.

### Topic finder (search by domain — bookmarks or all of X)

Open the **◫ Topics** panel in the web UI (or use the CLI) and enter a domain
or topic. Two scopes are available:

- **local** (default) — analyze your bookmark archive.
- **x** — live-search **all of X** via `GET /2/tweets/search/recent` and
  analyze those results. Toggle **“Search all of X (live)”** in the panel,
  or pass `--scope x` / `scope=x`.

Input handling is the same in both scopes:

- **Domain mode** (`arxiv.org`, `https://arxiv.org/abs/…`, `www.arxiv.org` —
  anything with a dot) — matches posts whose expanded link URLs point at
  that host, plus plain-text mentions of it. Live search uses the
  `url:"arxiv.org" -is:retweet` operator.
- **Topic mode** (anything else, e.g. `rust` or `machine learning`) — matches
  whole words in post text, hashtags, mentions, and authors (no substring
  false positives: `firs` does not match `first`).

From the matching posts it extracts, with counts:

- **Keywords** — frequent words (stopwords and the needle itself excluded)
- **Hashtags**
- **Top links** — page titles of bookmarked URLs
- **Authors** — whose posts match

```bash
# CLI (great for agents)
xanalysis topics arxiv.org                     # local bookmarks, table
xanalysis topics arxiv.org --scope x           # live search across all of X
xanalysis topics rust --scope x --max-posts 100 --format csv
xanalysis topics arxiv.org --format md         # digest incl. matching posts
xanalysis topics arxiv.org --format json > topics-arxiv.json
```

In the UI: click a topic chip to search the feed for it, **Show matching
bookmarks** (local scope) to filter the feed by the domain, and **⤓ JSON /
CSV / MD** to download the list — with the scope you searched.

### Live-search notes & caveats

- Requires a connected account (`xanalysis login`) — it runs under your
  OAuth token's `tweet.read` scope.
- Uses the **pay-per-use** X API search endpoint: every search costs API
  credits under your developer account's plan. Default cap is 300 posts
  (≈ 3 requests); tune with `--max-posts` or `max_search_posts` in
  `config.toml`.
- Recent search only covers roughly the **last 7 days** of posts.
- Some tiers cap the page size below 100; Xanalysis detects that and
  automatically falls back to the tier's limit.
- Live results are analyzed in memory — nothing is written to your archive.

## Automating it

`scripts/sync_bookmarks.sh` is a cron-friendly wrapper:

```bash
*/30 * * * * "$HOME/Codebase/git/Xanalysis/scripts/sync_bookmarks.sh"
```

## Feeding bookmarks to an agent

`bookmarks.jsonl` is the main artifact for analysis — one self-contained JSON
object per line (bookmarks newest-first, `position` 0 = most recent):

```jsonc
{
  "id": "1757000000000000000",
  "url": "https://x.com/author/status/1757000000000000000",
  "text": "Full post text…",
  "created_at": "2026-11-01T12:34:56.000Z",
  "lang": "en",
  "author": { "id": "…", "username": "author", "name": "Author", "description": "…", "verified": false },
  "metrics": { "replies": 2, "reposts": 5, "quotes": 1, "likes": 40, "bookmarks": 12, "views": 9000 },
  "entities": { "urls": [/* expanded_url, title, … */], "mentions": [], "hashtags": [] },
  "media": [ { "type": "photo", "url": "https://pbs.twimg.com/…" } ],
  "polls": [],
  "referenced": [ { "type": "quoted", "id": "…" } ],
  "quoted":      { /* full quoted post, same shape, depth 1 */ },
  "retweeted":   { /* full reposted post, same shape, depth 1 */ },
  "is_reply": false,
  "possibly_sensitive": false,
  "position": 0,
  "first_seen_at": "2026-11-07T…"
}
```

Example questions an agent can answer straight from the file:

```bash
# Top bookmarked links
jq -r '.entities.urls[]?.expanded_url' ~/.xanalysis/data/bookmarks.jsonl \
  | sort | uniq -c | sort -rn | head -20

# Most-bookmarked authors
jq -r '.author.username' ~/.xanalysis/data/bookmarks.jsonl \
  | sort | uniq -c | sort -rn | head -20

# Bookmarks mentioning rust
grep -i rust ~/.xanalysis/data/bookmarks.jsonl | head
```

Or just tell your agent: *"Read `~/.xanalysis/data/bookmarks.jsonl` and
summarize the themes in my bookmarks."*

## REST API (served by `xanalysis serve`)

| Method | Path | Description |
|---|---|---|
| GET | `/api/status` | auth state, user, counts, sync progress, paths |
| GET | `/api/bookmarks?q=&limit=&offset=` | paginated, searchable bookmark list |
| GET | `/api/bookmarks/{id}` | one bookmark + raw API JSON |
| POST | `/api/sync` | start a background sync (409 if one runs) |
| POST | `/api/login/start` | begin OAuth; returns `{url}` to open |
| GET | `/callback` | OAuth redirect target (exchange happens here) |
| GET | `/api/topics?domain=arxiv.org&scope=local\|x` | topics from matching bookmarks (local) or live X search (x) |
| GET | `/api/topics/export?domain=…&scope=…&format=json\|csv\|md` | download the topic report |
| GET | `/api/export?format=jsonl\|md` | download current exports |

`/api/bookmarks` also accepts `domain=` to filter the feed. `scope=x` also
accepts `max_posts=` (default from config, max 1000).

## Troubleshooting

- **`redirect_uri` mismatch** — the callback URL in your X app settings must
  match `callback_url` byte-for-byte (including the port).
- **401/`Could not authenticate you`** — run `xanalysis login` again.
- **Sync stuck on "rate limited"** — expected on the first sync of a large
  library; watch `xanalysis status` or the UI, it will resume automatically.
- **`client_id not configured`** — set `client_id` in `~/.xanalysis/config.toml`
  or export `X_CLIENT_ID`, then restart `xanalysis serve`.
- If X renames API fields, `post_fields` / `expansions` in `config.toml`
  let you adjust the request without recompiling.

## Development

```bash
cargo test             # unit + mock-server integration tests
cargo build --release
RUST_LOG=debug xanalysis serve --no-open
```

Crates: `axum` (web), `reqwest` (HTTP), `rusqlite` (SQLite, bundled),
`clap` (CLI), `tokio`, `serde`/`serde_json`, `tracing`.
The web UI in `web/` is dependency-free vanilla JS/CSS embedded into the
binary — no build step.

## Notes

- Only **read** scopes are requested; Xanalysis can't post, like, or modify
  anything on your behalf.
- The X API is mid-migration from "tweet" to "post" terminology; the client
  requests the current field names (`note_post`, `referenced_posts`, …) and
  accepts both old and new response shapes.
- Bookmark *folders* (`/2/users/:id/bookmarks/folders`) are not supported
  yet — all bookmarks sync into a single list.
