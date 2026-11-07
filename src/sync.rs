use crate::api::XClient;
use crate::config::Config;
use crate::models::{Page, User};
use crate::store::Store;
use anyhow::Result;
use serde::Serialize;
use std::collections::HashSet;
use std::sync::Mutex;

#[derive(Clone, Debug, Serialize)]
pub struct SyncStatus {
    pub state: String,
    pub message: String,
    pub mode: String,
    pub pages_fetched: u64,
    pub tweets_seen: u64,
    pub new_bookmarks: u64,
    pub removed: u64,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
}

impl Default for SyncStatus {
    fn default() -> Self {
        Self {
            state: "idle".into(),
            message: String::new(),
            mode: String::new(),
            pages_fetched: 0,
            tweets_seen: 0,
            new_bookmarks: 0,
            removed: 0,
            started_at: None,
            finished_at: None,
        }
    }
}

/// Shared, pollable progress handle. The CLI reads it for the final summary;
/// the web UI polls it from `/api/status`.
pub struct SyncHandle {
    inner: Mutex<SyncStatus>,
}

impl Default for SyncHandle {
    fn default() -> Self {
        Self {
            inner: Mutex::new(SyncStatus::default()),
        }
    }
}

impl SyncHandle {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn snapshot(&self) -> SyncStatus {
        self.inner.lock().unwrap().clone()
    }

    pub fn is_running(&self) -> bool {
        self.inner.lock().unwrap().state == "running"
    }

    fn start(&self, mode: &str) {
        let mut s = self.inner.lock().unwrap();
        *s = SyncStatus {
            state: "running".into(),
            mode: mode.into(),
            message: format!("{mode} sync starting…"),
            started_at: Some(crate::store::now_rfc3339()),
            ..Default::default()
        };
    }

    fn set_message(&self, msg: impl Into<String>) {
        self.inner.lock().unwrap().message = msg.into();
    }

    fn page_done(&self, pages: u64, seen: u64, new: u64) {
        let mut s = self.inner.lock().unwrap();
        s.pages_fetched = pages;
        s.tweets_seen = seen;
        s.new_bookmarks = new;
    }

    fn finish(&self, removed: u64, msg: String) {
        let mut s = self.inner.lock().unwrap();
        s.state = "done".into();
        s.message = msg;
        s.removed = removed;
        s.finished_at = Some(crate::store::now_rfc3339());
    }

