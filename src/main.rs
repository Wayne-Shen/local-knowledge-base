mod collector;
mod config;
mod folder_picker;
mod markdown;
mod mcp;
mod models;
mod rerank;
mod search;
mod server;
mod store;
mod worker;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use config::Config;
use reqwest::blocking::Client;
use serde::Serialize;
use serde_json::{Value, json};
use std::{
    fs::{self, File},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, RwLock,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

pub struct Runtime {
    config_path: PathBuf,
    settings: RwLock<Arc<Config>>,
    progress: RwLock<Progress>,
    full_text: search::FullText,
    vectors: RwLock<()>,
    cloud_gate: Mutex<()>,
    http: Client,
    scan_requested: AtomicBool,
    retry_requested: AtomicBool,
    reindex_requested: AtomicBool,
    stopping: AtomicBool,
    folder_access: folder_picker::FolderAccess,
    requests: Arc<tokio::sync::Semaphore>,
    _lock: File,
}

impl Runtime {
    fn config(&self) -> Arc<Config> {
        self.settings.read().unwrap().clone()
    }
}

#[derive(Clone, Default, Serialize)]
struct Progress {
    scanning: bool,
    last_scan: Option<String>,
    scan_error: Option<String>,
    config_error: Option<String>,
    embedding_error: Option<String>,
    annotation_error: Option<String>,
}

#[derive(Parser)]
#[command(version, about = "Local knowledge base")]
struct Cli {
    #[arg(short, long, env = "KB_CONFIG", global = true, value_name = "FILE")]
    config: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the collector and loopback Web service.
    Serve,
    /// Search for passages in archived documents.
    Search {
        query: String,
        #[arg(short, long, default_value_t = 10)]
        limit: usize,
        /// Search the Tantivy index without querying embeddings.
        #[arg(long)]
        keyword_only: bool,
        #[arg(long)]
        json: bool,
    },
    /// Ask a question and return links to supporting passages.
    Ask {
        question: String,
        /// Request DeepSeek for passages from eligible sources.
        #[arg(long)]
        cloud: bool,
        #[arg(long)]
        keyword_only: bool,
        #[arg(long)]
        json: bool,
    },
    /// Show indexing counts and processing status.
    Status,
    /// Schedule a full scan and retry earlier processing errors.
    Scan,
    /// Rebuild full-text and vector indexes from the existing catalog.
    Reindex,
    /// Serve search and question answering to an MCP client via stdio.
    Mcp,
    /// Save the DeepSeek API key in the platform credential store.
    SetKey,
    /// Print the local log of DeepSeek request bodies as JSON Lines.
    Audit {
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let config_path = std::path::absolute(cli.config.unwrap_or_else(Config::default_path))?;
    let bytes = fs::read(&config_path).with_context(|| {
        format!(
            "Read {}. Copy config.example.yaml to config.yaml to get started.",
            config_path.display()
        )
    })?;
    let config = Config::parse(&config_path, &bytes)?;
    match cli.command {
        Command::Serve => serve(config_path, bytes, config),
        Command::SetKey => models::set_credential(&config.models.fallback.credential_account),
        Command::Audit { limit } => {
            anyhow::ensure!(
                config.data.join("metadata.sqlite3").is_file(),
                "Start kb serve to initialize the catalog"
            );
            store::Store::open(&config.data)?.print_audit(limit)
        }
        command => dispatch(&Api::new(config.port)?, command),
    }
}

fn serve(config_path: PathBuf, config_bytes: Vec<u8>, mut config: Config) -> Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();
    let lock = collector::initialize_data(&config)
        .with_context(|| format!("Initialize knowledge base at {}", config.data.display()))?;
    let mut store = store::Store::open(&config.data)?;
    store.initialize()?;
    config.apply_folder_controls(&store.folder_controls()?);
    let mut bookmarks = store.folder_bookmarks()?;
    bookmarks.retain(|path, _| config.has_root(path));
    let folder_access = folder_picker::FolderAccess::new(bookmarks);
    let (full_text, text_created) = search::FullText::open(&config.data.join(search::INDEX_DIR))?;
    store.prepare_text_index(search::INDEX_DIR, text_created)?;
    let port = config.port;
    let listener = std::net::TcpListener::bind(("127.0.0.1", port))
        .with_context(|| format!("Bind 127.0.0.1:{port}"))?;
    listener.set_nonblocking(true)?;
    let runtime = Arc::new(Runtime {
        config_path,
        settings: RwLock::new(Arc::new(config)),
        progress: RwLock::new(Progress::default()),
        full_text,
        vectors: RwLock::new(()),
        cloud_gate: Mutex::new(()),
        http: models::client()?,
        scan_requested: AtomicBool::new(false),
        retry_requested: AtomicBool::new(false),
        reindex_requested: AtomicBool::new(false),
        stopping: AtomicBool::new(false),
        folder_access,
        requests: Arc::new(tokio::sync::Semaphore::new(4)),
        _lock: lock,
    });
    let executor = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;
    let collector = collector::start(runtime.clone(), config_bytes)?;
    let result = run_server(&executor, runtime.clone(), listener);
    runtime.stopping.store(true, Ordering::Relaxed);
    worker::stop();
    collector
        .join()
        .map_err(|_| anyhow::anyhow!("Collector stopped unexpectedly"))?;
    drop(executor);
    result
}

fn run_server(
    executor: &tokio::runtime::Runtime,
    runtime: Arc<Runtime>,
    listener: std::net::TcpListener,
) -> Result<()> {
    #[cfg(target_os = "macos")]
    if runtime.folder_access.picker_available {
        struct StopPicker;
        impl Drop for StopPicker {
            fn drop(&mut self) {
                folder_picker::stop();
            }
        }
        let server = executor.spawn(async move {
            let _stop_picker = StopPicker;
            server::run(runtime, listener).await
        });
        folder_picker::run();
        return executor
            .block_on(server)
            .context("Web service stopped unexpectedly")?;
    }
    executor.block_on(server::run(runtime, listener))
}

pub struct Api {
    http: Client,
    base: String,
}

impl Api {
    fn new(port: u16) -> Result<Self> {
        let http = Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(2))
            .timeout(Duration::from_secs(260))
            .build()?;
        Ok(Self {
            http,
            base: format!("http://127.0.0.1:{port}"),
        })
    }

    fn call(&self, endpoint: &str, input: Option<&Value>) -> Result<Value> {
        let url = format!("{}/api/{endpoint}", self.base);
        let request = match input {
            Some(input) => self.http.post(url).json(input),
            None => self.http.get(url),
        };
        let response = request
            .send()
            .context("Connect to the local knowledge base. Start kb serve first.")?;
        let status = response.status();
        let mut body: Value = response
            .json()
            .with_context(|| format!("Read local service response (HTTP {status})"))?;
        if !status.is_success() {
            bail!(
                "{}",
                body["error"]
                    .as_str()
                    .unwrap_or("Local service request failed")
            );
        }
        for field in ["hits", "sources"] {
            if let Some(hits) = body.get_mut(field).and_then(Value::as_array_mut) {
                for hit in hits {
                    if let Some(path) = hit["original_url"].as_str() {
                        hit["original_url"] = json!(format!("{}{path}", self.base));
                    }
                }
            }
        }
        Ok(body)
    }
}

