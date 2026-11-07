---
title: Topic finder — analyze bookmarks by domain
date: 2026-11-07
status: done
---

# Topic finder — analyze bookmarks by domain

## Summary

Add a "Topics" feature: enter a domain (e.g. `arxiv.org`) or topic word (e.g.
`rust`); Xanalysis finds matching bookmarks, extracts relevant topics
(keywords, hashtags, top links, authors) from them, lists them in a new UI
panel, and can export the result as JSON/CSV/Markdown.

## Reason

The plan's goal is an agent-friendly local archive. Finding which themes live
inside the content bookmarked from a given site (and exporting that list)
makes the archive analyzable without reading every post.

## Scope

- In scope: topic extraction, matching (domain + word modes), REST endpoints,
  UI panel + feed domain filter, CLI `topics` subcommand, exports.
- Out of scope: remote URL fetching/scraping, embeddings/LLM analysis,
  bookmark folders.

## Files Affected

| File            | Change |
| --------------- | ------ |
| src/topics.rs   | NEW — normalize input, precise matchers, topic extraction, exporters |
| src/lib.rs      | register `topics` module |
| src/store.rs    | `find_candidates()` (broad SQL LIKE) + `list_bookmarks` gains optional domain filter |
| src/server.rs   | `GET /api/topics`, `GET /api/topics/export`, `domain` param on `/api/bookmarks` |
| src/main.rs     | `xanalysis topics <input> [--format table|json|csv|md]` |
| web/index.html  | Topics button + panel markup, filter row |
| web/app.js      | panel logic, topic chips → search, domain filter chip, export buttons |
| web/style.css   | panel/chip styles |
| tests/sync_test.rs | end-to-end topics assertion on the mock sync data |
| README.md       | document the feature |

## Steps

1. [x] Plan file
2. [x] `src/topics.rs` — normalization, domain/word matchers, keyword/hashtag/link/author extraction, json/csv/md exporters
3. [x] Store: `find_candidates` + domain filter in `list_bookmarks`
4. [x] Server endpoints + wire into router
5. [x] CLI `topics` subcommand
6. [x] UI panel, feed domain filter, exports
7. [x] Unit + integration tests
8. [x] README, build, clippy, smoke test

## Side Effects

- `list_bookmarks` signature gains a `domain` parameter (internal callers updated).
- `/api/bookmarks` accepts `domain=` (broad SQL match used for feed filtering;
  topics endpoint applies a stricter second-pass filter, so counts can differ
  slightly for odd inputs).

## Notes

- Input handling: if the input contains a dot and no spaces it is treated as a
  domain (matched against expanded URL hosts and text mentions); otherwise as
  a topic word (word-boundary text/hashtag/mention/author match).
- Candidates fetched with a generous SQL LIKE, then precisely filtered in
  Rust so e.g. "ai" does not match "said".
- Export includes the topic list plus the matching bookmark list (id, url,
  author, date, text).
