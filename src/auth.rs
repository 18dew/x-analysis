use crate::models::{CallbackQuery, TokenResponse, TokenSet};
use anyhow::{bail, Context, Result};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use rand::{distributions::Alphanumeric, thread_rng, Rng};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::RwLock;
use url::Url;

/// OAuth scopes needed to read bookmarks and refresh tokens.
pub const SCOPE: &str = "bookmark.read tweet.read users.read offline.access";

pub struct Pkce {
    pub verifier: String,
    pub state: String,
    pub challenge: String,
}

pub fn generate_pkce() -> Pkce {
    let verifier: String = thread_rng()
        .sample_iter(&Alphanumeric)
        .take(64)
        .map(char::from)
        .collect();
    let state: String = thread_rng()
        .sample_iter(&Alphanumeric)
        .take(24)
        .map(char::from)
        .collect();
    let challenge = code_challenge(&verifier);
    Pkce {
        verifier,
        state,
        challenge,
    }
}

/// S256 code challenge: BASE64URL(SHA256(verifier)) without padding.
pub fn code_challenge(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

pub fn authorize_url(
    authorize_base: &str,
    client_id: &str,
    redirect_uri: &str,
    scope: &str,
    state: &str,
    challenge: &str,
) -> Result<String> {
    // Validate the base and strip any existing query string.
    let base = Url::parse(authorize_base).context("invalid authorize_url in config")?;
    let base = match base.query() {
        Some(q) => authorize_base
            .trim_end_matches(q)
            .trim_end_matches('?')
            .to_string(),
        None => authorize_base.to_string(),
    };
    // X's documented authorize URLs percent-encode spaces as %20 (not `+`),
    // so encode strictly per RFC 3986.
    Ok(format!(
        "{base}\
?response_type=code\
&client_id={}\
&redirect_uri={}\
&scope={}\
&state={}\
&code_challenge={}\
&code_challenge_method=S256",
        percent_encode(client_id),
        percent_encode(redirect_uri),
        percent_encode(scope),
        percent_encode(state),
        percent_encode(challenge),
    ))
}

/// RFC 3986 percent-encoding (unreserved characters kept as-is).
fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Basic auth header for confidential clients.
fn basic_auth(client_id: &str, client_secret: &str) -> String {
    let raw = format!("{client_id}:{client_secret}");
    format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode(raw)
    )
}

/// POST to the OAuth2 token endpoint. Confidential clients (client secret
/// configured) authenticate with HTTP Basic auth; public clients send
/// `client_id` in the form body.
async fn token_request(
    http: &reqwest::Client,
    token_url: &str,
    client_id: &Option<String>,
    client_secret: &Option<String>,
    form: Vec<(&'static str, String)>,
) -> Result<TokenSet> {
    let mut form = form;
    let confidential = matches!(client_secret, Some(s) if !s.trim().is_empty());
    if !confidential {
        let Some(cid) = client_id else {
            bail!("client_id is not configured");
        };
        form.push(("client_id", cid.clone()));
    }

    let mut req = http.post(token_url).form(&form);
    if let (Some(cid), Some(secret)) = (client_id, client_secret) {
        if !secret.trim().is_empty() {
            req = req.header(reqwest::header::AUTHORIZATION, basic_auth(cid, secret));
        }
    }

    let resp = req.send().await.context("token endpoint request failed")?;
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        bail!("token endpoint returned {status}: {}", truncate(&body, 400));
    }

    let parsed: TokenResponse =
        serde_json::from_str(&body).context("could not parse token endpoint response")?;
    let now = chrono::Utc::now().timestamp();
    Ok(TokenSet {
        access_token: parsed.access_token,
        refresh_token: parsed.refresh_token,
        expires_at: now + parsed.expires_in.unwrap_or(7200),
        scope: parsed.scope,
        token_type: parsed.token_type,
        obtained_at: now,
    })
}

