use crate::models::{Media, Tweet, User};
use anyhow::Result;
use rusqlite::{params, params_from_iter, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashSet;
use std::io::Write;
use std::path::Path;
use std::sync::Mutex;

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS tweets (
    id TEXT PRIMARY KEY,
    author_id TEXT,
    text TEXT NOT NULL,
    created_at TEXT,
    lang TEXT,
    source TEXT,
    conversation_id TEXT,
    in_reply_to_user_id TEXT,
    possibly_sensitive INTEGER NOT NULL DEFAULT 0,
    metrics_json TEXT,
    entities_json TEXT,
    attachments_json TEXT,
    referenced_json TEXT,
    withheld_json TEXT,
    raw_json TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS users (
    id TEXT PRIMARY KEY,
    username TEXT,
    name TEXT,
    description TEXT,
    profile_image_url TEXT,
    verified INTEGER NOT NULL DEFAULT 0,
    verified_type TEXT,
    protected INTEGER NOT NULL DEFAULT 0,
    location TEXT,
    url TEXT,
    created_at TEXT,
    metrics_json TEXT,
    raw_json TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS media (
    media_key TEXT PRIMARY KEY,
    tweet_id TEXT NOT NULL,
    type TEXT,
    url TEXT,
    preview_image_url TEXT,
    alt_text TEXT,
    width INTEGER,
    height INTEGER,
    duration_ms INTEGER,
    variants_json TEXT,
    raw_json TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_media_tweet ON media(tweet_id);

CREATE TABLE IF NOT EXISTS polls (
    id TEXT PRIMARY KEY,
    tweet_id TEXT NOT NULL,
    raw_json TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_polls_tweet ON polls(tweet_id);

CREATE TABLE IF NOT EXISTS bookmarks (
    tweet_id TEXT PRIMARY KEY,
    position INTEGER NOT NULL,
    first_seen_at TEXT NOT NULL,
    last_seen_at TEXT NOT NULL,
    removed_at TEXT
);
CREATE INDEX IF NOT EXISTS idx_bookmarks_position ON bookmarks(position);

CREATE TABLE IF NOT EXISTS meta (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
"#;

pub struct Store {
    conn: Mutex<Connection>,
}

pub fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339()
}

fn escape_like(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

fn jstr(v: &Option<Value>) -> Option<String> {
    v.as_ref().map(|x| x.to_string())
}

// ---------- Denormalized views used by the API and exports ----------

#[derive(Debug, Clone, Serialize)]
pub struct AuthorView {
    pub id: String,
    pub username: Option<String>,
    pub name: Option<String>,
    pub description: Option<String>,
    pub profile_image_url: Option<String>,
    pub verified: bool,
    pub verified_type: Option<String>,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct MetricsView {
    pub replies: u64,
    pub reposts: u64,
    pub quotes: u64,
    pub likes: u64,
    pub bookmarks: u64,
    pub views: Option<u64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct VariantView {
    pub bit_rate: Option<u64>,
    pub content_type: Option<String>,
    pub url: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct MediaView {
    #[serde(rename = "type")]
    pub kind: String,
    pub url: Option<String>,
    pub preview_image_url: Option<String>,
    pub alt_text: Option<String>,
    pub width: Option<u64>,
    pub height: Option<u64>,
    pub duration_ms: Option<u64>,
    pub variants: Vec<VariantView>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RefView {
    #[serde(rename = "type")]
    pub kind: String,
    pub id: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct BookmarkView {
    pub id: String,
    pub url: String,
    pub text: String,
    pub created_at: Option<String>,
    pub lang: Option<String>,
    pub author: Option<AuthorView>,
    pub metrics: MetricsView,
    pub media: Vec<MediaView>,
    pub polls: Vec<Value>,
    pub entities: Option<Value>,
    pub referenced: Vec<RefView>,
    pub quoted: Option<Box<BookmarkView>>,
    pub retweeted: Option<Box<BookmarkView>>,
    pub is_reply: bool,
    pub possibly_sensitive: bool,
    pub position: Option<i64>,
    pub first_seen_at: Option<String>,
    pub removed_at: Option<String>,
}

/// How many levels of quoted/reposted context to resolve inline.
const MAX_DEPTH: u32 = 1;

struct RowData {
    id: String,
    text: String,
    created_at: Option<String>,
    lang: Option<String>,
    possibly_sensitive: bool,
    in_reply_to_user_id: Option<String>,
    entities_json: Option<String>,
    metrics_json: Option<String>,
    referenced_json: Option<String>,
    position: Option<i64>,
    first_seen_at: Option<String>,
    removed_at: Option<String>,
    author: Option<AuthorView>,
}

const TWEET_COLUMNS: &str = "t.id, t.text, t.created_at, t.lang, t.possibly_sensitive, \
t.in_reply_to_user_id, t.entities_json, t.metrics_json, t.referenced_json, \
u.id, u.username, u.name, u.description, u.profile_image_url, u.verified, u.verified_type";

fn row_data(row: &rusqlite::Row<'_>) -> rusqlite::Result<RowData> {
    // Column order must match TWEET_COLUMNS: u.verified is 14, u.verified_type is 15.
    let verified: i64 = row.get::<_, Option<i64>>(14)?.unwrap_or(0);
    let author = match row.get::<_, Option<String>>(9)? {
        Some(id) => Some(AuthorView {
            id,
            username: row.get(10)?,
            name: row.get(11)?,
            description: row.get(12)?,
            profile_image_url: row.get(13)?,
            verified: verified != 0,
            verified_type: row.get(15)?,
        }),
        None => None,
    };
    Ok(RowData {
        id: row.get(0)?,
        text: row.get(1)?,
        created_at: row.get(2)?,
        lang: row.get(3)?,
        possibly_sensitive: row.get::<_, i64>(4)? != 0,
        in_reply_to_user_id: row.get(5)?,
        entities_json: row.get(6)?,
        metrics_json: row.get(7)?,
        referenced_json: row.get(8)?,
        position: None,
        first_seen_at: None,
        removed_at: None,
        author,
    })
}

impl Store {
    pub fn open(path: &Path) -> Result<Store> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path)?;
        conn.execute_batch("PRAGMA journal_mode=WAL;")?;
        conn.execute_batch(SCHEMA)?;
        Ok(Store {
            conn: Mutex::new(conn),
        })
    }

    /// Ephemeral store used by the live-search topic finder: same schema and
    /// view-building code, but nothing is written to disk.
    pub fn open_in_memory() -> Result<Store> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(SCHEMA)?;
        Ok(Store {
            conn: Mutex::new(conn),
        })
    }

    // ---------- Writes ----------

    pub fn upsert_tweet(&self, t: &Tweet, raw: &Value) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let metrics = t.public_metrics.clone().map(|m| serde_json::to_string(&m));
        conn.execute(
            "INSERT INTO tweets (id, author_id, text, created_at, lang, source, conversation_id, \
             in_reply_to_user_id, possibly_sensitive, metrics_json, entities_json, attachments_json, \
             referenced_json, withheld_json, raw_json, updated_at)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16)
             ON CONFLICT(id) DO UPDATE SET
               author_id=excluded.author_id, text=excluded.text, created_at=excluded.created_at,
               lang=excluded.lang, source=excluded.source, conversation_id=excluded.conversation_id,
               in_reply_to_user_id=excluded.in_reply_to_user_id,
               possibly_sensitive=excluded.possibly_sensitive, metrics_json=excluded.metrics_json,
               entities_json=excluded.entities_json, attachments_json=excluded.attachments_json,
               referenced_json=excluded.referenced_json, withheld_json=excluded.withheld_json,
               raw_json=excluded.raw_json, updated_at=excluded.updated_at",
            params![
                t.id,
                t.author_id,
                t.full_text(),
                t.created_at,
                t.lang,
                t.source,
                t.conversation_id,
                t.in_reply_to_user_id,
                t.possibly_sensitive.unwrap_or(false) as i64,
                metrics.transpose()?,
                jstr(&t.full_entities()),
                jstr(&t.attachments),
                serde_json::to_string(&t.references()).ok(),
                jstr(&t.withheld),
                raw.to_string(),
                now_rfc3339(),
            ],
        )?;
        Ok(())
    }

    pub fn upsert_user(&self, u: &User, raw: &Value) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO users (id, username, name, description, profile_image_url, verified, \
             verified_type, protected, location, url, created_at, metrics_json, raw_json, updated_at)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)
             ON CONFLICT(id) DO UPDATE SET
               username=excluded.username, name=excluded.name, description=excluded.description,
               profile_image_url=excluded.profile_image_url, verified=excluded.verified,
               verified_type=excluded.verified_type, protected=excluded.protected,
               location=excluded.location, url=excluded.url, created_at=excluded.created_at,
               metrics_json=excluded.metrics_json, raw_json=excluded.raw_json,
               updated_at=excluded.updated_at",
            params![
                u.id,
                u.username,
                u.name,
                u.description,
                u.profile_image_url,
                u.verified.unwrap_or(false) as i64,
                u.verified_type,
                u.protected.unwrap_or(false) as i64,
                u.location,
                u.url,
                u.created_at,
                jstr(&u.public_metrics),
                raw.to_string(),
                now_rfc3339(),
            ],
        )?;
        Ok(())
    }

    pub fn upsert_media(&self, m: &Media, tweet_id: &str, raw: &Value) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO media (media_key, tweet_id, type, url, preview_image_url, alt_text, \
             width, height, duration_ms, variants_json, raw_json)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)
             ON CONFLICT(media_key) DO UPDATE SET
               tweet_id=excluded.tweet_id, type=excluded.type, url=excluded.url,
               preview_image_url=excluded.preview_image_url, alt_text=excluded.alt_text,
               width=excluded.width, height=excluded.height, duration_ms=excluded.duration_ms,
               variants_json=excluded.variants_json, raw_json=excluded.raw_json",
            params![
                m.media_key,
                tweet_id,
                m.kind,
                m.url,
                m.preview_image_url,
                m.alt_text,
                m.width,
                m.height,
                m.duration_ms,
                m.variants
                    .as_ref()
                    .and_then(|v| serde_json::to_string(v).ok()),
                raw.to_string(),
            ],
        )?;
        Ok(())
    }

    pub fn upsert_poll(&self, poll: &Value, tweet_id: &str) -> Result<()> {
        let Some(id) = poll.get("id").and_then(|i| i.as_str()) else {
            return Ok(());
        };
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO polls (id, tweet_id, raw_json) VALUES (?1,?2,?3)
             ON CONFLICT(id) DO UPDATE SET tweet_id=excluded.tweet_id, raw_json=excluded.raw_json",
            params![id, tweet_id, poll.to_string()],
        )?;
        Ok(())
    }

    /// Insert/update a bookmark position. Returns true if it already existed.
    pub fn upsert_bookmark(&self, tweet_id: &str, position: i64, now: &str) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let existed: Option<i64> = conn
            .query_row(
                "SELECT 1 FROM bookmarks WHERE tweet_id = ?1",
                params![tweet_id],
                |r| r.get(0),
            )
            .optional()?;
        conn.execute(
            "INSERT INTO bookmarks (tweet_id, position, first_seen_at, last_seen_at)
             VALUES (?1,?2,?3,?3)
             ON CONFLICT(tweet_id) DO UPDATE SET
               position=excluded.position, last_seen_at=excluded.last_seen_at, removed_at=NULL",
            params![tweet_id, position, now],
        )?;
        Ok(existed.is_some())
    }

    /// After a complete pass, mark bookmarks that were not seen as removed.
    pub fn mark_removed(&self, seen: &HashSet<String>, now: &str) -> Result<usize> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT tweet_id FROM bookmarks WHERE removed_at IS NULL")?;
        let ids: Vec<String> = stmt
            .query_map([], |r| r.get(0))?
            .collect::<std::result::Result<_, _>>()?;
        drop(stmt);

        let to_remove: Vec<&String> = ids.iter().filter(|id| !seen.contains(*id)).collect();
        if to_remove.is_empty() {
            return Ok(0);
        }
        let mut stmt = conn.prepare("UPDATE bookmarks SET removed_at = ?1 WHERE tweet_id = ?2")?;
        for id in &to_remove {
            stmt.execute(params![now, id])?;
        }
        Ok(to_remove.len())
    }

    // ---------- Meta ----------

    pub fn set_meta(&self, key: &str, value: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO meta (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    pub fn get_meta(&self, key: &str) -> Result<Option<String>> {
        let conn = self.conn.lock().unwrap();
        let v = conn
            .query_row("SELECT value FROM meta WHERE key = ?1", params![key], |r| {
                r.get::<_, String>(0)
            })
            .optional()?;
        Ok(v)
    }

    pub fn count_bookmarks(&self) -> Result<i64> {
        let conn = self.conn.lock().unwrap();
        let n = conn.query_row(
            "SELECT COUNT(*) FROM bookmarks WHERE removed_at IS NULL",
            [],
            |r| r.get(0),
        )?;
        Ok(n)
    }

    // ---------- Reads ----------

    /// List bookmarks with optional text search (`q`) and optional domain
    /// filter (`domain`, matched broadly against post text, URL entities, and
    /// attachments — callers wanting precise matching use `find_candidates`
    /// plus `topics::matches`).
    pub fn list_bookmarks(
        &self,
        q: Option<&str>,
        domain: Option<&str>,
        limit: i64,
        offset: i64,
    ) -> Result<(Vec<BookmarkView>, i64)> {
        let conn = self.conn.lock().unwrap();

        let mut binds: Vec<String> = Vec::new();
        let mut where_sql = String::from("b.removed_at IS NULL");

        if let Some(q) = q.map(str::trim).filter(|s| !s.is_empty()) {
            binds.push(format!("%{}%", escape_like(q)));
            let n = binds.len();
            where_sql.push_str(&format!(
                " AND (t.text LIKE ?{n} ESCAPE '\\' OR u.name LIKE ?{n} ESCAPE '\\'
                     OR u.username LIKE ?{n} ESCAPE '\\')"
            ));
        }
        if let Some(d) = domain.map(str::trim).filter(|s| !s.is_empty()) {
            binds.push(format!("%{}%", escape_like(&crate::topics::normalize(d))));
            let n = binds.len();
            where_sql.push_str(&format!(
                " AND (t.text LIKE ?{n} ESCAPE '\\' OR t.entities_json LIKE ?{n} ESCAPE '\\'
                     OR t.attachments_json LIKE ?{n} ESCAPE '\\')"
            ));
        }

        let count_sql = format!(
            "SELECT COUNT(*) FROM bookmarks b
             JOIN tweets t ON t.id = b.tweet_id
             LEFT JOIN users u ON u.id = t.author_id
             WHERE {where_sql}"
        );
        let total: i64 =
            conn.query_row(&count_sql, params_from_iter(binds.iter()), |r| r.get(0))?;

        binds.push(limit.to_string());
        let limit_n = binds.len();
        binds.push(offset.to_string());
        let offset_n = binds.len();
        let sql = format!(
            "SELECT {TWEET_COLUMNS}, b.position, b.first_seen_at, b.removed_at
             FROM bookmarks b
             JOIN tweets t ON t.id = b.tweet_id
             LEFT JOIN users u ON u.id = t.author_id
             WHERE {where_sql}
             ORDER BY b.position ASC LIMIT ?{limit_n} OFFSET ?{offset_n}"
        );

        let mut stmt = conn.prepare(&sql)?;
        let mut rows = stmt.query(params_from_iter(binds.iter()))?;

        let mut items = Vec::new();
        while let Some(row) = rows.next()? {
            let mut data = row_data(row)?;
            data.position = row.get(16)?;
            data.first_seen_at = row.get(17)?;
            data.removed_at = row.get(18)?;
            if let Some(view) = view_from(&conn, data, 0)? {
                items.push(view);
            }
        }
        Ok((items, total))
    }

    /// Broad candidate rows for topic analysis: any bookmark whose text,
    /// entities, or author mentions the needle. The caller applies precise
    /// matching (`crate::topics::matches`) on the result.
    pub fn find_candidates(&self, needle: &str, max: i64) -> Result<Vec<BookmarkView>> {
        let conn = self.conn.lock().unwrap();
        let pattern = format!("%{}%", escape_like(needle));
        let sql = format!(
            "SELECT {TWEET_COLUMNS}, b.position, b.first_seen_at, b.removed_at
             FROM bookmarks b
             JOIN tweets t ON t.id = b.tweet_id
             LEFT JOIN users u ON u.id = t.author_id
             WHERE b.removed_at IS NULL
               AND (t.text LIKE ?1 ESCAPE '\\' OR t.entities_json LIKE ?1 ESCAPE '\\'
                    OR u.username LIKE ?1 ESCAPE '\\' OR u.name LIKE ?1 ESCAPE '\\')
             ORDER BY b.position ASC LIMIT ?2"
        );
        let mut stmt = conn.prepare(&sql)?;
        let mut rows = stmt.query(params![pattern, max])?;
        let mut items = Vec::new();
        while let Some(row) = rows.next()? {
            let mut data = row_data(row)?;
            data.position = row.get(16)?;
            data.first_seen_at = row.get(17)?;
            data.removed_at = row.get(18)?;
            if let Some(view) = view_from(&conn, data, 0)? {
                items.push(view);
            }
        }
        Ok(items)
    }

    /// Single bookmark/tweet view plus its raw API JSON (for the detail view).
    pub fn get_view(&self, id: &str) -> Result<Option<(BookmarkView, Value)>> {
        let conn = self.conn.lock().unwrap();
        let sql = format!(
            "SELECT {TWEET_COLUMNS}, b.position, b.first_seen_at, b.removed_at, t.raw_json
             FROM tweets t
             LEFT JOIN users u ON u.id = t.author_id
             LEFT JOIN bookmarks b ON b.tweet_id = t.id
             WHERE t.id = ?1"
        );
        let mut stmt = conn.prepare(&sql)?;
        let mut rows = stmt.query(params![id])?;
        if let Some(row) = rows.next()? {
            let mut data = row_data(row)?;
            data.position = row.get(16)?;
            data.first_seen_at = row.get(17)?;
            data.removed_at = row.get(18)?;
            let raw: String = row.get(19)?;
            if let Some(view) = view_from(&conn, data, 0)? {
                return Ok(Some((view, serde_json::from_str(&raw)?)));
            }
        }
        Ok(None)
    }

    // ---------- Exports ----------

    /// Write all bookmarks as JSON Lines (one denormalized JSON object per
    /// line) — the primary format for agent analysis.
    pub fn export_jsonl(&self, w: &mut dyn Write) -> Result<usize> {
        let conn = self.conn.lock().unwrap();
        let sql = format!(
            "SELECT {TWEET_COLUMNS}, b.position, b.first_seen_at, b.removed_at
             FROM bookmarks b
             JOIN tweets t ON t.id = b.tweet_id
             LEFT JOIN users u ON u.id = t.author_id
             WHERE b.removed_at IS NULL
             ORDER BY b.position ASC"
        );
        let mut stmt = conn.prepare(&sql)?;
        let mut rows = stmt.query([])?;
        let mut n = 0;
        while let Some(row) = rows.next()? {
            let mut data = row_data(row)?;
            data.position = row.get(16)?;
            data.first_seen_at = row.get(17)?;
            data.removed_at = row.get(18)?;
            if let Some(view) = view_from(&conn, data, 0)? {
                writeln!(w, "{}", serde_json::to_string(&view)?)?;
                n += 1;
            }
        }
        Ok(n)
    }

    /// Write all bookmarks as a readable Markdown digest.
    pub fn export_md(&self, w: &mut dyn Write) -> Result<usize> {
        let conn = self.conn.lock().unwrap();
        writeln!(w, "# X Bookmarks export\n\nGenerated: {}\n", now_rfc3339())?;
        let sql = format!(
            "SELECT {TWEET_COLUMNS}, b.position, b.first_seen_at, b.removed_at
             FROM bookmarks b
             JOIN tweets t ON t.id = b.tweet_id
             LEFT JOIN users u ON u.id = t.author_id
             WHERE b.removed_at IS NULL
             ORDER BY b.position ASC"
        );
        let mut stmt = conn.prepare(&sql)?;
        let mut rows = stmt.query([])?;
        let mut n = 0;
        while let Some(row) = rows.next()? {
            let mut data = row_data(row)?;
            data.position = row.get(16)?;
            data.first_seen_at = row.get(17)?;
            data.removed_at = row.get(18)?;
            let Some(view) = view_from(&conn, data, 0)? else {
                continue;
            };
            write_md_entry(w, &view)?;
            n += 1;
        }
        writeln!(w, "\n---\n\n_Total: {n} bookmarks_\n")?;
        Ok(n)
    }

    pub fn write_exports(&self, jsonl_path: &Path, md_path: &Path) -> Result<usize> {
        if let Some(parent) = jsonl_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut jf = std::fs::File::create(jsonl_path)?;
        let n = self.export_jsonl(&mut jf)?;
        let mut mf = std::fs::File::create(md_path)?;
        self.export_md(&mut mf)?;
        Ok(n)
    }
}

