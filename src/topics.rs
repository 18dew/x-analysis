//! Topic finder: given a domain (e.g. `arxiv.org`) or a topic word (e.g.
//! `rust`), find matching bookmarks and extract the topics inside them.
//!
//! Inputs containing a dot and no whitespace are treated as domains (matched
//! against expanded URL hosts and plain-text mentions); everything else is
//! treated as a topic word (word-boundary matching against text, hashtags,
//! mentions, and authors).

use crate::api::XClient;
use crate::store::{now_rfc3339, BookmarkView, Store};
use anyhow::Result;
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use url::Url;

/// Upper bound on candidate rows fetched for a single analysis.
pub const MAX_CANDIDATES: i64 = 2000;

const KEYWORD_LIMIT: usize = 40;
const HASHTAG_LIMIT: usize = 40;
const LINK_LIMIT: usize = 25;
const AUTHOR_LIMIT: usize = 25;

#[derive(Debug, Clone, Serialize)]
pub struct TopicItem {
    pub label: String,
    pub count: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TopicsReport {
    pub input: String,
    pub needle: String,
    pub is_domain: bool,
    /// Where the analyzed posts came from: "local" (bookmark archive) or
    /// "x" (live search across all of X).
    pub scope: String,
    pub generated_at: String,
    pub total_matches: u64,
    pub keywords: Vec<TopicItem>,
    pub hashtags: Vec<TopicItem>,
    pub links: Vec<TopicItem>,
    pub authors: Vec<TopicItem>,
}

/// Result of an analysis: the report plus the matched bookmark views
/// (needed by the exporters).
pub struct TopicsResult {
    pub report: TopicsReport,
    pub matched: Vec<BookmarkView>,
}

// ---------- input normalization ----------

/// Normalize user input: lowercase, strip scheme / path / leading `www.` /
/// trailing dot. `"https://www.Arxiv.org/abs/1"` → `"arxiv.org"`.
pub fn normalize(input: &str) -> String {
    let mut s = input.trim().to_lowercase();
    s = s
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .to_string();
    if let Some(pos) = s.find(['/', '?', '#']) {
        s.truncate(pos);
    }
    let s = s.trim_start_matches("www.");
    s.trim_end_matches('.').to_string()
}

/// Domain-like inputs are matched against URL hosts; others as topic words.
pub fn is_domain_like(s: &str) -> bool {
    s.contains('.') && !s.chars().any(|c| c.is_whitespace())
}

// ---------- matching ----------

/// The views whose text/entities/author should be considered for a bookmark:
/// a repost's real content (via `retweeted`) plus any quoted post.
fn sources(v: &BookmarkView) -> Vec<&BookmarkView> {
    let display = v.retweeted.as_deref().unwrap_or(v);
    let mut out = vec![display];
    if let Some(q) = &display.quoted {
        out.push(q);
    }
    out
}

fn url_host_matches(url_str: &str, domain: &str) -> bool {
    let Ok(parsed) = Url::parse(url_str) else {
        return false;
    };
    let Some(host) = parsed.host_str() else {
        return false;
    };
    let host = host.trim_start_matches("www.");
    host == domain || host.ends_with(&format!(".{domain}"))
}

fn entity_urls(v: &BookmarkView) -> Vec<(String, Option<String>, Option<String>)> {
    // (expanded_url, title, description)
    let mut out = Vec::new();
    for src in sources(v) {
        let Some(entities) = &src.entities else {
            continue;
        };
        let Some(urls) = entities.get("urls").and_then(|u| u.as_array()) else {
            continue;
        };
        for u in urls {
            let expanded = ["expanded_url", "unwound_url", "url"]
                .iter()
                .find_map(|k| u.get(*k).and_then(|x| x.as_str()))
                .map(str::to_string);
            if let Some(expanded) = expanded {
                if expanded.contains("://t.co") || expanded.starts_with("t.co") {
                    continue; // unexpanded short link, useless for topics
                }
                let title = u.get("title").and_then(|x| x.as_str()).map(str::to_string);
                let description = u
                    .get("description")
                    .and_then(|x| x.as_str())
                    .map(str::to_string);
                out.push((expanded, title, description));
            }
        }
    }
    out
}

/// Precise domain match: an expanded URL host equals/suffices the domain, or
/// the text mentions the domain.
pub fn matches_domain(v: &BookmarkView, domain: &str) -> bool {
    if entity_urls(v)
        .iter()
        .any(|(expanded, _, _)| url_host_matches(expanded, domain))
    {
        return true;
    }
    sources(v)
        .iter()
        .any(|src| src.text.to_lowercase().contains(domain))
}

/// Case-insensitive whole-word search (so "ai" won't match inside "said").
fn text_contains_word(lower_text: &str, word: &str) -> bool {
    let mut start = 0;
    while let Some(pos) = lower_text[start..].find(word) {
        let abs = start + pos;
        let before_ok = abs == 0
            || lower_text[..abs]
                .chars()
                .last()
                .map(|c| !c.is_alphanumeric())
                .unwrap_or(true);
        let end = abs + word.len();
        let after_ok = end >= lower_text.len()
            || lower_text[end..]
                .chars()
                .next()
                .map(|c| !c.is_alphanumeric())
                .unwrap_or(true);
        if before_ok && after_ok {
            return true;
        }
        start = abs + word.len().max(1);
    }
    false
}

/// Precise topic-word match: text word, hashtag, mention, or author.
pub fn matches_word(v: &BookmarkView, word: &str) -> bool {
    let w = word.to_lowercase();
    for src in sources(v) {
        if text_contains_word(&src.text.to_lowercase(), &w) {
            return true;
        }
        if let Some(entities) = &src.entities {
            if let Some(tags) = entities.get("hashtags").and_then(|x| x.as_array()) {
                if tags.iter().any(|t| {
                    t.get("tag")
                        .and_then(|x| x.as_str())
                        .map(|s| s.to_lowercase() == w)
                        .unwrap_or(false)
                }) {
                    return true;
                }
            }
            if let Some(mentions) = entities.get("mentions").and_then(|x| x.as_array()) {
                if mentions.iter().any(|m| {
                    m.get("username")
                        .and_then(|x| x.as_str())
                        .map(|s| s.to_lowercase() == w)
                        .unwrap_or(false)
                }) {
                    return true;
                }
            }
        }
        if let Some(author) = &src.author {
            if author
                .username
                .as_deref()
                .map(|u| u.to_lowercase() == w)
                .unwrap_or(false)
            {
                return true;
            }
            if author
                .name
                .as_deref()
                .map(|n| n.to_lowercase().contains(&w))
                .unwrap_or(false)
            {
                return true;
            }
        }
    }
    false
}

pub fn matches(v: &BookmarkView, needle: &str) -> bool {
    if is_domain_like(needle) {
        matches_domain(v, needle)
    } else {
        matches_word(v, needle)
    }
}

// ---------- extraction ----------

/// Words too common to be interesting as topics.
const STOPWORDS: &[&str] = &[
    "the", "a", "an", "and", "or", "but", "if", "then", "else", "when", "at", "by", "for", "with",
    "about", "against", "between", "into", "through", "during", "before", "after", "above",
    "below", "from", "up", "down", "in", "out", "on", "off", "over", "under", "again", "further",
    "once", "here", "there", "all", "any", "both", "each", "few", "more", "most", "other", "some",
    "such", "no", "nor", "not", "only", "own", "same", "so", "than", "too", "very", "can", "will",
    "just", "should", "now", "is", "are", "was", "were", "be", "been", "being", "have", "has",
    "had", "having", "do", "does", "did", "doing", "would", "could", "might", "must", "shall",
    "may", "this", "that", "these", "those", "am", "it", "its", "they", "them", "their", "we",
    "our", "you", "your", "he", "she", "his", "her", "i", "me", "my", "we", "us", "who", "whom",
    "which", "what", "where", "why", "how", "because", "as", "until", "while", "of",
    // twitter-isms and URL fragments
    "amp", "rt", "via", "u", "s", "t", "m", "re", "ve", "ll", "d", "o", "y", "http", "https", "www",
    "com", "org", "net", "io", "co", "gif",
];

fn tokens(text: &str) -> Vec<String> {
    text.to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { ' ' })
        .collect::<String>()
        .split(' ')
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

fn sort_items(items: &mut [TopicItem]) {
    items.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.label.cmp(&b.label)));
}

