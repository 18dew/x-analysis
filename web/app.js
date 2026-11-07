/* Xanalysis web UI — vanilla JS, no build step. */

const state = {
  items: [],
  total: 0,
  offset: 0,
  limit: 50,
  q: "",
  domain: null,
  syncTimer: null,
  status: null,
};

const $ = (id) => document.getElementById(id);
const feed = $("feed");

// ---------- helpers ----------

function esc(s) {
  return String(s ?? "")
    .replaceAll("&", "&amp;")
    .replaceAll("<", "&lt;")
    .replaceAll(">", "&gt;")
    .replaceAll('"', "&quot;")
    .replaceAll("'", "&#39;");
}

function fmt(n) {
  if (n === null || n === undefined) return "–";
  if (n >= 1_000_000) return (n / 1_000_000).toFixed(1).replace(/\.0$/, "") + "M";
  if (n >= 1_000) return (n / 1_000).toFixed(1).replace(/\.0$/, "") + "K";
  return String(n);
}

function fmtDate(iso) {
  if (!iso) return "";
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return iso;
  return d.toLocaleString(undefined, {
    year: "numeric", month: "short", day: "numeric",
    hour: "2-digit", minute: "2-digit",
  });
}

function rel(iso) {
  if (!iso) return "";
  const ms = Date.now() - new Date(iso).getTime();
  if (Number.isNaN(ms)) return iso;
  const min = Math.round(ms / 60000);
  if (min < 1) return "just now";
  if (min < 60) return `${min}m ago`;
  const h = Math.round(min / 60);
  if (h < 24) return `${h}h ago`;
  return `${Math.round(h / 24)}d ago`;
}

function bigAvatar(u) {
  return (u || "").replace(/_normal\./, "_400x400.");
}

function toast(msg, ms = 3500) {
  const el = $("toast");
  el.textContent = msg;
  el.classList.remove("hidden");
  clearTimeout(el._t);
  el._t = setTimeout(() => el.classList.add("hidden"), ms);
}

async function api(path, opts) {
  const resp = await fetch(path, opts);
  let body = {};
  try { body = await resp.json(); } catch { /* non-JSON */ }
  if (!resp.ok) throw new Error(body.error || resp.statusText || `HTTP ${resp.status}`);
  return body;
}

// ---------- status ----------

async function loadStatus() {
  let s;
  try {
    s = await api("/api/status");
  } catch (e) {
    $("status").innerHTML = `<span class="chip err">server error: ${esc(e.message)}</span>`;
    return null;
  }
  state.status = s;
  renderStatus(s);
  renderBanner(s);
  if (s.sync && s.sync.state === "running") scheduleStatusPoll();
  return s;
}

function renderStatus(s) {
  const chips = [];
  if (s.authenticated && s.user) {
    chips.push(`<span class="chip ok">🟢 @${esc(s.user.username || "?")}</span>`);
  } else if (s.authenticated) {
    chips.push(`<span class="chip ok">🟢 connected</span>`);
  } else {
    chips.push(`<span class="chip err">🔴 not connected</span>`);
  }
  chips.push(`<span class="chip">🔖 ${fmt(s.total_bookmarks)}</span>`);
  if (s.last_sync_at) chips.push(`<span class="chip">synced ${rel(s.last_sync_at)}</span>`);
  if (s.sync && s.sync.state === "error") {
    chips.push(`<span class="chip err" title="${esc(s.sync.message)}">sync error</span>`);
  }
  $("status").innerHTML = chips.join("");

  const btn = $("sync-btn");
  if (s.sync && s.sync.state === "running") {
    btn.disabled = true;
    btn.textContent = `⟳ ${s.sync.message || "syncing…"}`;
  } else {
    btn.disabled = false;
    btn.textContent = "⟳ Sync";
  }

  const paths = s.paths || {};
  $("export-paths").textContent =
    `Files also live at: ${paths.jsonl || "?"} and ${paths.markdown || "?"}`;
}

function scheduleStatusPoll() {
  if (state.syncTimer) return;
  state.syncTimer = setInterval(async () => {
    const s = await loadStatus();
    if (!s || !s.sync || s.sync.state !== "running") {
      clearInterval(state.syncTimer);
      state.syncTimer = null;
      if (s && s.sync && s.sync.state === "error") {
        toast(`Sync failed: ${s.sync.message}`);
      } else {
        toast("✓ Sync finished");
      }
      await reloadBookmarks();
    }
  }, 2000);
}