fn tweet_url(username: Option<&str>, id: &str) -> String {
    match username {
        Some(u) => format!("https://x.com/{u}/status/{id}"),
        None => format!("https://x.com/i/web/status/{id}"),
    }
}

fn parse_metrics(metrics_json: Option<&str>) -> MetricsView {
    let mut view = MetricsView::default();
    let Some(json) = metrics_json else {
        return view;
    };
    let Ok(v) = serde_json::from_str::<Value>(json) else {
        return view;
    };
    let get = |k: &str| v.get(k).and_then(|x| x.as_u64());
    view.replies = get("reply_count").unwrap_or(0);
    view.reposts = get("repost_count").or(get("retweet_count")).unwrap_or(0);
    view.quotes = get("quote_count").unwrap_or(0);
    view.likes = get("like_count").unwrap_or(0);
    view.bookmarks = get("bookmark_count").unwrap_or(0);
    view.views = get("impression_count");
    view
}

fn load_media(conn: &Connection, tweet_id: &str) -> Result<Vec<MediaView>> {
    let mut stmt = conn.prepare(
        "SELECT type, url, preview_image_url, alt_text, width, height, duration_ms, variants_json
         FROM media WHERE tweet_id = ?1 ORDER BY rowid",
    )?;
    let rows = stmt.query_map(params![tweet_id], |r| {
        let variants: Option<String> = r.get(7)?;
        Ok(MediaView {
            kind: r.get::<_, Option<String>>(0)?.unwrap_or_default(),
            url: r.get(1)?,
            preview_image_url: r.get(2)?,
            alt_text: r.get(3)?,
            width: r.get(4)?,
            height: r.get(5)?,
            duration_ms: r.get(6)?,
            variants: variants
                .and_then(|s| serde_json::from_str::<Vec<crate::models::Variant>>(&s).ok())
                .map(|vs| {
                    vs.into_iter()
                        .map(|v| VariantView {
                            bit_rate: v.bit_rate,
                            content_type: v.content_type,
                            url: v.url,
                        })
                        .collect()
                })
                .unwrap_or_default(),
        })
    })?;
    Ok(rows.filter_map(|r| r.ok()).collect())
}