    fn fail(&self, msg: String) {
        let mut s = self.inner.lock().unwrap();
        s.state = "error".into();
        s.message = msg;
        s.finished_at = Some(crate::store::now_rfc3339());
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct SyncReport {
    pub mode: String,
    pub pages: u64,
    pub seen: usize,
    pub new: u64,
    pub removed: usize,
    pub exported: usize,
}

/// Sync bookmarks from X into the local store, then regenerate exports.
///
/// - The first sync (or `full: true`) pages through *every* bookmark and
///   reconciles removed ones. X allows ~15 requests / 15 min for this
///   endpoint, so large libraries take a while; rate limits are awaited.
/// - Later syncs stop early once a few consecutive pages contain only
///   already-known bookmarks (adding new bookmarks shifts positions, but the
///   relative order of older items never changes).
pub async fn run_sync(
    client: &XClient,
    store: &Store,
    cfg: &Config,
    handle: &SyncHandle,
    full: bool,
    on_me: impl FnOnce(User) + Send,
) -> Result<SyncReport> {
    let mode = if full || store.get_meta("last_full_sync_at")?.is_none() {
        "full"
    } else {
        "incremental"
    };
    handle.start(mode);
    match run_sync_inner(client, store, cfg, handle, mode, on_me).await {
        Ok(report) => {
            handle.finish(
                report.removed as u64,
                format!(
                    "{mode} sync complete: {} bookmarks seen, {} new, {} removed",
                    report.seen, report.new, report.removed
                ),
            );
            Ok(report)
        }
        Err(e) => {
            handle.fail(format!("{e:#}"));
            Err(e)
        }
    }
}

/// What `ingest_page` stored from one API page.
#[derive(Debug, Default)]
pub struct Ingested {
    pub len: usize,
    /// Rows that already existed in the bookmarks table.
    pub known: usize,
    pub ids: Vec<String>,
}

/// Persist one API page (posts, includes, bookmark positions) into a store.
/// Used by both the bookmark sync and the live-search topic finder (which
/// points it at a temporary in-memory store).
pub fn ingest_page(store: &Store, page: &Page, position: &mut i64, now: &str) -> Result<Ingested> {
    // Includes first, so tweet rows can resolve their author.
    for (user, raw) in &page.users {
        store.upsert_user(user, raw)?;
    }
    for (post, raw) in &page.ref_posts {
        store.upsert_tweet(post, raw)?;
    }

    let mut known = 0usize;
    let mut ids = Vec::with_capacity(page.tweets.len());
    for (tweet, raw) in &page.tweets {
        store.upsert_tweet(tweet, raw)?;

        if let Some(attachments) = &tweet.attachments {
            if let Some(keys) = attachments.get("media_keys").and_then(|k| k.as_array()) {
                for key in keys.iter().filter_map(|k| k.as_str()) {
                    if let Some((media, mraw)) = page.media.iter().find(|(m, _)| m.media_key == key)
                    {
                        store.upsert_media(media, &tweet.id, mraw)?;
                    }
                }
            }
            if let Some(poll_ids) = attachments.get("poll_ids").and_then(|p| p.as_array()) {
                for id in poll_ids.iter().filter_map(|i| i.as_str()) {
                    if let Some(poll) = page
                        .polls
                        .iter()
                        .find(|p| p.get("id").and_then(|i| i.as_str()) == Some(id))
                    {
                        store.upsert_poll(poll, &tweet.id)?;
                    }
                }
            }
        }

        let existed = store.upsert_bookmark(&tweet.id, *position, now)?;
        if existed {
            known += 1;
        }
        ids.push(tweet.id.clone());
        *position += 1;
    }
    Ok(Ingested {
        len: page.tweets.len(),
        known,
        ids,
    })
}

async fn run_sync_inner(
    client: &XClient,
    store: &Store,
    cfg: &Config,
    handle: &SyncHandle,
    mode: &str,
    on_me: impl FnOnce(User) + Send,
) -> Result<SyncReport> {
    let me = client.get_me().await?;
    on_me(me.clone());

    let notify = |m: String| handle.set_message(m);
    let mut pagination: Option<String> = None;
    let mut seen: HashSet<String> = HashSet::new();
    let mut new_count: u64 = 0;
    let mut position: i64 = 0;
    let mut pages: u64 = 0;
    let mut consecutive_known_pages: u32 = 0;
    let mut completed_naturally = false;

    loop {
        handle.set_message(format!("fetching page {}…", pages + 1));
        let page = client
            .get_bookmarks_page(&me.id, cfg.max_results, pagination.clone(), &notify)
            .await?;
        pages += 1;

        let now = crate::store::now_rfc3339();
        let ingested = ingest_page(store, &page, &mut position, &now)?;
        let page_len = ingested.len;
        let known_in_page = ingested.known;
        seen.extend(ingested.ids.iter().cloned());
        new_count += page_len.saturating_sub(known_in_page) as u64;

        for err in &page.errors {
            tracing::warn!(
                "skipped unavailable post: {}",
                err.get("detail").and_then(|d| d.as_str()).unwrap_or("?")
            );
        }

        handle.page_done(pages, seen.len() as u64, new_count);
        tracing::info!(
            "page {pages}: {page_len} posts ({} total, {new_count} new)",
            seen.len()
        );

        // Incremental mode: stop after 3 consecutive fully-known pages.
        if mode == "incremental" && page_len > 0 && known_in_page == page_len {
            consecutive_known_pages += 1;
            if consecutive_known_pages >= 3 {
                tracing::info!("incremental sync caught up; stopping early");
                break;
            }
        } else {
            consecutive_known_pages = 0;
        }

        pagination = page.meta.next_token.clone();
        if pagination.is_none() {
            completed_naturally = true;
            break;
        }
        if pages >= cfg.max_pages as u64 {
            tracing::warn!("reached max_pages cap ({}) — stopping", cfg.max_pages);
            break;
        }
    }

    let now = crate::store::now_rfc3339();
    let mut removed = 0usize;
    if mode == "full" && completed_naturally {
        removed = store.mark_removed(&seen, &now)?;
        store.set_meta("last_full_sync_at", &now)?;
    }
    store.set_meta("last_sync_at", &now)?;

    let exported = store.write_exports(&cfg.jsonl_path(), &cfg.md_path())?;

    Ok(SyncReport {
        mode: mode.to_string(),
        pages,
        seen: seen.len(),
        new: new_count,
        removed,
        exported,
    })
}