// ---------- banner / login ----------

function renderBanner(s) {
  const el = $("banner");
  if (s.authenticated) {
    el.innerHTML = "";
    return;
  }
  const cb = esc(s.callback_url || "");
  el.innerHTML = `
    <div class="banner">
      <h2>Connect your X account</h2>
      <p>Xanalysis reads your bookmarks through the official X API. You need a
      developer app from <a href="https://developer.x.com" target="_blank" rel="noopener">developer.x.com</a>
      with this exact callback URL registered in its User authentication settings:</p>
      <p><code>${cb}</code></p>
      ${s.client_id_set
        ? `<button class="btn primary" id="login-btn">Connect X account</button>`
        : `<p>Then set <code>client_id</code> in the config file (or <code>X_CLIENT_ID</code> environment variable) and restart <code>xanalysis serve</code>.</p>`}
    </div>`;
  const btn = $("login-btn");
  if (btn) {
    btn.addEventListener("click", async () => {
      try {
        const { url } = await api("/api/login/start", { method: "POST" });
        location.href = url;
      } catch (e) {
        toast(`Login failed: ${e.message}`);
      }
    });
  }
}

// ---------- bookmarks ----------

async function loadBookmarks(append) {
  const params = new URLSearchParams({ limit: String(state.limit), offset: String(state.offset) });
  if (state.q) params.set("q", state.q);
  if (state.domain) params.set("domain", state.domain);
  renderFilterRow();
  let data;
  try {
    data = await api("/api/bookmarks?" + params);
  } catch (e) {
    toast(`Could not load bookmarks: ${e.message}`);
    return;
  }
  state.total = data.total;
  if (append) state.items.push(...data.items);
  else state.items = data.items;
  renderFeed();
}

async function reloadBookmarks() {
  state.offset = 0;
  await loadBookmarks(false);
}

function renderFeed() {
  if (!state.items.length) {
    feed.innerHTML = "";
    const empty = $("empty");
    empty.classList.remove("hidden");
    if (state.status && !state.status.authenticated) {
      empty.textContent = "Connect your account to get started.";
    } else if (state.q) {
      empty.textContent = `No bookmarks match “${state.q}”.`;
    } else {
      empty.textContent = "No bookmarks yet — hit ⟳ Sync to download them from X.";
    }
  } else {
    $("empty").classList.add("hidden");
    feed.innerHTML = state.items.map(card).join("");
  }
  const more = $("load-more");
  if (state.items.length < state.total) {
    more.classList.remove("hidden");
    more.textContent = `Load more (${state.total - state.items.length} left)`;
  } else {
    more.classList.add("hidden");
  }
}

// Render post text with entities linkified (URLs, mentions, hashtags).
function renderText(text, entities) {
  const spans = [];
  const push = (start, end, html) => {
    if (Number.isInteger(start) && Number.isInteger(end) && end > start) {
      spans.push({ start, end, html });
    }
  };
  if (entities) {
    for (const u of entities.urls || []) {
      const href = esc(u.expanded_url || u.url || "");
      const label = esc(u.display_url || u.expanded_url || u.url || "");
      const title = u.title ? ` title="${esc(u.title)}"` : "";
      push(u.start, u.end, `<a href="${href}" target="_blank" rel="noopener"${title}>${label}</a>`);
    }
    for (const m of entities.mentions || []) {
      push(m.start, m.end,
        `<a href="https://x.com/${esc(m.username)}" target="_blank" rel="noopener">@${esc(m.username)}</a>`);
    }
    for (const h of entities.hashtags || []) {
      push(h.start, h.end,
        `<a href="https://x.com/hashtag/${encodeURIComponent(h.tag || "")}" target="_blank" rel="noopener">#${esc(h.tag)}</a>`);
    }
    for (const c of entities.cashtags || []) {
      push(c.start, c.end,
        `<a href="https://x.com/search?q=%24${encodeURIComponent(c.tag || "")}" target="_blank" rel="noopener">$${esc(c.tag)}</a>`);
    }
  }

  spans.sort((a, b) => a.start - b.start);
  let out = "";
  let pos = 0;
  for (const s of spans) {
    if (s.start < pos) continue; // overlapping (e.g. url inside note text)
    out += esc(text.slice(pos, s.start));
    out += s.html;
    pos = s.end;
  }
  out += esc(text.slice(pos));

  if (!entities) {
    // Fallback: linkify bare URLs.
    out = out.replace(/(https?:\/\/[^\s<)]+)/g, (m) =>
      m.includes("href=") ? m : `<a href="${m}" target="_blank" rel="noopener">${m}</a>`
    );
  }
  return out;
}