/// Extract topics (keywords, hashtags, top links, authors) from matched
/// bookmark views. Counts are per bookmark, so one spammy post can't
/// dominate a topic.
pub fn extract_topics(matched: &[&BookmarkView], needle: &str) -> TopicsReport {
    let mut keywords: HashMap<String, u64> = HashMap::new();
    let mut hashtags: HashMap<String, u64> = HashMap::new();
    let mut links: HashMap<String, (u64, String)> = HashMap::new();
    let mut authors: HashMap<String, u64> = HashMap::new();

    // Never suggest the needle itself (or its parts) as a topic.
    let needle_tokens: HashSet<String> = tokens(needle).into_iter().collect();

    for v in matched {
        // Keywords + hashtags: dedup within one bookmark, count across bookmarks.
        let mut kw_seen: HashSet<String> = HashSet::new();
        let mut tag_seen: HashSet<String> = HashSet::new();
        for src in sources(v) {
            for tok in tokens(&src.text) {
                if tok.len() < 3 || tok.len() > 25 || tok.chars().all(|c| c.is_ascii_digit()) {
                    continue;
                }
                if needle_tokens.contains(&tok) {
                    continue;
                }
                if is_stopword(&tok) {
                    continue;
                }
                kw_seen.insert(tok);
            }
            if let Some(entities) = &src.entities {
                if let Some(tags) = entities.get("hashtags").and_then(|x| x.as_array()) {
                    for t in tags {
                        if let Some(tag) = t.get("tag").and_then(|x| x.as_str()) {
                            if !needle_tokens.contains(&tag.to_lowercase()) {
                                tag_seen.insert(format!("#{tag}"));
                            }
                        }
                    }
                }
            }
        }
        for tok in kw_seen {
            *keywords.entry(tok).or_insert(0) += 1;
        }
        for tag in tag_seen {
            *hashtags.entry(tag).or_insert(0) += 1;
        }

        // Links: count per bookmark by expanded URL.
        for (expanded, title, description) in entity_urls(v) {
            let label = title
                .or(description)
                .unwrap_or_else(|| expanded.clone())
                .trim()
                .to_string();
            let label = if label.chars().count() > 120 {
                label.chars().take(120).collect::<String>() + "…"
            } else {
                label
            };
            let entry = links.entry(expanded.clone()).or_insert((0, label));
            entry.0 += 1;
        }

        // Authors of the matched content.
        if let Some(display) = v.retweeted.as_deref().or(Some(v)) {
            if let Some(author) = &display.author {
                let label = match &author.username {
                    Some(u) => format!("@{u}"),
                    None => author.name.clone().unwrap_or_else(|| "?".into()),
                };
                *authors.entry(label).or_insert(0) += 1;
            }
        }
    }

    let mut keywords: Vec<TopicItem> = keywords
        .into_iter()
        .map(|(label, count)| TopicItem {
            label,
            count,
            url: None,
        })
        .collect();
    sort_items(&mut keywords);
    // Prefer topics that appear in 2+ bookmarks, but fall back to top-10
    // when the corpus is small.
    if keywords.iter().filter(|k| k.count >= 2).count() >= 5 {
        keywords.retain(|k| k.count >= 2);
    } else if keywords.len() > 10 {
        keywords.truncate(10);
    }
    keywords.truncate(KEYWORD_LIMIT);

    let mut hashtags: Vec<TopicItem> = hashtags
        .into_iter()
        .map(|(label, count)| TopicItem {
            label,
            count,
            url: None,
        })
        .collect();
    sort_items(&mut hashtags);
    hashtags.truncate(HASHTAG_LIMIT);

    let mut links: Vec<TopicItem> = links
        .into_iter()
        .map(|(expanded, (count, label))| TopicItem {
            label,
            count,
            url: Some(expanded),
        })
        .collect();
    sort_items(&mut links);
    links.truncate(LINK_LIMIT);

    let mut authors: Vec<TopicItem> = authors
        .into_iter()
        .map(|(label, count)| TopicItem {
            label,
            count,
            url: None,
        })
        .collect();
    sort_items(&mut authors);
    authors.truncate(AUTHOR_LIMIT);

    TopicsReport {
        input: String::new(),
        needle: needle.to_string(),
        is_domain: is_domain_like(needle),
        scope: "local".into(),
        generated_at: now_rfc3339(),
        total_matches: matched.len() as u64,
        keywords,
        hashtags,
        links,
        authors,
    }
}

