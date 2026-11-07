use axum::extract::Query;
use axum::routing::get;
use axum::{Json, Router};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;
use xanalysis::api::XClient;
use xanalysis::auth::TokenStore;
use xanalysis::config::Config;
use xanalysis::models::TokenSet;
use xanalysis::store::Store;
use xanalysis::sync::{run_sync, SyncHandle};

fn test_config(home: &std::path::Path) -> Config {
    Config {
        home: home.to_path_buf(),
        client_id: Some("test-client".into()),
        client_secret: None,
        port: 0,
        callback_url: "http://localhost:0/callback".into(),
        max_results: 2,
        max_pages: 100,
        max_search_posts: 300,
        search_page_size: 100,
        api_base_url: String::new(),
        authorize_url: String::new(),
        token_url: String::new(),
        post_fields: xanalysis::config::DEFAULT_POST_FIELDS.to_string(),
        expansions: xanalysis::config::DEFAULT_EXPANSIONS.to_string(),
    }
}

fn tweet(id: &str, text: &str, author_id: &str) -> Value {
    json!({
        "id": id,
        "text": text,
        "author_id": author_id,
        "created_at": "2026-01-01T00:00:00.000Z",
        "lang": "en",
        "public_metrics": {
            "reply_count": 1,
            "repost_count": 2,
            "like_count": 3,
            "quote_count": 4,
            "bookmark_count": 5,
            "impression_count": 100
        }
    })
}

/// Mock of the X bookmarks API + live search API: two bookmark pages of two
/// posts with expansions (author users, quoted post, media) in `includes`,
/// plus a /2/tweets/search/recent route that simulates a low-tier API
/// (rejects max_results > 10) and validates search operator syntax.
fn mock_router() -> Router {
    Router::new()
        .route(
            "/2/users/me",
            get(|| async {
                Json(json!({
                    "data": {"id": "42", "name": "Test User", "username": "testuser"}
                }))
            }),
        )
        .route(
            "/2/tweets/search/recent",
            get(|Query(params): Query<HashMap<String, String>>| async move {
                let q = params.get("query").cloned().unwrap_or_default();
                let valid_query = q.contains("url:\"arxiv.org\"") || q.contains("\"rust\"");
                if !valid_query || !q.contains("-is:retweet") {
                    return (
                        axum::http::StatusCode::BAD_REQUEST,
                        Json(json!({"detail": format!("bad query operator: {q}")})),
                    );
                }
                let max_results: u32 = params
                    .get("max_results")
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0);
                if max_results > 10 {
                    return (
                        axum::http::StatusCode::BAD_REQUEST,
                        Json(json!({"errors": [{"message":
                            "max_results must be between 10 and 10"}]})),
                    );
                }
                if matches!(
                    params.get("pagination_token").map(String::as_str),
                    Some("tok2")
                ) {
                    (
                        axum::http::StatusCode::OK,
                        Json(json!({
                            "data": [tweet("200", "third live post about rust from arxiv.org", "7")],
                            "includes": {},
                            "meta": {"result_count": 1}
                        })),
                    )
                } else {
                    (
                        axum::http::StatusCode::OK,
                        Json(json!({
                            "data": [
                                {
                                    "id": "300",
                                    "text": "live post one",
                                    "author_id": "7",
                                    "entities": {"urls": [
                                        {"expanded_url": "https://arxiv.org/abs/1",
                                         "title": "Live Paper"}
                                    ]}
                                },
                                tweet("301", "live post two about rust and transformers via arxiv.org", "7")
                            ],
                            "includes": {"users": [
                                {"id": "7", "username": "author", "name": "The Author"}
                            ]},
                            "meta": {"result_count": 2, "next_token": "tok2"}
                        })),
                    )
                }
            }),
        )
        .route(
            "/2/users/42/bookmarks",
            get(|Query(params): Query<HashMap<String, String>>| async move {
                let page2 = params
                    .get("pagination_token")
                    .map(|t| t == "tok2")
                    .unwrap_or(false);
                if !page2 {
                    Json(json!({
                        "data": [
                            tweet("100", "first post about rust", "7"),
                            {
                                "id": "101",
                                "text": "look at this",
                                "author_id": "7",
                                "referenced_posts": [{"type": "quoted", "id": "55"}],
                                "attachments": {"media_keys": ["mk1"]},
                                "entities": {
                                    "urls": [{
                                        "expanded_url": "https://arxiv.org/abs/1706.03762",
                                        "title": "Attention Is All You Need"
                                    }]
                                },
                                "note_post": {"text": "a long post that goes past 280 chars…"}
                            }
                        ],
                        "includes": {
                            "users": [{"id": "7", "username": "author", "name": "The Author"}],
                            "posts": [tweet("55", "quoted content", "9")],
                            "media": [{"media_key": "mk1", "type": "photo", "url": "https://img"}]
                        },
                        "meta": {"result_count": 2, "next_token": "tok2"}
                    }))
                } else {
                    Json(json!({
                        "data": [tweet("102", "third post", "7")],
                        "includes": {},
                        "meta": {"result_count": 1}
                    }))
                }
            }),
        )
}

