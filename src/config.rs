use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

pub const DEFAULT_PORT: u16 = 8080;

/// Values that may live in `~/.xanalysis/config.toml`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FileConfig {
    pub client_id: Option<String>,
    pub client_secret: Option<String>,
    pub port: Option<u16>,
    pub callback_url: Option<String>,
    pub max_results: Option<u32>,
    pub max_pages: Option<u32>,
    pub max_search_posts: Option<u64>,
    pub search_page_size: Option<u32>,
    pub api_base_url: Option<String>,
    pub authorize_url: Option<String>,
    pub token_url: Option<String>,
    /// Escape hatches in case the X API field set changes.
    pub post_fields: Option<String>,
    pub expansions: Option<String>,
}

/// Fully resolved configuration. Precedence: env > config file > defaults.
#[derive(Debug, Clone)]
pub struct Config {
    pub home: PathBuf,
    pub client_id: Option<String>,
    pub client_secret: Option<String>,
    pub port: u16,
    pub callback_url: String,
    pub max_results: u32,
    pub max_pages: u32,
    /// Topic-finder live search: how many posts to fetch from X at most.
    pub max_search_posts: u64,
    /// Topic-finder live search: page size requested per search call
    /// (X allows 10-100; some tiers cap it lower — fallback is automatic).
    pub search_page_size: u32,
    pub api_base_url: String,
    pub authorize_url: String,
    pub token_url: String,
    pub post_fields: String,
    pub expansions: String,
}

pub const DEFAULT_POST_FIELDS: &str = "attachments,author_id,context_annotations,conversation_id,created_at,entities,geo,id,in_reply_to_user_id,lang,note_post,possibly_sensitive,public_metrics,referenced_posts,source,text,withheld";
pub const DEFAULT_EXPANSIONS: &str =
    "attachments.media_keys,attachments.poll_ids,author_id,entities.mentions.username,in_reply_to_user_id,referenced_posts";
pub const DEFAULT_USER_FIELDS: &str = "created_at,description,id,location,name,profile_image_url,protected,public_metrics,url,username,verified,verified_type";
pub const DEFAULT_MEDIA_FIELDS: &str =
    "alt_text,duration_ms,height,media_key,preview_image_url,public_metrics,type,url,variants,width";
pub const DEFAULT_POLL_FIELDS: &str = "duration_minutes,end_datetime,id,options,voting_status";

fn non_empty(v: Option<String>) -> Option<String> {
    v.filter(|s| !s.trim().is_empty())
}

fn env_non_empty(key: &str) -> Option<String> {
    non_empty(std::env::var(key).ok())
}

fn env_parse<T: std::str::FromStr>(key: &str) -> Option<T> {
    std::env::var(key)
        .ok()
        .and_then(|v| v.trim().parse::<T>().ok())
}

impl Config {
    pub fn home_dir() -> Result<PathBuf> {
        if let Some(dir) = env_non_empty("XANALYSIS_HOME") {
            return Ok(PathBuf::from(dir));
        }
        let home = dirs::home_dir().context("could not determine your home directory")?;
        Ok(home.join(".xanalysis"))
    }

    pub fn load() -> Result<Config> {
        let home = Self::home_dir()?;
        let config_path = home.join("config.toml");
        let file: FileConfig = match fs::read_to_string(&config_path) {
            Ok(text) => toml::from_str(&text)
                .with_context(|| format!("failed to parse {}", config_path.display()))?,
            Err(_) => FileConfig::default(),
        };

        let port = env_parse::<u16>("XANALYSIS_PORT")
            .or(file.port)
            .unwrap_or(DEFAULT_PORT);
        let callback_url = env_non_empty("XANALYSIS_CALLBACK_URL")
            .or(non_empty(file.callback_url))
            .unwrap_or_else(|| format!("http://localhost:{port}/callback"));
        let client_id = env_non_empty("X_CLIENT_ID").or(non_empty(file.client_id));
        let client_secret = env_non_empty("X_CLIENT_SECRET").or(non_empty(file.client_secret));

        Ok(Config {
            home,
            client_id,
            client_secret,
            port,
            callback_url,
            max_results: file.max_results.unwrap_or(100).clamp(1, 100),
            max_pages: file.max_pages.unwrap_or(1000),
            max_search_posts: file.max_search_posts.unwrap_or(300).min(1000),
            search_page_size: file.search_page_size.unwrap_or(100).clamp(10, 100),
            api_base_url: non_empty(file.api_base_url)
                .unwrap_or_else(|| "https://api.x.com/2".to_string()),
            authorize_url: non_empty(file.authorize_url)
                .unwrap_or_else(|| "https://x.com/i/oauth2/authorize".to_string()),
            token_url: non_empty(file.token_url)
                .unwrap_or_else(|| "https://api.x.com/2/oauth2/token".to_string()),
            post_fields: non_empty(file.post_fields)
                .unwrap_or_else(|| DEFAULT_POST_FIELDS.to_string()),
            expansions: non_empty(file.expansions)
                .unwrap_or_else(|| DEFAULT_EXPANSIONS.to_string()),
        })
    }

    pub fn config_path(&self) -> PathBuf {
        self.home.join("config.toml")
    }
    pub fn tokens_path(&self) -> PathBuf {
        self.home.join("tokens.json")
    }
    pub fn data_dir(&self) -> PathBuf {
        self.home.join("data")
    }
    pub fn db_path(&self) -> PathBuf {
        self.data_dir().join("bookmarks.db")
    }
    pub fn jsonl_path(&self) -> PathBuf {
        self.data_dir().join("bookmarks.jsonl")
    }
    pub fn md_path(&self) -> PathBuf {
        self.data_dir().join("bookmarks.md")
    }
    pub fn listen_addr(&self) -> String {
        format!("127.0.0.1:{}", self.port)
    }

    pub fn write_default_config(&self) -> Result<bool> {
        let path = self.config_path();
        if path.exists() {
            return Ok(false);
        }
        fs::create_dir_all(&self.home)?;
        fs::create_dir_all(self.data_dir())?;
        fs::write(&path, CONFIG_TEMPLATE.trim_start())?;
        Ok(true)
    }
}

const CONFIG_TEMPLATE: &str = r#"
# Xanalysis configuration.
#
# 1. Create a developer app at https://developer.x.com (see README.md for the
#    exact settings to pick).
# 2. Copy the OAuth 2.0 "Client ID" (and "Client Secret" for confidential
#    clients) from your app's "Keys and tokens" page into the values below,
#    or export X_CLIENT_ID / X_CLIENT_SECRET instead.
# 3. The callback URL registered in your X app settings must match
#    `callback_url` below exactly.

client_id = ""
client_secret = ""

# Local port for the web UI and the OAuth callback listener.
# port = 8080

# Must exactly match a Callback URI registered in your X app settings.
# callback_url = "http://localhost:8080/callback"

# Page size when fetching bookmarks (X allows 1-100).
# max_results = 100

# Safety cap on pages fetched per sync (each page is up to 100 bookmarks).
# max_pages = 1000

# Topic finder — live search across all of X (scope=x):
# how many posts to fetch at most, and the page size per request.
# Live search uses the pay-per-use X API and only covers recent posts
# (roughly the last 7 days).
# max_search_posts = 300
# search_page_size = 100

# API endpoints. Override only if X changes them.
# api_base_url = "https://api.x.com/2"
# authorize_url = "https://x.com/i/oauth2/authorize"
# token_url = "https://api.x.com/2/oauth2/token"

# Field sets requested from the API. Override if X renames fields.
# post_fields = "..."
# expansions = "..."
"#;
