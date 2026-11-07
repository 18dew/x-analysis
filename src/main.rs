use anyhow::{anyhow, bail, Context, Result};
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use xanalysis::api::XClient;
use xanalysis::auth::{self, TokenStore};
use xanalysis::config::Config;
use xanalysis::models::CallbackQuery;
use xanalysis::server;
use xanalysis::store::Store;
use xanalysis::sync::{run_sync, SyncHandle};

#[derive(Parser)]
#[command(
    name = "xanalysis",
    version,
    about = "Sync, browse, and analyze your X (Twitter) bookmarks locally"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Create the config directory and a template config file
    Init,
    /// Authorize Xanalysis to read your bookmarks (opens the browser)
    Login,
    /// Remove stored credentials
    Logout,
    /// Download your bookmarks into the local database and exports
    Sync {
        /// Re-fetch every page and reconcile removed bookmarks
        #[arg(long)]
        full: bool,
    },
    /// Start the local web UI
    Serve {
        /// Don't open the browser automatically
        #[arg(long)]
        no_open: bool,
    },
    /// Show auth, sync, and storage status
    Status,
    /// Regenerate exports from the local database
    Export {
        /// Output format: jsonl or md
        #[arg(long, default_value = "jsonl")]
        format: String,
        /// Output path (default: the standard export location)
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Find topics matching a domain (e.g. arxiv.org) or word
    Topics {
        /// Domain (e.g. arxiv.org) or topic word (e.g. rust) to analyze
        input: String,
        /// Where to search: local bookmarks (local) or all of X via live
        /// search (x)
        #[arg(long, default_value = "local")]
        scope: String,
        /// For --scope x: max posts to fetch (default from config, 300)
        #[arg(long)]
        max_posts: Option<u64>,
        /// Output format: table, json, csv, or md
        #[arg(long, default_value = "table")]
        format: String,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();
    let cfg = Config::load()?;

    match cli.command {
        Command::Init => init_cmd(cfg),
        Command::Login => login_cmd(cfg).await,
        Command::Logout => logout_cmd(cfg),
        Command::Sync { full } => sync_cmd(cfg, full).await,
        Command::Serve { no_open } => server::serve(cfg, !no_open).await,
        Command::Status => status_cmd(cfg).await,
        Command::Export { format, out } => export_cmd(cfg, &format, out),
        Command::Topics {
            input,
            scope,
            max_posts,
            format,
        } => topics_cmd(cfg, &input, &scope, max_posts, &format).await,
    }
}

fn init_cmd(cfg: Config) -> Result<()> {
    let created = cfg.write_default_config()?;
    if created {
        println!("✓ Created {}", cfg.config_path().display());
        println!("✓ Created {}", cfg.data_dir().display());
    } else {
        println!("Config already exists: {}", cfg.config_path().display());
    }
    println!("\nNext steps:");
    println!("  1. Create a developer app at https://developer.x.com");
    println!("     (README.md has the exact settings to pick).");
    println!("  2. Register this exact callback URL in your app settings:");
    println!("     {}", cfg.callback_url);
    println!(
        "  3. Put your OAuth 2.0 Client ID into {} (or export X_CLIENT_ID).",
        cfg.config_path().display()
    );
    println!("  4. xanalysis login");
    println!("  5. xanalysis sync && xanalysis serve");
    Ok(())
}

async fn login_cmd(cfg: Config) -> Result<()> {
    let client_id = cfg
        .client_id
        .clone()
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| {
            anyhow!(
                "X client_id is not configured.\n\
                 1) Create an app at https://developer.x.com (see README 'Setup').\n\
                 2) Register callback URL {} in the app's User authentication settings.\n\
                 3) Put the OAuth 2.0 Client ID in {} (or export X_CLIENT_ID).",
                cfg.callback_url,
                cfg.config_path().display()
            )
        })?;

    let pkce = auth::generate_pkce();
    let url = auth::authorize_url(
        &cfg.authorize_url,
        &client_id,
        &cfg.callback_url,
        auth::SCOPE,
        &pkce.state,
        &pkce.challenge,
    )?;

    let addr = cfg.listen_addr();
    let listener = tokio::net::TcpListener::bind(&addr).await.map_err(|e| {
        anyhow!("cannot listen on {addr} ({e}). Stop the running Xanalysis server first, or use the Connect button in the web UI.")
    })?;

    println!("Opening the browser to authorize Xanalysis…");
    println!("If it doesn't open, visit:\n\n  {url}\n");
    println!(
        "Waiting for the callback on {} (timeout 5 min)…",
        cfg.callback_url
    );
    let _ = open::that(&url);

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<CallbackQuery>();
    let tx = Arc::new(Mutex::new(Some(tx)));
    let app =
        axum::Router::new().route("/callback", axum::routing::get(cli_callback).with_state(tx));
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let q = tokio::time::timeout(Duration::from_secs(300), rx.recv())
        .await
        .map_err(|_| anyhow!("timed out waiting for authorization"))?
        .ok_or_else(|| anyhow!("callback channel closed unexpectedly"))?;

    let code = auth::check_callback(&q, &pkce.state)?;

    let http = reqwest::Client::new();
    let tokens = auth::exchange_code(
        &http,
        &cfg.token_url,
        &cfg.client_id,
        &cfg.client_secret,
        &cfg.callback_url,
        &code,
        &pkce.verifier,
    )
    .await
    .context("exchanging the authorization code failed")?;

    let token_store = Arc::new(TokenStore::load(cfg.tokens_path()));
    token_store.set(tokens)?;

    let client = XClient::new(&cfg, token_store);
    let me = client
        .get_me()
        .await
        .context("verifying the new tokens failed")?;

    println!(
        "✓ Logged in as {} (@{}) — tokens saved to {}",
        me.name.unwrap_or_default(),
        me.username.unwrap_or_default(),
        cfg.tokens_path().display()
    );
    println!("\nNext steps:");
    println!("  xanalysis sync    # download your bookmarks");
    println!(
        "  xanalysis serve   # browse them at http://localhost:{}",
        cfg.port
    );
    Ok(())
}