fn load_polls(conn: &Connection, tweet_id: &str) -> Result<Vec<Value>> {
    let mut stmt = conn.prepare("SELECT raw_json FROM polls WHERE tweet_id = ?1")?;
    let rows = stmt.query_map(params![tweet_id], |r| r.get::<_, String>(0))?;
    Ok(rows
        .filter_map(|r| r.ok())
        .filter_map(|s| serde_json::from_str(&s).ok())
        .collect())
}

fn load_refs(referenced_json: Option<&str>) -> Vec<RefView> {
    referenced_json
        .and_then(|s| serde_json::from_str::<Vec<RefView>>(s).ok())
        .unwrap_or_default()
}

fn load_ref_view(conn: &Connection, id: &str, depth: u32) -> Result<Option<BookmarkView>> {
    if depth > MAX_DEPTH {
        return Ok(None);
    }
    let sql = format!(
        "SELECT {TWEET_COLUMNS}, NULL, NULL, NULL
         FROM tweets t
         LEFT JOIN users u ON u.id = t.author_id
         WHERE t.id = ?1"
    );
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query(params![id])?;
    if let Some(row) = rows.next()? {
        let data = row_data(row)?;
        return view_from(conn, data, depth);
    }
    Ok(None)
}

fn view_from(conn: &Connection, data: RowData, depth: u32) -> Result<Option<BookmarkView>> {
    let username = data.author.as_ref().and_then(|a| a.username.clone());
    let url = tweet_url(username.as_deref(), &data.id);
    let metrics = parse_metrics(data.metrics_json.as_deref());
    let entities = data
        .entities_json
        .as_deref()
        .and_then(|s| serde_json::from_str::<Value>(s).ok());
    let referenced = load_refs(data.referenced_json.as_deref());
    let media = load_media(conn, &data.id)?;
    let polls = load_polls(conn, &data.id)?;

    let mut quoted = None;
    let mut retweeted = None;
    for r in &referenced {
        if depth < MAX_DEPTH {
            if r.kind == "quoted" && quoted.is_none() {
                quoted = load_ref_view(conn, &r.id, depth + 1)?.map(Box::new);
            } else if r.kind == "retweeted" && retweeted.is_none() {
                retweeted = load_ref_view(conn, &r.id, depth + 1)?.map(Box::new);
            }
        }
    }

    Ok(Some(BookmarkView {
        id: data.id,
        url,
        text: data.text,
        created_at: data.created_at,
        lang: data.lang,
        author: data.author,
        metrics,
        media,
        polls,
        entities,
        referenced,
        quoted,
        retweeted,
        is_reply: data.in_reply_to_user_id.is_some(),
        possibly_sensitive: data.possibly_sensitive,
        position: data.position,
        first_seen_at: data.first_seen_at,
        removed_at: data.removed_at,
    }))
}

