use crate::auth::TokenStore;
use crate::config::Config;
use crate::models::{Meta, Page, Tweet, User};
use anyhow::{anyhow, bail, Result};
use serde::de::DeserializeOwned;
use serde_json::Value;
use std::sync::Arc;
use std::time::Duration;

/// Error marker for "no tokens / login required", so the web UI can return 401.
#[derive(Debug)]
pub struct NotAuthenticated;

impl std::fmt::Display for NotAuthenticated {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "not authenticated: connect your X account first")
    }
}
impl std::error::Error for NotAuthenticated {}

#[derive(Clone)]
pub struct XClient {
    http: reqwest::Client,
    api_base: String,
    token_url: String,
    client_id: Option<String>,
    client_secret: Option<String>,
    redirect_uri: String,
    post_fields: String,
    expansions: String,
    tokens: Arc<TokenStore>,
}

impl XClient {
    pub fn new(cfg: &Config, tokens: Arc<TokenStore>) -> Self {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .expect("failed to build HTTP client");
        Self {
            http,
            api_base: cfg.api_base_url.trim_end_matches('/').to_string(),
            token_url: cfg.token_url.clone(),
            client_id: cfg.client_id.clone(),
            client_secret: cfg.client_secret.clone(),
            redirect_uri: cfg.callback_url.clone(),
            post_fields: cfg.post_fields.clone(),
            expansions: cfg.expansions.clone(),
            tokens,
        }
    }

    pub fn tokens(&self) -> &Arc<TokenStore> {
        &self.tokens
    }

    pub fn http(&self) -> &reqwest::Client {
        &self.http
    }

    pub fn token_url(&self) -> &str {
        &self.token_url
    }

    pub fn client_id(&self) -> &Option<String> {
        &self.client_id
    }

    pub fn client_secret(&self) -> &Option<String> {
        &self.client_secret
    }

    pub fn redirect_uri(&self) -> &str {
        &self.redirect_uri
    }

    /// Current access token, refreshing first when close to expiry.
    pub async fn access_token(&self) -> Result<String> {
        let ts = self
            .tokens
            .get()
            .ok_or_else(|| anyhow::Error::new(NotAuthenticated))?;
        if ts.is_expired(60) {
            if let Some(refresh) = ts.refresh_token.clone() {
                match crate::auth::refresh_tokens(
                    &self.http,
                    &self.token_url,
                    &self.client_id,
                    &self.client_secret,
                    &refresh,
                )
                .await
                {
                    Ok(new) => {
                        self.tokens.set(new.clone())?;
                        return Ok(new.access_token);
                    }
                    Err(e) => {
                        tracing::warn!("token refresh failed, trying existing token: {e:#}");
                    }
                }
            }
        }
        Ok(ts.access_token)
    }

    /// GET /2/users/me
    pub async fn get_me(&self) -> Result<User> {
        let token = self.access_token().await?;
        let url = format!("{}/users/me", self.api_base);
        let resp = self
            .http
            .get(&url)
            .query(&[("user.fields", crate::config::DEFAULT_USER_FIELDS)])
            .bearer_auth(token)
            .send()
            .await?;
        let v = Self::parse(resp).await?;
        let data = v
            .get("data")
            .cloned()
            .ok_or_else(|| anyhow!("unexpected response from /users/me"))?;
        Ok(serde_json::from_value(data)?)
    }

    /// GET /2/users/{id}/bookmarks — one page.
    ///
    /// Handles 429 rate limits by sleeping until the window resets, and 401s
    /// by refreshing tokens once. `notify` receives human-readable progress
    /// messages (e.g. rate-limit waits) for surfacing in the UI/CLI.
    pub async fn get_bookmarks_page(
        &self,
        user_id: &str,
        max_results: u32,
        pagination_token: Option<String>,
        notify: &(dyn Fn(String) + Send + Sync),
    ) -> Result<Page> {
        let url = format!("{}/users/{}/bookmarks", self.api_base, user_id);
        let mut refreshed = false;
        let mut rate_limit_waits = 0u32;

        loop {
            let token = self.access_token().await?;
            let mut query: Vec<(&str, String)> = vec![
                ("max_results", max_results.to_string()),
                ("expansions", self.expansions.clone()),
                ("post.fields", self.post_fields.clone()),
                (
                    "user.fields",
                    crate::config::DEFAULT_USER_FIELDS.to_string(),
                ),
                (
                    "media.fields",
                    crate::config::DEFAULT_MEDIA_FIELDS.to_string(),
                ),
                (
                    "poll.fields",
                    crate::config::DEFAULT_POLL_FIELDS.to_string(),
                ),
            ];
            if let Some(t) = &pagination_token {
                query.push(("pagination_token", t.clone()));
            }

            let resp = self
                .http
                .get(&url)
                .query(&query)
                .bearer_auth(token)
                .send()
                .await?;

            let status = resp.status();

            if status.as_u16() == 429 {
                let reset = resp
                    .headers()
                    .get("x-rate-limit-reset")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|s| s.trim().parse::<i64>().ok())
                    .unwrap_or(0);
                let wait = (reset - chrono::Utc::now().timestamp() + 2).max(5) as u64;
                rate_limit_waits += 1;
                if rate_limit_waits > 12 {
                    bail!("rate limited too many times in a row; aborting sync");
                }
                notify(format!(
                    "rate limited; waiting {wait}s for the window to reset…"
                ));
                tracing::warn!("rate limited; sleeping {wait}s");
                tokio::time::sleep(Duration::from_secs(wait)).await;
                continue;
            }

            if status.as_u16() == 401 && !refreshed {
                refreshed = true;
                if let Some(ts) = self.tokens.get() {
                    if let Some(refresh) = ts.refresh_token {
                        match crate::auth::refresh_tokens(
                            &self.http,
                            &self.token_url,
                            &self.client_id,
                            &self.client_secret,
                            &refresh,
                        )
                        .await
                        {
                            Ok(new) => {
                                self.tokens.set(new)?;
                                continue;
                            }
                            Err(e) => {
                                tracing::warn!("refresh after 401 failed: {e:#}");
                            }
                        }
                    }
                }
            }

            let v = Self::parse(resp).await?;
            return parse_bookmarks_page(&v);
        }
    }

    async fn parse(resp: reqwest::Response) -> Result<Value> {
        let status = resp.status();
        let body = resp.text().await?;
        if !status.is_success() {
            bail!("X API error ({status}): {}", brief_error(&body));
        }
        Ok(serde_json::from_str(&body)?)
    }
}