type CliCallbackState = Arc<Mutex<Option<tokio::sync::mpsc::UnboundedSender<CallbackQuery>>>>;

async fn cli_callback(
    axum::extract::State(tx): axum::extract::State<CliCallbackState>,
    axum::extract::Query(q): axum::extract::Query<CallbackQuery>,
) -> axum::response::Html<&'static str> {
    if let Some(tx) = tx.lock().unwrap().take() {
        let _ = tx.send(q);
    }
    axum::response::Html(
        "<!doctype html><html><head><meta charset=\"utf-8\"><title>Authorization received</title>\
         <style>body{font-family:system-ui;background:#0b0e13;color:#e6e9ef;display:grid;place-items:center;height:100vh;margin:0}\
         h1{font-weight:500}</style></head><body><div style=\"text-align:center\">\
         <h1>🔖 Authorization received</h1><p>You can close this tab and return to your terminal.</p>\
         </div></body></html>",
    )
}

fn logout_cmd(cfg: Config) -> Result<()> {
    TokenStore::load(cfg.tokens_path()).clear()?;
    println!("✓ Removed {}", cfg.tokens_path().display());
    Ok(())
}

async fn sync_cmd(cfg: Config, full: bool) -> Result<()> {
    let store = Store::open(&cfg.db_path())?;
    let tokens = Arc::new(TokenStore::load(cfg.tokens_path()));
    if tokens.get().is_none() {
        bail!(
            "not logged in. Run `xanalysis login` first (tokens not found at {}).",
            cfg.tokens_path().display()
        );
    }
    let client = XClient::new(&cfg, tokens);
    let handle = SyncHandle::new();

    println!(
        "Syncing bookmarks into {} (first sync pages through everything; rate limits are awaited)…",
        cfg.db_path().display()
    );
    let report = run_sync(&client, &store, &cfg, &handle, full, |_| {}).await?;

    println!("\n✓ {} sync finished", report.mode);
    println!("  pages fetched:  {}", report.pages);
    println!("  bookmarks seen: {}", report.seen);
    println!("  new:            {}", report.new);
    println!("  removed:        {}", report.removed);
    println!("  exports:        {}", cfg.jsonl_path().display());
    println!("                   {}", cfg.md_path().display());
    Ok(())
}

async fn status_cmd(cfg: Config) -> Result<()> {
    println!("home:     {}", cfg.home.display());
    println!("config:   {}", cfg.config_path().display());
    println!("tokens:   {}", cfg.tokens_path().display());
    println!("database: {}", cfg.db_path().display());
    println!(
        "exports:  {}, {}",
        cfg.jsonl_path().display(),
        cfg.md_path().display()
    );
    println!("callback: {}", cfg.callback_url);
    println!(
        "client_id: {}",
        cfg.client_id.as_deref().unwrap_or("(not set)")
    );

    let store = Store::open(&cfg.db_path())?;
    println!("bookmarks: {}", store.count_bookmarks()?);
    println!(
        "last sync:        {}",
        store
            .get_meta("last_sync_at")?
            .unwrap_or_else(|| "never".into())
    );
    println!(
        "last full sync:   {}",
        store
            .get_meta("last_full_sync_at")?
            .unwrap_or_else(|| "never".into())
    );

    let tokens = Arc::new(TokenStore::load(cfg.tokens_path()));
    match tokens.get() {
        Some(ts) => {
            let client = XClient::new(&cfg, tokens);
            match client.get_me().await {
                Ok(me) => println!(
                    "auth:     ✓ {} (@{}) — token expires {}",
                    me.name.unwrap_or_default(),
                    me.username.unwrap_or_default(),
                    chrono::DateTime::from_timestamp(ts.expires_at, 0)
                        .map(|t| t.to_rfc3339())
                        .unwrap_or_else(|| "?".into())
                ),
                Err(e) => println!("auth:     token present, but API call failed: {e:#}"),
            }
        }
        None => println!("auth:     not logged in (run `xanalysis login`)"),
    }
    Ok(())
}