#[tokio::test]
async fn sync_paginates_stores_and_exports() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, mock_router()).await.unwrap();
    });

    let tmp = tempfile::tempdir().unwrap();
    let mut cfg = test_config(tmp.path());
    cfg.api_base_url = format!("http://{addr}/2");
    cfg.token_url = format!("http://{addr}/2/oauth2/token");
    cfg.max_results = 2; // force the two pages

    let tokens = Arc::new(TokenStore::load(tmp.path().join("tokens.json")));
    tokens
        .set(TokenSet {
            access_token: "fake-token".into(),
            refresh_token: None,
            expires_at: chrono::Utc::now().timestamp() + 3600,
            scope: None,
            token_type: None,
            obtained_at: chrono::Utc::now().timestamp(),
        })
        .unwrap();

    let client = XClient::new(&cfg, tokens);
    let store = Store::open(&tmp.path().join("data/bookmarks.db")).unwrap();
    let handle = SyncHandle::new();

    let report = run_sync(&client, &store, &cfg, &handle, false, |_| {})
        .await
        .unwrap();

    // First sync is a full sync (no last_full_sync_at yet).
    assert_eq!(report.mode, "full");
    assert_eq!(report.pages, 2);
    assert_eq!(report.seen, 3);
    assert_eq!(report.new, 3);
    assert_eq!(report.removed, 0);
    assert_eq!(store.count_bookmarks().unwrap(), 3);

    // Bookmarks keep API order (position 0 = newest).
    let (items, total) = store.list_bookmarks(None, None, 10, 0).unwrap();
    assert_eq!(total, 3);
    assert_eq!(items[0].id, "100");
    assert_eq!(items[0].metrics.likes, 3);
    assert_eq!(items[0].metrics.reposts, 2);
    assert_eq!(items[0].metrics.views, Some(100));

    // Quoted post + media resolved from includes.
    assert_eq!(items[1].quoted.as_ref().unwrap().text, "quoted content");
    assert_eq!(items[1].media.len(), 1);
    assert_eq!(items[1].media[0].kind, "photo");
    // note_post full text wins over truncated text.
    assert!(items[1].text.starts_with("a long post"));

    // Author joined in.
    assert_eq!(
        items[0].author.as_ref().unwrap().username.as_deref(),
        Some("author")
    );

    // Exports were written by the sync.
    let jsonl = cfg.jsonl_path();
    assert!(jsonl.exists());
    let content = std::fs::read_to_string(&jsonl).unwrap();
    assert_eq!(content.lines().count(), 3);
    let first: Value = serde_json::from_str(content.lines().next().unwrap()).unwrap();
    assert_eq!(first["author"]["username"], "author");
    assert_eq!(first["url"], "https://x.com/author/status/100");
    assert!(cfg.md_path().exists());

    // A second, incremental sync stops early once everything is known.
    let handle2 = SyncHandle::new();
    let report2 = run_sync(&client, &store, &cfg, &handle2, false, |_| {})
        .await
        .unwrap();
    assert_eq!(report2.mode, "incremental");
    assert_eq!(report2.new, 0);
    assert_eq!(report2.removed, 0);
    assert_eq!(store.count_bookmarks().unwrap(), 3);

    // A forced full sync with a vanished bookmark marks it removed.
    let handle3 = SyncHandle::new();
    let report3 = run_sync(&client, &store, &cfg, &handle3, true, |_| {})
        .await
        .unwrap();
    assert_eq!(report3.mode, "full");
    assert_eq!(store.count_bookmarks().unwrap(), 3);

    // ---- Topic finder ----
    // Domain mode: one bookmark links arxiv.org.
    let result = xanalysis::topics::analyze(&store, "https://arxiv.org/").unwrap();
    assert_eq!(result.report.input, "https://arxiv.org/");
    assert_eq!(result.report.needle, "arxiv.org");
    assert!(result.report.is_domain);
    assert_eq!(result.report.total_matches, 1);
    assert!(result
        .report
        .links
        .iter()
        .any(|l| l.label == "Attention Is All You Need"
            && l.url.as_deref() == Some("https://arxiv.org/abs/1706.03762")));
    // Exports contain the report and the matched bookmarks.
    let json = xanalysis::topics::export_json(&result);
    assert_eq!(json["total_matches"], 1);
    assert_eq!(json["bookmarks"][0]["author_username"], "author");
    let csv = xanalysis::topics::export_csv(&result.report);
    assert!(csv.starts_with("kind,label,count,url\n"));

    // Word mode: matches text, not substrings.
    let word = xanalysis::topics::analyze(&store, "first").unwrap();
    assert!(!word.report.is_domain);
    assert_eq!(word.report.total_matches, 1);
    let none = xanalysis::topics::analyze(&store, "firs").unwrap();
    assert_eq!(none.report.total_matches, 0);

    // Feed-level domain filter.
    let (feed, feed_total) = store
        .list_bookmarks(None, Some("arxiv.org"), 10, 0)
        .unwrap();
    assert_eq!(feed_total, 1);
    assert_eq!(feed[0].id, "101");
}

