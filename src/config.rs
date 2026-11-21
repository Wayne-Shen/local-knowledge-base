use anyhow::{Context, Result, ensure};
use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs,
    path::{Component, Path, PathBuf},
};
use url::{Host, Url};

pub const MAX_QUERY_BYTES: usize = 2_000;
pub const DEFAULT_PORT: u16 = 8765;

#[derive(Debug)]
pub struct Config {
    pub data: PathBuf,
    pub port: u16,
    pub worker: Worker,
    pub sources: BTreeMap<String, Source>,
    pub models: Models,
    pub schedule: Schedule,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FileConfig {
    data_dir: PathBuf,
    #[serde(default = "default_port")]
    port: u16,
    worker: WorkerFile,
    #[serde(default)]
    sources: BTreeMap<String, SourceFile>,
    #[serde(default)]
    models: Models,
    #[serde(default)]
    schedule: Schedule,
}

fn default_port() -> u16 {
    DEFAULT_PORT
}

#[derive(Debug)]
pub struct Worker {
    pub python: PathBuf,
    pub script: PathBuf,
    pub ocr_languages: Vec<String>,
    pub timeout_seconds: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkerFile {
    python: Platform<PathBuf>,
    script: PathBuf,
    #[serde(default)]
    ocr_languages: Vec<String>,
    #[serde(default = "worker_timeout")]
    timeout_seconds: u64,
}

fn worker_timeout() -> u64 {
    180
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Platform<T> {
    all: T,
    macos: Option<T>,
    windows: Option<T>,
    linux: Option<T>,
}

impl<T> Platform<T> {
    fn current(self) -> T {
        match std::env::consts::OS {
            "macos" => self.macos,
            "windows" => self.windows,
            "linux" => self.linux,
            _ => None,
        }
        .unwrap_or(self.all)
    }
}

#[derive(Debug)]
pub struct Source {
    pub roots: Vec<PathBuf>,
    pub allow_cloud: bool,
    pub is_inbox: bool,
    include: GlobSet,
    exclude: GlobSet,
}

impl Source {
    pub fn accepts(&self, root: &Path, path: &Path) -> bool {
        let Ok(relative) = path.strip_prefix(root) else {
            return false;
        };
        (self.include.is_empty() || self.include.is_match(relative))
            && !self.exclude.is_match(relative)
    }
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum Preset {
    Inbox,
    Wechat,
    Feishu,
}

#[derive(Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
struct SourceFile {
    preset: Option<Preset>,
    paths: Platform<Vec<PathBuf>>,
    include: Vec<String>,
    exclude: Vec<String>,
    allow_cloud: bool,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Models {
    pub primary: Primary,
    pub fallback: Fallback,
    pub annotate: bool,
    pub reranker: Option<Reranker>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Reranker {
    pub url: String,
    pub api_key_file: Option<PathBuf>,
}

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Primary {
    pub url: String,
    pub chat_model: String,
    pub context_tokens: usize,
}

impl Default for Primary {
    fn default() -> Self {
        Self {
            url: "http://127.0.0.1:11434".into(),
            chat_model: "qwen3:4b".into(),
            context_tokens: 16_384,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Fallback {
    pub enabled: bool,
    pub model: String,
    pub credential_account: String,
}

impl Default for Fallback {
    fn default() -> Self {
        Self {
            enabled: false,
            model: "deepseek-flash".into(),
            credential_account: "default".into(),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Schedule {
    pub scan_seconds: u64,
    pub settle_seconds: u64,
    pub embedding_seconds: u64,
    pub embedding_batch: usize,
    pub max_file_mib: u64,
}

impl Default for Schedule {
    fn default() -> Self {
        Self {
            scan_seconds: 1800,
            settle_seconds: 3,
            embedding_seconds: 6,
            embedding_batch: 32,
            max_file_mib: 128,
        }
    }
}

impl Config {
    pub fn default_path() -> PathBuf {
        let local = PathBuf::from("config.yaml");
        if local.is_file() {
            return local;
        }
        dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("local-knowledge-base/config.yaml")
    }

    pub fn read(path: &Path) -> Result<Self> {
        Self::parse(
            path,
            &fs::read(path).with_context(|| format!("Read {}", path.display()))?,
        )
    }

    pub fn parse(path: &Path, bytes: &[u8]) -> Result<Self> {
        let raw: FileConfig = serde_yaml_ng::from_slice(bytes).context("Read config.yaml")?;
        let base = path.parent().context("Config path has no parent")?;
        let data = resolve(base, &raw.data_dir)?;
        let mut sources = BTreeMap::new();
        for (name, mut source) in raw.sources {
            ensure!(
                !name.is_empty()
                    && name.len() <= 64
                    && name
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-')),
                "Invalid source ID {name:?}. Use letters, digits, underscores or hyphens."
            );
            // The source ID becomes a directory name on Windows as well.
            let upper = name.to_ascii_uppercase();
            ensure!(
                !matches!(upper.as_str(), "CON" | "PRN" | "AUX" | "NUL")
                    && !(upper.len() == 4
                        && (upper.starts_with("COM") || upper.starts_with("LPT"))
                        && upper.as_bytes()[3].is_ascii_digit()),
                "Reserved source ID {name:?}"
            );
            let mut roots = source.paths.current();
            if roots.is_empty() {
                roots = preset_roots(source.preset, &data)?;
                if source.preset == Some(Preset::Wechat) && source.include.is_empty() {
                    source.include = [
                        "**/[Ff]ile/**",
                        "**/[Ff]iles/**",
                        "**/MsgAttach/**",
                        "**/MessageTemp/**",
                        "**/OpenData/**",
                    ]
                    .map(String::from)
                    .to_vec();
                }
            }
            let mut roots = roots
                .iter()
                .map(|root| resolve(base, root))
                .collect::<Result<Vec<_>>>()?;
            roots.sort();
            roots.dedup();
            ensure!(
                !roots
                    .iter()
                    .any(|root| root.starts_with(&data) && !root.starts_with(data.join("inbox"))),
                "Source {name} overlaps internal knowledge base storage"
            );
            sources.insert(
                name,
                Source {
                    roots,
                    allow_cloud: source.allow_cloud,
                    is_inbox: source.preset == Some(Preset::Inbox),
                    include: globs(&source.include)?,
                    exclude: globs(&source.exclude)?,
                },
            );
        }

        let mut models = raw.models;
        validate_loopback_origin(&models.primary.url, "Ollama")?;
        if let Some(reranker) = &mut models.reranker {
            validate_loopback_origin(&reranker.url, "Reranker")?;
            if let Some(path) = &mut reranker.api_key_file {
                *path = resolve(base, path)?;
            }
        }
        ensure!(
            !models.primary.chat_model.trim().is_empty(),
            "Missing local chat model"
        );
        ensure!(
            !models.primary.chat_model.contains("cloud")
                && !models.primary.chat_model.contains('/'),
            "Use an installed local Ollama model tag"
        );
        ensure!(
            (4096..=131_072).contains(&models.primary.context_tokens),
            "context_tokens must be between 4096 and 131072"
        );
        ensure!(
            !models.fallback.model.trim().is_empty()
                && !models.fallback.credential_account.trim().is_empty(),
            "DeepSeek model and credential account must be nonempty"
        );
        ensure!(raw.port > 0, "Port must be greater than zero");
        ensure!(
            (1..=3600).contains(&raw.worker.timeout_seconds),
            "Invalid worker timeout"
        );
        let schedule = raw.schedule;
        ensure!(
            (1..=86_400).contains(&schedule.scan_seconds)
                && (1..=86_400).contains(&schedule.embedding_seconds)
                && schedule.settle_seconds <= 3600
                && (1..=128).contains(&schedule.embedding_batch)
                && (1..=2048).contains(&schedule.max_file_mib),
            "Invalid scan or embedding schedule"
        );
        Ok(Self {
            data,
            port: raw.port,
            sources,
            models,
            schedule,
            worker: Worker {
                python: resolve(base, &raw.worker.python.current())?,
                script: resolve(base, &raw.worker.script)?,
                ocr_languages: raw.worker.ocr_languages,
                timeout_seconds: raw.worker.timeout_seconds,
            },
        })
    }

    pub fn apply_folder_controls(&mut self, controls: &BTreeMap<PathBuf, bool>) {
        self.sources.retain(|_, source| {
            source
                .roots
                .retain(|path| controls.get(path) != Some(&false));
            !source.roots.is_empty()
        });
        for (path, enabled) in controls {
            if *enabled && !self.has_root(path) {
                // This ID is longer than a valid config.yaml source ID.
                let digest = Sha256::digest(path.as_os_str().as_encoded_bytes());
                self.sources.insert(
                    format!("folder-{digest:x}"),
                    Source {
                        roots: vec![path.clone()],
                        allow_cloud: false,
                        is_inbox: false,
                        include: GlobSet::empty(),
                        exclude: GlobSet::empty(),
                    },
                );
            }
        }
    }

    pub fn has_root(&self, path: &Path) -> bool {
        self.sources
            .values()
            .any(|source| source.roots.iter().any(|root| root == path))
    }

    pub fn accepts(&self, name: &str, root: &Path, path: &Path) -> bool {
        self.sources.get(name).is_some_and(|source| {
            source.roots.iter().any(|candidate| candidate == root) && source.accepts(root, path)
        })
    }

    pub fn resolve_folder(&self, config_path: &Path, input: &Path) -> Result<PathBuf> {
        ensure!(
            input.is_absolute() || input.starts_with("~"),
            "Enter an absolute folder path or one beginning with ~/"
        );
        let path = resolve(
            config_path.parent().context("Config path has no parent")?,
            input,
        )?;
        let canonical = path
            .canonicalize()
            .with_context(|| format!("Open folder {}", path.display()))?;
        let data = self.data.canonicalize()?;
        ensure!(
            !canonical.starts_with(&data) || canonical.starts_with(data.join("inbox")),
            "The knowledge base's own storage cannot be watched"
        );
        fs::read_dir(&path).with_context(|| format!("Read folder {}", path.display()))?;

        // Use the configured spelling for existing roots, including their intake and filter settings.
        if let Some(root) = self
            .sources
            .values()
            .flat_map(|source| source.roots.iter())
            .find(|root| {
                **root == path || root.canonicalize().is_ok_and(|actual| actual == canonical)
            })
        {
            return Ok(root.clone());
        }
        let is_symlink = fs::symlink_metadata(&path)?.is_symlink();
        Ok(if is_symlink { canonical } else { path })
    }

    pub fn inboxes(&self) -> impl Iterator<Item = &Path> {
        self.sources
            .values()
            .filter(|source| source.is_inbox)
            .flat_map(|source| source.roots.iter().map(PathBuf::as_path))
    }

    pub fn same_storage(&self, other: &Self) -> bool {
        self.data == other.data && self.port == other.port
    }
}

fn validate_loopback_origin(value: &str, service: &str) -> Result<()> {
    let url = Url::parse(value).with_context(|| format!("Invalid {service} URL"))?;
    let loopback = match url.host() {
        Some(Host::Ipv4(address)) => address.is_loopback(),
        Some(Host::Ipv6(address)) => address.is_loopback(),
        _ => false,
    };
    ensure!(
        matches!(url.scheme(), "http" | "https") && loopback,
        "{service} URL must use a numeric loopback address"
    );
    ensure!(
        url.path() == "/"
            && url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none(),
        "{service} URL must be an origin without a path, credentials or query"
    );
    Ok(())
}

fn globs(patterns: &[String]) -> Result<GlobSet> {
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        builder.add(
            GlobBuilder::new(pattern)
                .literal_separator(true)
                .build()
                .with_context(|| format!("Invalid path glob {pattern:?}"))?,
        );
    }
    Ok(builder.build()?)
}

fn preset_roots(preset: Option<Preset>, data: &Path) -> Result<Vec<PathBuf>> {
    if preset == Some(Preset::Inbox) {
        return Ok(vec![data.join("inbox")]);
    }
    let home = dirs::home_dir().context("Cannot locate the user home folder")?;
    let candidates = match preset {
        Some(Preset::Wechat) if cfg!(target_os = "macos") => {
            let container = home.join("Library/Containers/com.tencent.xinWeChat/Data");
            vec![
                container.join("Documents/xwechat_files"),
                container.join("Documents/WeChat Files"),
                container.join("Library/Application Support/com.tencent.xinWeChat"),
            ]
        }
        Some(Preset::Wechat) if cfg!(target_os = "windows") => vec![
            dirs::document_dir()
                .unwrap_or_else(|| home.join("Documents"))
                .join("xwechat_files"),
        ],
        Some(Preset::Feishu) => vec![
            dirs::download_dir()
                .unwrap_or_else(|| home.join("Downloads"))
                .join("Feishu"),
        ],
        _ => anyhow::bail!("Define at least one source path for this platform"),
    };
    Ok(candidates)
}

fn resolve(base: &Path, path: &Path) -> Result<PathBuf> {
    ensure!(!path.as_os_str().is_empty(), "Empty configured path");
    let expanded = if path.starts_with("~") {
        dirs::home_dir()
            .context("Cannot expand home folder")?
            .join(path.strip_prefix("~")?)
    } else if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    };
    let mut normalized = PathBuf::new();
    for component in expanded.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            _ => normalized.push(component),
        }
    }
    ensure!(
        normalized.is_absolute(),
        "Expected absolute path {}",
        normalized.display()
    );
    Ok(normalized)
}