async fn topics_cmd(
    cfg: Config,
    input: &str,
    scope: &str,
    max_posts: Option<u64>,
    format: &str,
) -> Result<()> {
    let result = match scope {
        "x" => {
            let tokens = Arc::new(TokenStore::load(cfg.tokens_path()));
            if tokens.get().is_none() {
                bail!("not logged in — `xanalysis login` is required for --scope x");
            }
            let client = XClient::new(&cfg, tokens);
            let max = max_posts.unwrap_or(cfg.max_search_posts);
            let progress: &(dyn Fn(String) + Send + Sync) = &|msg: String| eprintln!("  {msg}");
            eprintln!("Searching all of X (recent posts, ~last 7 days) for {input:?}…");
            xanalysis::topics::analyze_x(&client, input, max, cfg.search_page_size, progress)
                .await?
        }
        "local" => {
            let store = Store::open(&cfg.db_path())?;
            xanalysis::topics::analyze(&store, input)?
        }
        other => bail!("unknown scope {other:?} (use local or x)"),
    };
    let report = &result.report;
    match format {
        "json" => println!(
            "{}",
            serde_json::to_string_pretty(&xanalysis::topics::export_json(&result))?
        ),
        "csv" => print!("{}", xanalysis::topics::export_csv(report)),
        "md" | "markdown" => print!("{}", xanalysis::topics::export_md(&result)),
        "table" => print_topics_table(report),
        other => bail!("unknown format {other:?} (use table, json, csv, or md)"),
    }
    Ok(())
}

fn print_topics_table(report: &xanalysis::topics::TopicsReport) {
    let source = if report.scope == "x" {
        "live X search"
    } else {
        "local bookmarks"
    };
    println!(
        "Topics for \"{}\" [{source}] — {} matching post(s)",
        report.input, report.total_matches
    );
    if report.total_matches == 0 {
        if report.scope == "x" {
            println!("\nNo matching posts found in the recent window.");
        } else {
            println!("\nNo matching bookmarks. (Run `xanalysis sync` first.)");
        }
        return;
    }
    let list = |items: &[xanalysis::topics::TopicItem], n: usize| {
        items
            .iter()
            .take(n)
            .map(|i| format!("{} ({})", i.label, i.count))
            .collect::<Vec<_>>()
            .join(", ")
    };
    if !report.keywords.is_empty() {
        println!("\nKeywords: {}", list(&report.keywords, 15));
    }
    if !report.hashtags.is_empty() {
        println!("Hashtags: {}", list(&report.hashtags, 15));
    }
    if !report.links.is_empty() {
        println!("\nTop links:");
        for l in report.links.iter().take(10) {
            println!(
                "  {:>3}× {} — {}",
                l.count,
                l.label,
                l.url.as_deref().unwrap_or("")
            );
        }
    }
    if !report.authors.is_empty() {
        println!("\nAuthors: {}", list(&report.authors, 15));
    }
}

fn export_cmd(cfg: Config, format: &str, out: Option<PathBuf>) -> Result<()> {
    let store = Store::open(&cfg.db_path())?;
    match format {
        "jsonl" => {
            let mut w: Box<dyn std::io::Write> = match out {
                Some(path) => Box::new(std::fs::File::create(&path)?),
                None => Box::new(std::io::stdout().lock()),
            };
            let n = store.export_jsonl(&mut w)?;
            eprintln!("wrote {n} bookmarks");
        }
        "md" | "markdown" => {
            let mut w: Box<dyn std::io::Write> = match out {
                Some(path) => Box::new(std::fs::File::create(&path)?),
                None => Box::new(std::io::stdout().lock()),
            };
            let n = store.export_md(&mut w)?;
            eprintln!("wrote {n} bookmarks");
        }
        other => bail!("unknown format {other:?} (use jsonl or md)"),
    }
    Ok(())
}