fn write_md_entry(w: &mut dyn Write, v: &BookmarkView) -> Result<()> {
    let author = v
        .author
        .as_ref()
        .map(|a| format!("@{}", a.username.as_deref().unwrap_or("?")))
        .unwrap_or_else(|| "unknown".to_string());
    let date = v
        .created_at
        .as_deref()
        .map(|d| d.to_string())
        .unwrap_or_else(|| "unknown date".to_string());
    writeln!(w, "\n---\n")?;
    writeln!(w, "## {}. {author} — {date}\n", v.position.unwrap_or(0) + 1)?;
    for line in v.text.lines() {
        writeln!(w, "> {line}")?;
    }
    writeln!(w)?;
    if let Some(q) = &v.quoted {
        let qauthor = q
            .author
            .as_ref()
            .and_then(|a| a.username.clone())
            .unwrap_or_else(|| "?".to_string());
        writeln!(w, "\n> **Quoting @{qauthor}:**")?;
        for line in q.text.lines() {
            writeln!(w, "> > {line}")?;
        }
        writeln!(w)?;
    }
    let m = &v.metrics;
    let views = m
        .views
        .map(|v| v.to_string())
        .unwrap_or_else(|| "?".to_string());
    writeln!(
        w,
        "- Engagement: {} likes · {} reposts · {} replies · {} quotes · {} views",
        m.likes, m.reposts, m.replies, m.quotes, views
    )?;
    if let Some(entities) = &v.entities {
        let urls: Vec<String> = entities
            .get("urls")
            .and_then(|u| u.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|u| {
                        u.get("expanded_url")
                            .or_else(|| u.get("url"))
                            .and_then(|x| x.as_str())
                            .map(|s| s.to_string())
                    })
                    .collect()
            })
            .unwrap_or_default();
        if !urls.is_empty() {
            writeln!(w, "- Links: {}", urls.join(" | "))?;
        }
    }
    let media: Vec<String> = v
        .media
        .iter()
        .map(|m| {
            let link = m
                .url
                .as_deref()
                .or(m.preview_image_url.as_deref())
                .unwrap_or("");
            format!("[{} {m:?}]({link})", m.kind)
        })
        .collect();
    if !media.is_empty() {
        writeln!(w, "- Media: {}", media.join(" | "))?;
    }
    writeln!(w, "- Original: {}", v.url)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn bookmark_roundtrip_and_search() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(&tmp.path().join("test.db")).unwrap();

        let raw = json!({"id": "1", "text": "hello world", "author_id": "7"});
        let tweet: Tweet = serde_json::from_value(raw.clone()).unwrap();
        store.upsert_tweet(&tweet, &raw).unwrap();

        let u_raw = json!({"id": "7", "username": "alice", "name": "Alice"});
        let user: User = serde_json::from_value(u_raw.clone()).unwrap();
        store.upsert_user(&user, &u_raw).unwrap();

        store
            .upsert_bookmark("1", 0, "2026-01-01T00:00:00Z")
            .unwrap();

        let (items, total) = store.list_bookmarks(None, None, 10, 0).unwrap();
        assert_eq!(total, 1);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].text, "hello world");
        assert_eq!(
            items[0].author.as_ref().unwrap().username.as_deref(),
            Some("alice")
        );
        assert_eq!(items[0].url, "https://x.com/alice/status/1");

        // Search matches text and username.
        let (hits, t1) = store.list_bookmarks(Some("hello"), None, 10, 0).unwrap();
        assert_eq!(t1, 1);
        assert_eq!(hits.len(), 1);
        let (_, t2) = store.list_bookmarks(Some("alice"), None, 10, 0).unwrap();
        assert_eq!(t2, 1);
        let (_, t3) = store.list_bookmarks(Some("nothing"), None, 10, 0).unwrap();
        assert_eq!(t3, 0);
        // Domain filter: none of these rows mention a URL domain.
        let (_, t4) = store
            .list_bookmarks(None, Some("arxiv.org"), 10, 0)
            .unwrap();
        assert_eq!(t4, 0);

        // Detail includes raw JSON.
        let (view, raw_out) = store.get_view("1").unwrap().unwrap();
        assert_eq!(view.id, "1");
        assert_eq!(raw_out["text"], "hello world");

        // Marking removed prunes everything not seen.
        let mut seen = HashSet::new();
        seen.insert("1".to_string());
        assert_eq!(store.mark_removed(&seen, "x").unwrap(), 0);
        assert_eq!(store.count_bookmarks().unwrap(), 1);
        let seen_empty = HashSet::new();
        assert_eq!(store.mark_removed(&seen_empty, "x").unwrap(), 1);
        assert_eq!(store.count_bookmarks().unwrap(), 0);

        // Exports reflect remaining rows.
        let mut out: Vec<u8> = Vec::new();
        assert_eq!(store.export_jsonl(&mut out).unwrap(), 0);
        assert!(out.is_empty());
    }

    #[test]
    fn media_and_quoted_resolution() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(&tmp.path().join("test.db")).unwrap();

        let quoted_raw = json!({"id": "5", "text": "quoted text", "author_id": "9"});
        let quoted: Tweet = serde_json::from_value(quoted_raw.clone()).unwrap();
        store.upsert_tweet(&quoted, &quoted_raw).unwrap();

        let raw = json!({
            "id": "1", "text": "look at this",
            "author_id": "7",
            "referenced_posts": [{"type": "quoted", "id": "5"}],
            "attachments": {"media_keys": ["mk1"]}
        });
        let tweet: Tweet = serde_json::from_value(raw.clone()).unwrap();
        store.upsert_tweet(&tweet, &raw).unwrap();

        let u_raw = json!({"id": "9", "username": "bob", "name": "Bob"});
        let user: User = serde_json::from_value(u_raw.clone()).unwrap();
        store.upsert_user(&user, &u_raw).unwrap();

        let m_raw = json!({"media_key": "mk1", "type": "photo", "url": "https://img"});
        let media: Media = serde_json::from_value(m_raw.clone()).unwrap();
        store.upsert_media(&media, "1", &m_raw).unwrap();

        store.upsert_bookmark("1", 0, "t").unwrap();

        let (items, _) = store.list_bookmarks(None, None, 10, 0).unwrap();
        let it = &items[0];
        assert_eq!(it.media.len(), 1);
        assert_eq!(it.media[0].kind, "photo");
        let q = it.quoted.as_ref().unwrap();
        assert_eq!(q.text, "quoted text");
        assert_eq!(q.author.as_ref().unwrap().username.as_deref(), Some("bob"));

        let mut out: Vec<u8> = Vec::new();
        let n = store.export_jsonl(&mut out).unwrap();
        assert_eq!(n, 1);
        let parsed: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(parsed["quoted"]["text"], "quoted text");
        assert_eq!(parsed["media"][0]["type"], "photo");
    }

    #[test]
    fn finds_candidates_and_domain_filter() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(&tmp.path().join("test.db")).unwrap();

        let linked = json!({
            "id": "1", "text": "great paper",
            "entities": {"urls": [{"expanded_url": "https://arxiv.org/abs/1706.03762"}]}
        });
        let tweet: Tweet = serde_json::from_value(linked.clone()).unwrap();
        store.upsert_tweet(&tweet, &linked).unwrap();
        store.upsert_bookmark("1", 0, "t").unwrap();

        let other = json!({"id": "2", "text": "cooking thread"});
        let tweet2: Tweet = serde_json::from_value(other.clone()).unwrap();
        store.upsert_tweet(&tweet2, &other).unwrap();
        store.upsert_bookmark("2", 1, "t").unwrap();

        // Broad candidate fetch by domain needle finds only the linked one.
        let candidates = store.find_candidates("arxiv.org", 100).unwrap();
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].id, "1");
        assert!(crate::topics::matches(&candidates[0], "arxiv.org"));

        // Feed-level domain filter agrees.
        let (items, total) = store
            .list_bookmarks(None, Some("arxiv.org"), 10, 0)
            .unwrap();
        assert_eq!(total, 1);
        assert_eq!(items[0].id, "1");
    }
}
