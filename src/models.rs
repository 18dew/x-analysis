use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A Post (tweet) as returned by the X API v2.
///
/// The API is mid-migration from "tweet" to "post" terminology, so responses
/// may use either `note_post`/`referenced_posts` or the legacy
/// `note_tweet`/`referenced_tweets` keys. We accept both.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tweet {
    pub id: String,
    pub text: String,
    #[serde(default)]
    pub author_id: Option<String>,
    #[serde(default)]
    pub created_at: Option<String>,
    #[serde(default)]
    pub lang: Option<String>,
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub conversation_id: Option<String>,
    #[serde(default)]
    pub in_reply_to_user_id: Option<String>,
    #[serde(default)]
    pub possibly_sensitive: Option<bool>,
    #[serde(default)]
    pub public_metrics: Option<PublicMetrics>,
    #[serde(default)]
    pub entities: Option<Value>,
    #[serde(default)]
    pub attachments: Option<Value>,
    #[serde(default)]
    pub referenced_posts: Option<Vec<ReferencedPost>>,
    #[serde(default)]
    pub referenced_tweets: Option<Vec<ReferencedPost>>,
    #[serde(default)]
    pub context_annotations: Option<Value>,
    #[serde(default)]
    pub withheld: Option<Value>,
    #[serde(default)]
    pub geo: Option<Value>,
    /// New-style long Post content.
    #[serde(default)]
    pub note_post: Option<NotePost>,
    /// Legacy long tweet content.
    #[serde(default)]
    pub note_tweet: Option<NoteTweet>,
}

impl Tweet {
    /// Full text, preferring long-form note content over the truncated text.
    pub fn full_text(&self) -> String {
        if let Some(note) = &self.note_post {
            if let Some(text) = &note.text {
                return text.clone();
            }
        }
        if let Some(note) = &self.note_tweet {
            if let Some(result) = note
                .note_tweet_results
                .as_ref()
                .and_then(|r| r.result.as_ref())
            {
                if let Some(text) = &result.text {
                    return text.clone();
                }
            }
        }
        self.text.clone()
    }

    /// Entities for the full text, preferring note entities when present.
    pub fn full_entities(&self) -> Option<Value> {
        if let Some(note) = &self.note_post {
            if note.entities.is_some() {
                return note.entities.clone();
            }
        }
        if let Some(note) = &self.note_tweet {
            if let Some(result) = note
                .note_tweet_results
                .as_ref()
                .and_then(|r| r.result.as_ref())
            {
                if result.entity_set.is_some() {
                    return result.entity_set.clone();
                }
                if result.entities.is_some() {
                    return result.entities.clone();
                }
            }
        }
        self.entities.clone()
    }

    /// Referenced posts, accepting both new and legacy keys.
    pub fn references(&self) -> Vec<ReferencedPost> {
        if let Some(refs) = &self.referenced_posts {
            return refs.clone();
        }
        if let Some(refs) = &self.referenced_tweets {
            return refs.clone();
        }
        Vec::new()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NotePost {
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub entities: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NoteTweet {
    #[serde(default)]
    pub note_tweet_results: Option<NoteTweetResults>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NoteTweetResults {
    #[serde(default)]
    pub result: Option<NoteTweetResult>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NoteTweetResult {
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub entity_set: Option<Value>,
    #[serde(default)]
    pub entities: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReferencedPost {
    #[serde(rename = "type")]
    pub kind: String,
    pub id: String,
}

/// Engagement metrics. `repost_count` is the new name for `retweet_count`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PublicMetrics {
    #[serde(default)]
    pub retweet_count: Option<u64>,
    #[serde(default)]
    pub repost_count: Option<u64>,
    #[serde(default)]
    pub reply_count: Option<u64>,
    #[serde(default)]
    pub like_count: Option<u64>,
    #[serde(default)]
    pub quote_count: Option<u64>,
    #[serde(default)]
    pub bookmark_count: Option<u64>,
    #[serde(default)]
    pub impression_count: Option<u64>,
}

impl PublicMetrics {
    pub fn reposts(&self) -> u64 {
        self.repost_count.or(self.retweet_count).unwrap_or(0)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct User {
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub profile_image_url: Option<String>,
    #[serde(default)]
    pub verified: Option<bool>,
    #[serde(default)]
    pub verified_type: Option<String>,
    #[serde(default)]
    pub protected: Option<bool>,
    #[serde(default)]
    pub location: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub created_at: Option<String>,
    #[serde(default)]
    pub public_metrics: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Media {
    pub media_key: String,
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub preview_image_url: Option<String>,
    #[serde(default)]
    pub alt_text: Option<String>,
    #[serde(default)]
    pub width: Option<u64>,
    #[serde(default)]
    pub height: Option<u64>,
    #[serde(default)]
    pub duration_ms: Option<u64>,
    #[serde(default)]
    pub variants: Option<Vec<Variant>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Variant {
    #[serde(default)]
    pub bit_rate: Option<u64>,
    #[serde(default)]
    pub content_type: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Meta {
    #[serde(default)]
    pub result_count: Option<u64>,
    #[serde(default)]
    pub next_token: Option<String>,
    #[serde(default)]
    pub previous_token: Option<String>,
}

/// One page of the bookmarks listing, with includes resolved alongside.
#[derive(Debug, Default)]
pub struct Page {
    pub tweets: Vec<(Tweet, Value)>,
    pub users: Vec<(User, Value)>,
    pub media: Vec<(Media, Value)>,
    /// Referenced posts returned under `includes.posts` (new) or
    /// `includes.tweets` (legacy).
    pub ref_posts: Vec<(Tweet, Value)>,
    pub polls: Vec<Value>,
    pub meta: Meta,
    pub errors: Vec<Value>,
}

/// OAuth token set persisted to disk.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenSet {
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: Option<String>,
    /// Unix seconds.
    pub expires_at: i64,
    #[serde(default)]
    pub scope: Option<String>,
    #[serde(default)]
    pub token_type: Option<String>,
    pub obtained_at: i64,
}

impl TokenSet {
    pub fn is_expired(&self, margin_secs: i64) -> bool {
        chrono::Utc::now().timestamp() >= self.expires_at - margin_secs
    }
}

/// Response body from the OAuth2 token endpoint.
#[derive(Debug, Clone, Deserialize)]
pub struct TokenResponse {
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: Option<String>,
    #[serde(default)]
    pub expires_in: Option<i64>,
    #[serde(default)]
    pub scope: Option<String>,
    #[serde(default)]
    pub token_type: Option<String>,
}

/// Query params sent to `/callback` by X after authorization.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct CallbackQuery {
    #[serde(default)]
    pub code: Option<String>,
    #[serde(default)]
    pub state: Option<String>,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub error_description: Option<String>,
}