function bestMp4(variants) {
  const mp4s = (variants || [])
    .filter((v) => v.content_type === "video/mp4" && v.url)
    .sort((a, b) => (b.bit_rate || 0) - (a.bit_rate || 0));
  return mp4s.length ? mp4s[0].url : null;
}

function renderMedia(media) {
  if (!media || !media.length) return "";
  const photos = media.filter((m) => m.type === "photo");
  const videos = media.filter((m) => m.type === "video" || m.type === "animated_gif");
  let html = "";
  if (photos.length) {
    html += `<div class="media-grid n${Math.min(photos.length, 4)}">` +
      photos.map((m) =>
        `<a href="${esc(m.url || "#")}" target="_blank" rel="noopener">
           <img loading="lazy" src="${esc(m.url || "")}" alt="${esc(m.alt_text || "bookmark image")}">
         </a>`).join("") +
      "</div>";
  }
  for (const m of videos) {
    const src = bestMp4(m.variants);
    if (src) {
      html += `<video controls playsinline preload="metadata"
        poster="${esc(m.preview_image_url || "")}" src="${esc(src)}"></video>`;
    } else if (m.preview_image_url) {
      html += `<a href="${esc(m.url || m.preview_image_url)}" target="_blank" rel="noopener">
        <img loading="lazy" src="${esc(m.preview_image_url)}" alt="${esc(m.alt_text || "video preview")}"></a>`;
    }
  }
  return html ? `<div class="media">${html}</div>` : "";
}

function renderPolls(polls) {
  if (!polls || !polls.length) return "";
  const totalVotes = (polls[0].options || []).reduce((a, o) => a + (o.votes || 0), 0);
  const options = (polls[0].options || [])
    .slice()
    .sort((a, b) => (a.position || 0) - (b.position || 0));
  const bars = options.map((o) => {
    const pct = totalVotes ? Math.round((100 * (o.votes || 0)) / totalVotes) : 0;
    return `<div class="poll-option">
      <div class="poll-fill" style="width:${pct}%"></div>
      <span>${esc(o.label)}</span><span class="votes">${pct}% · ${fmt(o.votes)}</span>
    </div>`;
  }).join("");
  return `<div class="polls">${bars}
    <div class="poll-meta" style="color:var(--muted);font-size:12px">
      ${fmt(totalVotes)} votes · ${esc(polls[0].voting_status || "")}
    </div></div>`;
}

function miniCard(v, cls) {
  const a = v.author || {};
  return `
    <a class="quoted" href="${esc(v.url)}" target="_blank" rel="noopener">
      <div class="q-head">
        <img class="q-avatar" src="${esc(bigAvatar(a.profile_image_url))}" alt="" onerror="this.style.display='none'">
        <span class="q-name">${esc(a.name || "Unknown")}</span>
        <span class="q-user">@${esc(a.username || "unknown")}</span>
      </div>
      <div class="q-text">${renderText(v.text, v.entities)}</div>
    </a>`;
}

function card(it) {
  const display = it.retweeted || it;
  const a = display.author || {};
  const head = it.retweeted
    ? `<div class="rt-note">🔁 Reposted by ${esc((it.author || {}).name || "@?")}</div>` : "";
  const reply = display.is_reply ? `<div class="reply-note">↩ Part of a reply thread</div>` : "";
  const quoted = display.quoted ? miniCard(display.quoted) : "";
  const media = renderMedia(display.media);
  const polls = renderPolls(display.polls);
  const m = display.metrics || {};
  const verified = a.verified ? `<span class="badge">${a.verified_type === "blue" ? "✓" : "✓ " + esc(a.verified_type || "verified")}</span>` : "";
  const sensitive = display.possibly_sensitive ? `<span class="chip">sensitive</span>` : "";

  return `
  <article class="card" data-id="${esc(it.id)}">
    ${head}
    <div class="card-head">
      <img class="avatar" src="${esc(bigAvatar(a.profile_image_url))}" alt="" onerror="this.style.visibility='hidden'">
      <div class="who">
        <div class="name">${esc(a.name || "Unknown")}${verified}</div>
        <div class="username">@${esc(a.username || "unknown")}</div>
      </div>
      <div class="when" title="${esc(display.created_at || "")}">${fmtDate(display.created_at)}</div>
    </div>
    ${reply}
    <div class="text">${renderText(display.text, display.entities)}</div>
    ${media}
    ${polls}
    ${quoted}
    <div class="foot">
      <span title="replies">💬 ${fmt(m.replies)}</span>
      <span title="reposts">🔁 ${fmt(m.reposts)}</span>
      <span title="likes">❤️ ${fmt(m.likes)}</span>
      <span title="views">👁 ${fmt(m.views)}</span>
      ${sensitive}
      <details class="raw"><summary>raw JSON</summary><pre>loading…</pre></details>
      <a class="orig" href="${esc(display.url)}" target="_blank" rel="noopener">Open on X ↗</a>
    </div>
  </article>`;
}