fn is_stopword(tok: &str) -> bool {
    STOPWORDS.contains(&tok)
}

/// Run the full analysis against the store: broad SQL candidates, precise
/// Rust filtering, topic extraction.
pub fn analyze(store: &Store, input: &str) -> Result<TopicsResult> {
    let input = input.trim();
    let needle = normalize(input);
    if needle.is_empty() {
        anyhow::bail!("provide a domain (e.g. arxiv.org) or a topic word (e.g. rust)");
    }
    let candidates = store.find_candidates(&needle, MAX_CANDIDATES)?;
    let matched: Vec<BookmarkView> = candidates
        .iter()
        .filter(|v| matches(v, &needle))
        .cloned()
        .collect();
    let matched_refs: Vec<&BookmarkView> = matched.iter().collect();
    let mut report = extract_topics(&matched_refs, &needle);
    report.input = input.to_string();
    Ok(TopicsResult { report, matched })
}

/// Build an X API search query for the input:
/// - domain-like → `url:"arxiv.org" -is:retweet` (matches any URL pointing
///   at that host)
/// - topic word/phrase → `"machine learning" -is:retweet`
pub fn build_search_query(input: &str) -> String {
    let needle = normalize(input);
    if is_domain_like(&needle) {
        format!("url:\"{needle}\" -is:retweet")
    } else {
        format!("\"{needle}\" -is:retweet")
    }
}