fn dispatch(api: &Api, command: Command) -> Result<()> {
    match command {
        Command::Search {
            query,
            limit,
            keyword_only,
            json: raw,
        } => {
            let request = search::Request {
                query,
                limit,
                keyword_only,
            };
            request.validate()?;
            let response = api.call("search", Some(&serde_json::to_value(request)?))?;
            if raw {
                print_json(&response)?;
            } else {
                print_hits(&response["hits"], false);
                if response["hits"].as_array().is_some_and(Vec::is_empty) {
                    println!("No matching passages.");
                }
                print_warning(&response);
            }
        }
        Command::Ask {
            question,
            cloud,
            keyword_only,
            json: raw,
        } => {
            let request = models::Ask {
                question,
                cloud,
                keyword_only,
            };
            let response = api.call("ask", Some(&serde_json::to_value(request)?))?;
            if raw {
                print_json(&response)?;
            } else {
                println!("{}", response["text"].as_str().unwrap_or(""));
                if let Some(model) = response["model"].as_str() {
                    println!("\nModel: {model}");
                }
                print_hits(&response["sources"], true);
                print_warning(&response);
            }
        }
        Command::Status => print_json(&api.call("status", None)?)?,
        Command::Scan => print_json(&api.call("scan", Some(&json!({})))?)?,
        Command::Reindex => print_json(&api.call("reindex", Some(&json!({})))?)?,
        Command::Mcp => mcp::run(api)?,
        _ => unreachable!(),
    }
    Ok(())
}

fn print_json(value: &Value) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

fn print_warning(response: &Value) {
    if let Some(warning) = response["warning"].as_str() {
        eprintln!("{warning}");
    }
}

fn print_hits(value: &Value, compact: bool) {
    if let Some(hits) = value.as_array() {
        for (index, hit) in hits.iter().enumerate() {
            let get = |field| hit[field].as_str().unwrap_or("");
            println!(
                "\n[{}] {} · {} · {}",
                index + 1,
                get("title"),
                get("source"),
                get("locator")
            );
            if !compact {
                println!("{}", get("snippet"));
            }
            println!("{}", get("original_url"));
        }
    }
}

fn resolve_archive(data: &Path, hash: &str) -> Result<Option<(PathBuf, String)>> {
    if hash.len() != 64
        || !hash
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return Ok(None);
    }
    let store = store::Store::open(data)?;
    Ok(store
        .archive(hash)?
        .map(|(path, title)| (data.join("archive").join(path), title)))
}