// Lazy-load raw JSON when a "raw JSON" expander is opened.
feed.addEventListener("toggle", async (e) => {
  const d = e.target;
  if (!(d instanceof HTMLDetailsElement) || !d.classList.contains("raw")) return;
  if (!d.open || d.dataset.loaded) return;
  const cardEl = d.closest(".card");
  const pre = d.querySelector("pre");
  try {
    const r = await api("/api/bookmarks/" + encodeURIComponent(cardEl.dataset.id));
    pre.textContent = JSON.stringify(r.raw, null, 2);
    d.dataset.loaded = "1";
  } catch (err) {
    pre.textContent = "failed to load: " + err.message;
  }
}, true);

// ---------- topic finder ----------

function toggleTopicsPanel(show) {
  const panel = $("topics-panel");
  const visible = show === undefined ? panel.classList.contains("hidden") : show;
  panel.classList.toggle("hidden", !visible);
  if (visible) {
    renderFilterRow();
    $("topics-input").focus();
  }
}

async function findTopics() {
  const input = $("topics-input").value.trim();
  const result = $("topics-result");
  if (!input) {
    result.innerHTML = `<div class="topics-empty">Enter a domain or topic first.</div>`;
    return;
  }
  const scopeX = $("topics-scope-x").checked;
  result.innerHTML = scopeX
    ? `<div class="topics-empty">Searching all of X live… this can take a moment.</div>`
    : `<div class="topics-empty">Analyzing…</div>`;
  let report;
  try {
    const params = new URLSearchParams({ domain: input, scope: scopeX ? "x" : "local" });
    report = await api("/api/topics?" + params);
  } catch (e) {
    result.innerHTML = `<div class="topics-empty">Failed: ${esc(e.message)}</div>`;
    return;
  }
  renderTopics(report);
}

function renderTopics(r) {
  const result = $("topics-result");
  const scopeBadge = r.scope === "x" ? "live X search" : "local bookmarks";
  const kindBadge = r.is_domain ? "domain" : "topic";
  let html = `
    <div class="topics-summary">
      <strong>${fmt(r.total_matches)}</strong> post(s) match
      <code>${esc(r.input)}</code>
      <span class="chip">${kindBadge}</span>
      <span class="chip">${scopeBadge}</span>
    </div>`;

  const groups = [
    ["keywords", "Keywords", "chips"],
    ["hashtags", "Hashtags", "chips"],
    ["links", "Top links", "links"],
    ["authors", "Authors", "chips"],
  ];
  let any = false;
  for (const [key, title, kind] of groups) {
    const items = r[key] || [];
    if (!items.length) continue;
    any = true;
    html += `<h3 class="topic-group-title">${title}</h3>`;
    if (kind === "links") {
      html += `<ul class="topic-links">` + items.map((it) =>
        `<li><span class="count">${it.count}×</span>
         <a href="${esc(it.url || "#")}" target="_blank" rel="noopener">${esc(it.label)}</a></li>`
      ).join("") + `</ul>`;
    } else {
      html += `<div class="topic-chips">` + items.map((it) =>
        `<button class="topic-chip" data-search="${esc(it.label.replace(/^[@#]/, ""))}" title="Search bookmarks for this">
           ${esc(it.label)}<span>${it.count}</span></button>`
      ).join("") + `</div>`;
    }
  }
  if (!any && r.total_matches > 0) {
    html += `<div class="topics-empty">Matched bookmarks, but no recurring topics found in them.</div>`;
  }
  if (r.total_matches === 0) {
    html = `<div class="topics-empty">${
      r.scope === "x"
        ? "No matching posts found in X's recent window."
        : `No bookmarks match “${esc(r.input)}”.`
    }</div>`;
  } else {
    const enc = encodeURIComponent(r.input);
    const scopeQ = `&scope=${r.scope}`;
    html += `
    <div class="topics-actions">
      ${r.scope === "local" ? `<button class="btn" id="topics-show">Show matching bookmarks</button>` : ""}
      <a class="btn" href="/api/topics/export?domain=${enc}${scopeQ}&format=json" download title="Topics + matching posts as JSON">⤓ JSON</a>
      <a class="btn" href="/api/topics/export?domain=${enc}${scopeQ}&format=csv" download title="Topic list as CSV">⤓ CSV</a>
      <a class="btn" href="/api/topics/export?domain=${enc}${scopeQ}&format=md" download title="Readable Markdown digest">⤓ MD</a>
    </div>`;
  }
  result.innerHTML = html;
}