/// Same analysis as `analyze`, but sourced from a live search across all of
/// X (recent window, ~last 7 days) instead of the local archive. Results are
/// ingested into an in-memory store so the existing view-building and topic
/// extraction code is reused verbatim. Nothing is persisted to disk.
pub async fn analyze_x(
    client: &XClient,
    input: &str,
    max_posts: u64,
    page_size: u32,
    on_progress: &(dyn Fn(String) + Send + Sync),
) -> Result<TopicsResult> {
    let input = input.trim();
    let needle = normalize(input);
    if needle.is_empty() {
        anyhow::bail!("provide a domain (e.g. arxiv.org) or a topic word (e.g. rust)");
    }
    let query = build_search_query(input);
    tracing::info!("live X search: {query} (up to {max_posts} posts)");

    let store = Store::open_in_memory()?;
    let mut position = 0i64;
    let mut pagination: Option<String> = None;
    let mut fetched: u64 = 0;
    let mut page_size = page_size.clamp(10, 100);

    loop {
        on_progress(format!("searching X… {fetched} post(s) so far"));
        let page = match client
            .search_recent(&query, page_size, pagination.clone(), on_progress)
            .await
        {
            Ok(page) => page,
            Err(e) => {
                // Some tiers cap max_results below 100; fall back once.
                let msg = format!("{e:#}");
                if fetched == 0 && page_size > 10 && msg.contains("max_results") {
                    tracing::warn!("X rejected page size {page_size}; retrying with 10");
                    page_size = 10;
                    continue;
                }
                return Err(e);
            }
        };
        let now = now_rfc3339();
        let ingested = crate::sync::ingest_page(&store, &page, &mut position, &now)?;
        fetched += ingested.len as u64;
        pagination = page.meta.next_token.clone();
        if pagination.is_none() {
            break;
        }
        if fetched >= max_posts {
            tracing::info!("live search reached the {max_posts}-post cap");
            break;
        }
    }

    let mut result = analyze(&store, input)?;
    result.report.scope = "x".into();
    Ok(result)
}

// ---------- exports ----------

fn slim_bookmark(v: &BookmarkView) -> Value {
    json!({
        "id": v.id,
        "url": v.url,
        "created_at": v.created_at,
        "author_username": v.author.as_ref().and_then(|a| a.username.clone()),
        "author_name": v.author.as_ref().and_then(|a| a.name.clone()),
        "text": v.text,
    })
}

/// Full export: report + the matching bookmarks.
pub fn export_json(result: &TopicsResult) -> Value {
    let mut report = serde_json::to_value(&result.report).unwrap_or(Value::Null);
    if let Some(obj) = report.as_object_mut() {
        obj.insert(
            "bookmarks".into(),
            Value::Array(result.matched.iter().map(slim_bookmark).collect()),
        );
    }
    report
}