/// Exchange an authorization code for tokens. Note: X authorization codes
/// expire ~30 seconds after issuance, so this must run immediately when the
/// callback arrives.
pub async fn exchange_code(
    http: &reqwest::Client,
    token_url: &str,
    client_id: &Option<String>,
    client_secret: &Option<String>,
    redirect_uri: &str,
    code: &str,
    verifier: &str,
) -> Result<TokenSet> {
    token_request(
        http,
        token_url,
        client_id,
        client_secret,
        vec![
            ("grant_type", "authorization_code".to_string()),
            ("code", code.to_string()),
            ("redirect_uri", redirect_uri.to_string()),
            ("code_verifier", verifier.to_string()),
        ],
    )
    .await
}

/// Refresh tokens. X rotates refresh tokens, so the response's new refresh
/// token (if any) replaces the old one.
pub async fn refresh_tokens(
    http: &reqwest::Client,
    token_url: &str,
    client_id: &Option<String>,
    client_secret: &Option<String>,
    refresh_token: &str,
) -> Result<TokenSet> {
    let ts = token_request(
        http,
        token_url,
        client_id,
        client_secret,
        vec![
            ("grant_type", "refresh_token".to_string()),
            ("refresh_token", refresh_token.to_string()),
        ],
    )
    .await?;
    Ok(TokenSet {
        refresh_token: ts.refresh_token.or(Some(refresh_token.to_string())),
        ..ts
    })
}

fn truncate(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

#[cfg(unix)]
fn set_private(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
}
#[cfg(not(unix))]
fn set_private(_path: &Path) {}

/// In-memory + on-disk token storage. The file is written with 0600 perms.
pub struct TokenStore {
    path: PathBuf,
    inner: RwLock<Option<TokenSet>>,
}

impl TokenStore {
    pub fn load(path: PathBuf) -> Self {
        let inner = fs::read_to_string(&path)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok());
        Self {
            path,
            inner: RwLock::new(inner),
        }
    }

    pub fn get(&self) -> Option<TokenSet> {
        self.inner.read().unwrap().clone()
    }

    pub fn set(&self, ts: TokenSet) -> Result<()> {
        let json = serde_json::to_string_pretty(&ts)?;
        fs::write(&self.path, json)?;
        set_private(&self.path);
        *self.inner.write().unwrap() = Some(ts);
        Ok(())
    }

    pub fn clear(&self) -> Result<()> {
        let _ = fs::remove_file(&self.path);
        *self.inner.write().unwrap() = None;
        Ok(())
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// Percent-encode a string for use in a query parameter.
pub fn urlencode(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes()).collect()
}

/// Validate a callback against a pending PKCE state.
pub fn check_callback(q: &CallbackQuery, expected_state: &str) -> Result<String> {
    if let Some(err) = &q.error {
        bail!(
            "authorization failed: {err}{}",
            q.error_description
                .as_ref()
                .map(|d| format!(": {d}"))
                .unwrap_or_default()
        );
    }
    let code = q
        .code
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("callback did not include an authorization code"))?;
    if q.state.as_deref() != Some(expected_state) {
        bail!("state mismatch in OAuth callback — possible CSRF or stale login attempt");
    }
    Ok(code.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Test vector from RFC 7636 appendix.
    #[test]
    fn pkce_challenge_rfc7636() {
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        assert_eq!(
            code_challenge(verifier),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn authorize_url_contains_params() {
        let url = authorize_url(
            "https://x.com/i/oauth2/authorize",
            "cid",
            "http://localhost:8080/callback",
            SCOPE,
            "st4te",
            "ch4ll",
        )
        .unwrap();
        assert!(url.starts_with("https://x.com/i/oauth2/authorize?"));
        assert!(url.contains("code_challenge_method=S256"));
        assert!(url.contains("code_challenge=ch4ll"));
        assert!(url.contains("state=st4te"));
        assert!(url.contains("client_id=cid"));
        assert!(url.contains("scope=bookmark.read%20tweet.read%20users.read%20offline.access"));
    }
}