fn brief_error(body: &str) -> String {
    match serde_json::from_str::<Value>(body) {
        Ok(v) => {
            let title = v.get("title").and_then(|x| x.as_str()).unwrap_or("");
            let detail = v.get("detail").and_then(|x| x.as_str()).unwrap_or("");
            let mut s = String::new();
            if !title.is_empty() {
                s.push_str(title);
            }
            if !detail.is_empty() {
                if !s.is_empty() {
                    s.push_str(": ");
                }
                s.push_str(detail);
            }
            if s.is_empty() {
                s.push_str(&body.chars().take(300).collect::<String>());
            }
            s
        }
        Err(_) => body.chars().take(300).collect(),
    }
}

fn parse_array<T: DeserializeOwned>(parent: &Value, key: &str) -> Vec<(T, Value)> {
    let Some(arr) = parent.get(key).and_then(|v| v.as_array()) else {
        return Vec::new();
    };
    arr.iter()
        .filter_map(|item| {
            let typed: T = serde_json::from_value(item.clone()).ok()?;
            Some((typed, item.clone()))
        })
        .collect()
}

/// Parse a bookmarks page JSON, accepting both new-style (`includes.posts`,
/// `referenced_posts`) and legacy (`includes.tweets`, `referenced_tweets`)
/// response shapes.
pub fn parse_bookmarks_page(v: &Value) -> Result<Page> {
    let mut page = Page {
        tweets: parse_array::<Tweet>(v, "data"),
        ..Default::default()
    };

    if let Some(includes) = v.get("includes") {
        page.users = parse_array::<User>(includes, "users");
        page.media = parse_array::<crate::models::Media>(includes, "media");
        // New-style responses put referenced posts under `posts`; legacy
        // responses used `tweets`. Collect whichever is present.
        page.ref_posts = parse_array::<Tweet>(includes, "posts");
        if page.ref_posts.is_empty() {
            page.ref_posts = parse_array::<Tweet>(includes, "tweets");
        }
        if let Some(polls) = includes.get("polls").and_then(|p| p.as_array()) {
            page.polls = polls.clone();
        }
    }

    if let Some(meta) = v.get("meta") {
        page.meta = serde_json::from_value::<Meta>(meta.clone()).unwrap_or_default();
    }
    if let Some(errors) = v.get("errors").and_then(|e| e.as_array()) {
        page.errors = errors.clone();
    }

    Ok(page)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_page_with_legacy_and_new_includes() {
        let legacy = json!({
            "data": [{ "id": "1", "text": "hello" }],
            "includes": { "tweets": [{ "id": "9", "text": "quoted" }], "users": [], "media": [] },
            "meta": { "result_count": 1, "next_token": "tok" }
        });
        let page = parse_bookmarks_page(&legacy).unwrap();
        assert_eq!(page.tweets.len(), 1);
        assert_eq!(page.ref_posts.len(), 1);
        assert_eq!(page.meta.next_token.as_deref(), Some("tok"));

        let modern = json!({
            "data": [{ "id": "2", "text": "hi", "note_post": { "text": "full text" } }],
            "includes": { "posts": [{ "id": "8", "text": "ref" }] },
            "meta": { "result_count": 1 }
        });
        let page = parse_bookmarks_page(&modern).unwrap();
        assert_eq!(page.ref_posts.len(), 1);
        assert_eq!(page.tweets[0].0.full_text(), "full text");
    }
}