fn csv_field(s: &str) -> String {
    if s.contains(',') || s.contains('"') || s.contains('\n') {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

/// Topics only, spreadsheet-friendly.
pub fn export_csv(report: &TopicsReport) -> String {
    let mut out = String::from("kind,label,count,url\n");
    for (kind, items) in [
        ("keyword", &report.keywords),
        ("hashtag", &report.hashtags),
        ("link", &report.links),
        ("author", &report.authors),
    ] {
        for it in items {
            out.push_str(&format!(
                "{kind},{},{},{}\n",
                csv_field(&it.label),
                it.count,
                it.url.as_deref().map(csv_field).unwrap_or_default()
            ));
        }
    }
    out
}

/// Readable digest: topics + the matching bookmarks.
pub fn export_md(result: &TopicsResult) -> String {
    let r = &result.report;
    let source = if r.scope == "x" {
        "live X search (recent posts, ~last 7 days)"
    } else {
        "local bookmark archive"
    };
    let mut out = format!(
        "# Topics in posts matching \"{}\"\n\nGenerated: {}  \nSource: {source}  \nMatching posts: {}\n\n",
        r.input, r.generated_at, r.total_matches
    );
    for (title, items) in [
        ("Keywords", &r.keywords),
        ("Hashtags", &r.hashtags),
        ("Top links", &r.links),
        ("Authors", &r.authors),
    ] {
        if items.is_empty() {
            continue;
        }
        out.push_str(&format!("\n## {title}\n\n"));
        for it in items {
            match &it.url {
                Some(url) => out.push_str(&format!("- {}× [{}]({url})\n", it.count, it.label)),
                None => out.push_str(&format!("- {}× {}\n", it.count, it.label)),
            }
        }
    }
    if !result.matched.is_empty() {
        out.push_str("\n## Matching bookmarks\n");
        for (i, v) in result.matched.iter().enumerate() {
            let author = v
                .retweeted
                .as_deref()
                .or(Some(v))
                .and_then(|d| d.author.as_ref())
                .and_then(|a| a.username.clone())
                .unwrap_or_else(|| "?".into());
            out.push_str(&format!(
                "\n{}. {} @{} — {}\n",
                i + 1,
                v.created_at.as_deref().unwrap_or(""),
                author,
                v.url
            ));
            let snippet: String = v.text.chars().take(240).collect();
            out.push_str(&format!("   > {}\n", snippet.replace('\n', " ")));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::AuthorView;
    use serde_json::json;

    fn view(id: &str, text: &str, entities: Value, username: Option<&str>) -> BookmarkView {
        BookmarkView {
            id: id.into(),
            url: format!("https://x.com/{}/status/{id}", username.unwrap_or("i")),
            text: text.into(),
            created_at: None,
            lang: None,
            author: username.map(|u| AuthorView {
                id: "1".into(),
                username: Some(u.into()),
                name: Some(format!("Name of {u}")),
                description: None,
                profile_image_url: None,
                verified: false,
                verified_type: None,
            }),
            metrics: Default::default(),
            media: vec![],
            polls: vec![],
            entities: Some(entities),
            referenced: vec![],
            quoted: None,
            retweeted: None,
            is_reply: false,
            possibly_sensitive: false,
            position: None,
            first_seen_at: None,
            removed_at: None,
        }
    }

    #[test]
    fn builds_search_queries() {
        assert_eq!(
            build_search_query("https://www.arxiv.org/abs/1"),
            "url:\"arxiv.org\" -is:retweet"
        );
        assert_eq!(build_search_query("rust"), "\"rust\" -is:retweet");
        assert_eq!(
            build_search_query("machine learning"),
            "\"machine learning\" -is:retweet"
        );
    }

    #[test]
    fn normalizes_inputs() {
        assert_eq!(normalize("https://arxiv.org/abs/1706.03762"), "arxiv.org");
        assert_eq!(normalize("http://WWW.Arxiv.Org/x?y"), "arxiv.org");
        assert_eq!(normalize("arxiv.org."), "arxiv.org");
        assert_eq!(normalize("  Rust Lang "), "rust lang");
        assert!(is_domain_like("arxiv.org"));
        assert!(!is_domain_like("rust"));
        assert!(!is_domain_like("machine learning"));
    }

    #[test]
    fn domain_matching_is_precise() {
        let v = view(
            "1",
            "great paper here",
            json!({"urls": [{"expanded_url": "https://arxiv.org/abs/1706.03762"}]}),
            Some("alice"),
        );
        assert!(matches_domain(&v, "arxiv.org"));
        // needles are normalized by the caller before matching
        assert!(matches_domain(
            &v,
            &normalize("https://www.Arxiv.ORG/abs/1")
        ));
        // subdomain of the needle matches
        let v2 = view(
            "2",
            "",
            json!({"urls": [{"expanded_url": "https://blog.arxiv.org/post"}]}),
            None,
        );
        assert!(matches_domain(&v2, "arxiv.org"));
        // lookalike domain must NOT match
        let v3 = view(
            "3",
            "",
            json!({"urls": [{"expanded_url": "https://notarxiv.org/x"}]}),
            None,
        );
        assert!(!matches_domain(&v3, "arxiv.org"));
        // plain-text mention matches
        let v4 = view("4", "check arxiv.org for more", json!({}), None);
        assert!(matches_domain(&v4, "arxiv.org"));
        // unrelated does not
        let v5 = view("5", "nothing here", json!({}), None);
        assert!(!matches_domain(&v5, "arxiv.org"));
    }

    #[test]
    fn word_matching_respects_boundaries() {
        let v = view("1", "AI is the future", json!({}), Some("bob"));
        assert!(matches_word(&v, "ai"));
        assert!(matches_word(&v, "AI"));
        let v2 = view("2", "he said hello", json!({}), None);
        assert!(!matches_word(&v2, "ai"));
        // hashtag + author matches
        let v3 = view(
            "3",
            "loving this",
            json!({"hashtags": [{"tag": "Rust"}]}),
            None,
        );
        assert!(matches_word(&v3, "rust"));
        let v4 = view("4", "nothing", json!({}), Some("alice"));
        assert!(matches_word(&v4, "alice"));
        // multi-word phrase
        let v5 = view("5", "machine learning is fun", json!({}), None);
        assert!(matches_word(&v5, "machine learning"));
    }

    #[test]
    fn extracts_topics_with_counts() {
        let v1 = view(
            "1",
            "The transformers architecture explained",
            json!({"urls": [
                {"expanded_url": "https://arxiv.org/abs/1706.03762", "title": "Attention Is All You Need"},
                {"expanded_url": "https://t.co/xyz"}
            ], "hashtags": [{"tag": "ml"}]}),
            Some("alice"),
        );
        let v2 = view(
            "2",
            "transformers and attention, explained via the paper",
            json!({"hashtags": [{"tag": "ml"}]}),
            Some("alice"),
        );
        let v3 = view(
            "3",
            "totally unrelated cooking thread",
            json!({}),
            Some("chef"),
        );
        let matched = vec![&v1, &v2, &v3];
        let report = extract_topics(&matched, "arxiv.org");

        assert_eq!(report.total_matches, 3);
        // "transformers" appears in two bookmarks → count 2; stopword "the" gone;
        // needle token "arxiv"/"org" gone.
        let kw: HashMap<&str, u64> = report
            .keywords
            .iter()
            .map(|k| (k.label.as_str(), k.count))
            .collect();
        assert_eq!(kw.get("transformers"), Some(&2));
        assert!(!kw.contains_key("the"));
        assert!(!kw.contains_key("arxiv"));
        assert_eq!(kw.get("explained"), Some(&2));
        // hashtag counted once per bookmark
        assert_eq!(report.hashtags[0].label, "#ml");
        assert_eq!(report.hashtags[0].count, 2);
        // t.co link skipped, real link kept with its title
        assert_eq!(report.links.len(), 1);
        assert_eq!(report.links[0].label, "Attention Is All You Need");
        assert_eq!(
            report.links[0].url.as_deref(),
            Some("https://arxiv.org/abs/1706.03762")
        );
        // authors counted per bookmark
        let authors: HashMap<&str, u64> = report
            .authors
            .iter()
            .map(|a| (a.label.as_str(), a.count))
            .collect();
        assert_eq!(authors.get("@alice"), Some(&2));
        assert_eq!(authors.get("@chef"), Some(&1));
    }

    #[test]
    fn csv_escapes_and_exports() {
        let v1 = view(
            "1",
            "paper, \"the best\"",
            json!({"urls": [{"expanded_url": "https://arxiv.org/abs/1", "title": "A, Title"}]}),
            Some("alice"),
        );
        let matched = vec![&v1];
        let mut report = extract_topics(&matched, "arxiv.org");
        report.input = "arxiv.org".into();
        let result = TopicsResult {
            report: report.clone(),
            matched: vec![v1],
        };
        let csv = export_csv(&result.report);
        assert!(csv.starts_with("kind,label,count,url\n"));
        assert!(csv.contains("\"A, Title\""));
        let json = export_json(&result);
        assert_eq!(json["total_matches"], 1);
        assert_eq!(json["bookmarks"][0]["author_username"], "alice");
        let md = export_md(&result);
        assert!(md.contains("# Topics in posts matching \"arxiv.org\""));
        assert!(md.contains("Source: local bookmark archive"));
    }
}