function applyDomainFilter(domain) {
  state.domain = domain || null;
  $("search").value = "";
  state.q = "";
  reloadBookmarks();
  toggleTopicsPanel(false);
  window.scrollTo({ top: 0 });
}

function renderFilterRow() {
  const row = $("filter-row");
  if (!state.domain) {
    row.innerHTML = "";
    return;
  }
  row.innerHTML = `
    <div class="filter-chip-wrap">
      <span class="filter-chip">
        ◫ domain: <code>${esc(state.domain)}</code>
        <button class="filter-clear" title="Clear domain filter">✕</button>
      </span>
    </div>`;
  row.querySelector(".filter-clear").addEventListener("click", () => {
    state.domain = null;
    reloadBookmarks();
  });
}

// ---------- sync ----------

async function doSync() {
  const btn = $("sync-btn");
  btn.disabled = true;
  btn.textContent = "⟳ starting…";
  let resp;
  try {
    resp = await fetch("/api/sync", { method: "POST" });
  } catch (e) {
    toast("Sync request failed: " + e.message);
    await loadStatus();
    return;
  }
  if (resp.status === 409) {
    toast("A sync is already running");
    scheduleStatusPoll();
    return;
  }
  if (!resp.ok) {
    let msg = resp.statusText;
    try { msg = (await resp.json()).error || msg; } catch { /* ignore */ }
    toast("Sync failed: " + msg);
    btn.disabled = false;
    btn.textContent = "⟳ Sync";
    return;
  }
  scheduleStatusPoll();
}

// ---------- wiring ----------

let searchTimer = null;
$("search").addEventListener("input", (e) => {
  clearTimeout(searchTimer);
  searchTimer = setTimeout(() => {
    state.q = e.target.value.trim();
    reloadBookmarks();
  }, 300);
});

$("sync-btn").addEventListener("click", doSync);

// Topics panel wiring.
$("topics-btn").addEventListener("click", () => toggleTopicsPanel());
$("topics-close").addEventListener("click", () => toggleTopicsPanel(false));
$("topics-go").addEventListener("click", findTopics);
$("topics-input").addEventListener("keydown", (e) => {
  if (e.key === "Enter") findTopics();
});
$("topics-scope-x").addEventListener("change", (e) => {
  $("topics-scope-note").classList.toggle("hidden", !e.target.checked);
});
$("topics-result").addEventListener("click", (e) => {
  const chip = e.target.closest(".topic-chip");
  if (chip) {
    // Jump to the feed, searching for that topic.
    $("search").value = chip.dataset.search;
    state.q = chip.dataset.search;
    state.domain = null;
    toggleTopicsPanel(false);
    reloadBookmarks();
    window.scrollTo({ top: 0 });
    return;
  }
  if (e.target.closest("#topics-show")) {
    applyDomainFilter($("topics-input").value.trim());
  }
});
$("load-more").addEventListener("click", () => {
  state.offset += state.items.length;
  loadBookmarks(true);
});

// Close the export menu after clicking one of its links.
document.querySelectorAll("#export-menu .menu-items a").forEach((l) =>
  l.addEventListener("click", () =>
    document.getElementById("export-menu").removeAttribute("open")));

// Keep the sync button useful when a sync is already running in another tab.
window.addEventListener("focus", () => loadStatus());

(async function init() {
  const params = new URLSearchParams(location.search);
  if (params.get("login") === "success") toast("✓ Connected to X");
  if (params.get("login") === "error") {
    toast("Login failed: " + (params.get("reason") || "unknown error"), 8000);
  }
  history.replaceState({}, "", "/");

  await loadStatus();
  await reloadBookmarks();
})();