/// The scope=x topic finder: live search across all of X, including the
/// automatic page-size fallback for tiers that cap max_results below 100,
/// pagination, the max-posts cap, and precise filtering.
#[tokio::test]
async fn topics_scope_x_searches_all_of_x() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, mock_router()).await.unwrap();
    });

    let tmp = tempfile::tempdir().unwrap();
    let mut cfg = test_config(tmp.path());
    cfg.api_base_url = format!("http://{addr}/2");

    let tokens = Arc::new(TokenStore::load(tmp.path().join("tokens.json")));
    tokens
        .set(TokenSet {
            access_token: "fake-token".into(),
            refresh_token: None,
            expires_at: chrono::Utc::now().timestamp() + 3600,
            scope: None,
            token_type: None,
            obtained_at: chrono::Utc::now().timestamp(),
        })
        .unwrap();
    let client = XClient::new(&cfg, tokens);

    // Domain mode: query must be url:"arxiv.org" -is:retweet (enforced by the
    // mock); page size 100 is rejected → falls back to 10; pages until done.
    let result = xanalysis::topics::analyze_x(&client, "arxiv.org", 5, 100, &|_| {})
        .await
        .unwrap();
    assert_eq!(result.report.scope, "x");
    assert!(result.report.is_domain);
    assert_eq!(result.report.total_matches, 3); // 2 posts + 1 from page 2
    assert!(result.report.links.iter().any(|l| l.label == "Live Paper"));
    assert_eq!(result.report.authors[0].label, "@author");
    // Nothing was written to the real archive.
    assert!(!cfg.db_path().exists());

    // Word mode with the max-posts cap: stops after the first page, then the
    // precise word filter keeps only the posts actually containing "rust".
    let capped = xanalysis::topics::analyze_x(&client, "rust", 2, 100, &|_| {})
        .await
        .unwrap();
    assert!(!capped.report.is_domain);
    assert_eq!(capped.report.total_matches, 1); // only "301" contains rust
    assert_eq!(capped.report.needle, "rust");

    // Exports work for live results too.
    let md = xanalysis::topics::export_md(&result);
    assert!(md.contains("Source: live X search"));

    // Unauthenticated → NotAuthenticated.
    let no_tokens = Arc::new(TokenStore::load(tmp.path().join("none.json")));
    let anon = XClient::new(&cfg, no_tokens);
    let err = match xanalysis::topics::analyze_x(&anon, "arxiv.org", 5, 100, &|_| {}).await {
        Err(e) => e,
        Ok(_) => panic!("expected an error for unauthenticated live search"),
    };
    assert!(format!("{err:#}").contains("not authenticated"));
}

#[tokio::test]
async fn unauthenticated_sync_fails_cleanly() {
    let tmp = tempfile::tempdir().unwrap();
    let mut cfg = test_config(tmp.path());
    cfg.api_base_url = "http://127.0.0.1:9/2".into(); // nothing listens here
    let tokens = Arc::new(TokenStore::load(tmp.path().join("tokens.json")));
    let client = XClient::new(&cfg, tokens);
    let store = Store::open(&tmp.path().join("data/bookmarks.db")).unwrap();
    let handle = SyncHandle::new();

    let err = run_sync(&client, &store, &cfg, &handle, false, |_| {}).await;
    assert!(err.is_err());
    let status = handle.snapshot();
    assert_eq!(status.state, "error");
    assert!(status.message.contains("not authenticated"));
}
