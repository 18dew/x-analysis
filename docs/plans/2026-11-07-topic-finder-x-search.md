---
title: Topic finder — live search across all of X
date: 2026-11-07
status: done
---

# Topic finder — live search across all of X

## Summary

Extend the topic finder with a second scope: besides `local` (bookmarks
archive), a `x` scope runs a live X API search across all posts (recent
window) for the given domain/topic and extracts topics from those results
using the same machinery.

## Reason

User asked for topic search "not just in bookmarks but in whole of x.com".
The local archive only covers bookmarked posts; live search covers all
public posts.

## Scope

- In scope: `GET /2/tweets/search/recent` client support, in-memory
  ingestion + topic extraction for live results, `scope=local|x` in API/CLI,
  UI toggle, exports, tests, docs.
- Out of scope: full-archive search (`/all`, higher tiers — config override
  possible later), streaming, saving live results into the archive.

## Files Affected

| File | Change |
| ---- | ------ |
| src/api.rs | extract shared paged-GET core; add `search_recent` |
| src/sync.rs | extract reusable `ingest_page()` from the sync loop |
| src/store.rs | `Store::open_in_memory()` |
| src/topics.rs | `build_search_query`, `analyze_x`, `scope` field on report |
| src/config.rs | `max_search_posts`, `search_page_size` settings |
| src/server.rs | `scope`/`max_posts` params on topics endpoints |
| src/main.rs | `--scope`, `--max-posts` flags |
| web/ | scope toggle in panel, scope badge, export links carry scope |
| tests/sync_test.rs | mock search route + live-search tests |
| README.md | document scopes, tier/credit + ~7-day window caveats |

## Steps

1. [x] Plan file
2. [x] API client: shared paging + `search_recent`
3. [x] `ingest_page` extraction + in-memory store
4. [x] `analyze_x` + search query builder
5. [x] Config, server params, CLI flags
6. [x] UI toggle + badges + export scope
7. [x] Tests (mock search: query operator validation, page-size fallback, pagination, max cap)
8. [x] README + build + clippy + live smoke test

## Side Effects

- `extract_topics` reports gain a `scope` field (`local` | `x`).
- Live search uses the pay-per-use X API (costs credits; recent window only,
  ~last 7 days); requires login; errors surface if the tier lacks access.

## Notes

- Domain mode searches `url:"arxiv.org" -is:retweet`; word mode searches the
  quoted phrase. Precise Rust-side filtering still runs on results.
- Some tiers cap `max_results` below 100; first-request 400 mentioning
  `max_results` triggers an automatic fallback to 10.
- Live results are ingested into an in-memory SQLite store so the existing
  view-building/topic code is reused verbatim.
