use crate::{
    Runtime,
    config::{Config, Source},
    models,
    search::FullText,
    store::{Archive, Origin, Store, path_string},
    worker,
};
use anyhow::{Context, Result, ensure};
use chrono::{Datelike, Local};
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeSet, HashSet},
    fs::{self, File, Metadata},
    io::{Read, Write},
    path::{Component, Path, PathBuf},
    sync::{Arc, atomic::Ordering, mpsc},
    thread,
    time::{Duration, Instant},
};
use tantivy::{IndexWriter, TantivyDocument};
use tempfile::NamedTempFile;
use tracing::warn;
use walkdir::WalkDir;

const TEXT_BATCH: usize = 400;

pub fn start(runtime: Arc<Runtime>, config_bytes: Vec<u8>) -> Result<thread::JoinHandle<()>> {
    let annotate_runtime = runtime.clone();
    thread::Builder::new()
        .name("kb-annotations".into())
        .spawn(move || annotations(annotate_runtime))?;
    let collector = Collector::new(runtime, config_bytes)?;
    Ok(thread::Builder::new()
        .name("kb-collector".into())
        .spawn(move || collector.run())?)
}

struct Collector {
    runtime: Arc<Runtime>,
    store: Store,
    index_writer: IndexWriter<TantivyDocument>,
    config_bytes: Vec<u8>,
    last_scan: Option<Instant>,
    next_embedding: Instant,
    pending: HashSet<PathBuf>,
    watcher: Option<RecommendedWatcher>,
    events: mpsc::Receiver<notify::Result<Event>>,
    watched: BTreeSet<PathBuf>,
}

impl Collector {
    fn new(runtime: Arc<Runtime>, config_bytes: Vec<u8>) -> Result<Self> {
        let (sender, events) = mpsc::sync_channel(2048);
        let watcher_runtime = runtime.clone();
        let watcher = notify::recommended_watcher(move |event| {
            if sender.try_send(event).is_err() {
                watcher_runtime
                    .scan_requested
                    .store(true, Ordering::Relaxed);
            }
        })
        .inspect_err(|error| warn!("File notifications unavailable: {error}"))
        .ok();
        let store = Store::open(&runtime.config().data)?;
        let index_writer = runtime.full_text.writer()?;
        Ok(Self {
            runtime,
            store,
            index_writer,
            config_bytes,
            last_scan: None,
            next_embedding: Instant::now(),
            pending: HashSet::new(),
            watcher,
            events,
            watched: BTreeSet::new(),
        })
    }

    fn run(mut self) {
        let mut more_work = true;
        while !self.runtime.stopping.load(Ordering::Relaxed) {
            if !more_work {
                // Keep quiet filesystem bursts together and poll for config replacements.
                thread::sleep(Duration::from_millis(850));
            }
            more_work = self.tick().unwrap_or_else(|error| {
                warn!("Collector: {error:#}");
                false
            });
        }
    }

    fn refresh_config(&mut self) -> bool {
        let changed = fs::read(&self.runtime.config_path).and_then(|bytes| {
            if bytes == self.config_bytes
                && self.runtime.progress.read().unwrap().config_error.is_none()
            {
                return Ok(false);
            }
            self.config_bytes = bytes;
            let _cloud = self.runtime.cloud_gate.lock().unwrap();
            let mut next = Config::parse(&self.runtime.config_path, &self.config_bytes)
                .map_err(std::io::Error::other)?;
            if !next.same_storage(&self.runtime.config()) {
                return Err(std::io::Error::other(
                    "Restart kb after changing data_dir or port",
                ));
            }
            let controls = self
                .store
                .folder_controls()
                .map_err(std::io::Error::other)?;
            next.apply_folder_controls(&controls);
            *self.runtime.settings.write().unwrap() = Arc::new(next);
            self.next_embedding = Instant::now();
            Ok(true)
        });
        let mut progress = self.runtime.progress.write().unwrap();
        match changed {
            Ok(true) => {
                progress.config_error = None;
                true
            }
            Ok(false) => false,
            Err(error) => {
                let message = error.to_string();
                if progress.config_error.as_ref() != Some(&message) {
                    warn!("Configuration: {message}");
                }
                progress.config_error = Some(message);
                false
            }
        }
    }

    fn tick(&mut self) -> Result<bool> {
        let config_changed = self.refresh_config();
        let scan_requested = self.runtime.scan_requested.swap(false, Ordering::Acquire);
        let config = self.runtime.config();
        let watch_changed = self.update_watcher(&config);
        let mut full = scan_requested
            || config_changed
            || watch_changed
            || self.last_scan.is_none_or(|last| {
                last.elapsed() >= Duration::from_secs(config.schedule.scan_seconds)
            });
        if self.runtime.retry_requested.swap(false, Ordering::Relaxed) {
            self.store.retry_errors()?;
            self.next_embedding = Instant::now();
        }
        if self
            .runtime
            .reindex_requested
            .swap(false, Ordering::Relaxed)
        {
            self.store.request_reindex()?;
            self.next_embedding = Instant::now();
        }

        let mut paths = std::mem::take(&mut self.pending);
        for event in self.events.try_iter().take(4096) {
            match event {
                Ok(event) if event.need_rescan() => full = true,
                Ok(event)
                    if matches!(
                        event.kind,
                        EventKind::Create(_)
                            | EventKind::Modify(_)
                            | EventKind::Any
                            | EventKind::Other
                    ) =>
                {
                    paths.extend(event.paths);
                }
                Err(error) => {
                    warn!("Filesystem notifications: {error}");
                    full = true;
                }
                _ => {}
            }
        }
        if full || !paths.is_empty() {
            self.runtime.progress.write().unwrap().scanning = true;
            let mut scan_error = None;
            for (name, source) in &config.sources {
                for root in &source.roots {
                    if self.runtime.stopping.load(Ordering::Relaxed) {
                        break;
                    }
                    if !self.runtime.folder_access.can_scan(root) {
                        continue;
                    }
                    if full {
                        self.walk(&config, name, source, root, root, &mut scan_error);
                        continue;
                    }
                    for path in &paths {
                        if path.starts_with(root) {
                            self.walk(&config, name, source, root, path, &mut scan_error);
                        }
                    }
                }
            }
            let mut progress = self.runtime.progress.write().unwrap();
            progress.scanning = false;
            progress.scan_error = scan_error;
            if full {
                progress.last_scan = Some(chrono::Utc::now().to_rfc3339());
                self.last_scan = Some(Instant::now());
            }
        }

        let parse_jobs = self.store.parse_jobs(4)?;
        let parsed_count = parse_jobs.len();
        for (hash, relative) in parse_jobs {
            if self.runtime.stopping.load(Ordering::Relaxed) {
                return Ok(false);
            }
            match worker::parse(&config, &config.data.join("archive").join(relative)) {
                Ok(sections) => self.store.parsed(&hash, sections)?,
                Err(error) => self.store.parse_failed(&hash, &format!("{error:#}"))?,
            }
        }

        let table_jobs = self.store.pdf_table_jobs(3)?;
        let table_count = table_jobs.len();
        for (hash, relative) in table_jobs {
            if self.runtime.stopping.load(Ordering::Relaxed) {
                return Ok(false);
            }
            match worker::pdf_rows(&config, &config.data.join("archive").join(relative)) {
                Ok(rows) => self.store.store_pdf_rows(&hash, rows)?,
                Err(error) => {
                    warn!("PDF table extraction for {hash}: {error:#}");
                    self.store.pdf_tables_failed(&hash, &format!("{error:#}"))?;
                }
            }
        }

        let jobs = self.store.index_jobs(TEXT_BATCH)?;
        if !jobs.is_empty() {
            if let Err(error) =
                FullText::update(&self.runtime.full_text, &mut self.index_writer, &jobs)
            {
                // Keep queued updates from a failed commit out of the next batch.
                let _ = self.index_writer.rollback();
                return Err(error);
            }
            self.store.mark_indexed(jobs.iter().map(|job| job.id))?;
        }
        if Instant::now() >= self.next_embedding || self.store.reset_vectors_pending()? {
            self.embed(&config)?;
        }
        Ok(parsed_count == 4 || table_count == 3 || jobs.len() == TEXT_BATCH)
    }

    fn update_watcher(&mut self, config: &Config) -> bool {
        let Some(watcher) = &mut self.watcher else {
            return false;
        };
        let desired: BTreeSet<_> = config
            .sources
            .values()
            .flat_map(|source| source.roots.iter())
            .filter(|root| self.runtime.folder_access.can_scan(root) && root.is_dir())
            .cloned()
            .collect();
        let changed = desired != self.watched;
        for root in self.watched.difference(&desired) {
            let _ = watcher.unwatch(root);
        }
        for root in desired.difference(&self.watched) {
            if let Err(error) = watcher.watch(root, RecursiveMode::Recursive) {
                warn!("Cannot watch {}: {error}", root.display());
            }
        }
        self.watched = desired;
        changed
    }

    fn walk(
        &mut self,
        config: &Config,
        name: &str,
        source: &Source,
        root: &Path,
        start: &Path,
        scan_error: &mut Option<String>,
    ) {
        if !visible(&config.data, root, start) {
            return;
        }
        match fs::symlink_metadata(start) {
            Ok(metadata) if metadata.is_file() => {
                self.visit(config, name, source, root, start, scan_error);
            }
            Ok(metadata) if metadata.is_dir() => {
                let entries = WalkDir::new(start)
                    .follow_root_links(false)
                    .into_iter()
                    .filter_entry(|entry| visible(&config.data, root, entry.path()));
                for entry in entries {
                    if self.runtime.stopping.load(Ordering::Relaxed) {
                        return;
                    }
                    match entry {
                        Ok(entry) if entry.file_type().is_file() => {
                            self.visit(config, name, source, root, entry.path(), scan_error);
                        }
                        Err(error) => note_scan_error(scan_error, error.to_string()),
                        _ => {}
                    }
                }
            }
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
                note_scan_error(scan_error, format!("{}: {error}", start.display()));
            }
            _ => {}
        }
    }

    fn visit(
        &mut self,
        config: &Config,
        name: &str,
        source: &Source,
        root: &Path,
        path: &Path,
        scan_error: &mut Option<String>,
    ) {
        if !source.accepts(root, path) {
            return;
        }
        let Some(extension) = path
            .extension()
            .and_then(|ext| ext.to_str())
            .map(str::to_ascii_lowercase)
        else {
            return;
        };
        if !is_supported(&extension) {
            return;
        }
        match archive(
            &self.runtime,
            &mut self.store,
            config,
            name,
            root,
            path,
            &extension,
        ) {
            Ok(Outcome::Stable) => {}
            Ok(Outcome::Retry) => {
                self.pending.insert(path.to_path_buf());
            }
            Err(error) => note_scan_error(scan_error, format!("{}: {error:#}", path.display())),
        }
    }

    fn embed(&mut self, config: &Config) -> Result<()> {
        let _vectors = self.runtime.vectors.write().unwrap();
        let directory = config.data.join("lance");
        if self.store.reset_vectors_pending()? {
            match fs::remove_dir_all(&directory) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error).context("Reset LanceDB"),
            }
            self.store.finish_vector_reset()?;
        }
        let rows = self.store.embedding_jobs(config.schedule.embedding_batch)?;
        if rows.is_empty() {
            return Ok(());
        }
        self.next_embedding =
            Instant::now() + Duration::from_secs(config.schedule.embedding_seconds);
        let outcome = worker::embed(config, &rows);
        let mut progress = self.runtime.progress.write().unwrap();
        match outcome {
            Ok(()) => {
                self.store.mark_embedded(rows.iter().map(|row| row.id))?;
                progress.embedding_error = None;
            }
            Err(error) => {
                let message = format!("{error:#}");
                if progress.embedding_error.as_ref() != Some(&message) {
                    warn!("Embeddings: {message}");
                }
                progress.embedding_error = Some(message);
            }
        }
        Ok(())
    }
}

fn annotations(runtime: Arc<Runtime>) {
    while !runtime.stopping.load(Ordering::Relaxed) {
        thread::sleep(Duration::from_secs(2));
        let config = runtime.config();
        if !config.models.annotate || runtime.stopping.load(Ordering::Relaxed) {
            continue;
        }
        let result = (|| -> Result<bool> {
            let store = Store::open(&config.data)?;
            let Some(hits) = store.annotation_job()? else {
                return Ok(false);
            };
            let Some(first) = hits.first() else {
                return Ok(false);
            };
            store.defer_annotation(&first.hash)?;
            models::annotate(&runtime, &config, &store, &hits)?;
            Ok(true)
        })();
        let mut progress = runtime.progress.write().unwrap();
        progress.annotation_error = match result {
            Ok(false) => continue,
            Ok(true) => None,
            Err(error) => {
                let message = format!("{error:#}");
                if progress.annotation_error.as_ref() != Some(&message) {
                    warn!("Annotations: {message}");
                }
                Some(message)
            }
        };
    }
}

fn note_scan_error(error: &mut Option<String>, message: String) {
    if error.is_none() {
        warn!("Scan: {message}");
        *error = Some(message);
    }
}

fn visible(data: &Path, root: &Path, path: &Path) -> bool {
    let inbox = data.join("inbox");
    if path.starts_with(data) && !path.starts_with(&inbox) && path != data {
        return false;
    }
    path.strip_prefix(root).is_ok_and(|relative| relative.components().all(|component| {
        matches!(component, Component::Normal(name) if name.to_str().is_some_and(|name| !name.starts_with('.')))
    }))
}

fn is_supported(extension: &str) -> bool {
    matches!(
        extension,
        "txt"
            | "md"
            | "markdown"
            | "csv"
            | "tsv"
            | "json"
            | "log"
            | "rst"
            | "pdf"
            | "docx"
            | "pptx"
            | "xlsx"
            | "xls"
            | "html"
            | "htm"
            | "epub"
            | "jpg"
            | "jpeg"
            | "png"
            | "tiff"
            | "tif"
            | "bmp"
            | "gif"
            | "heic"
            | "webp"
    ) && (cfg!(target_os = "macos") || extension != "heic")
}

enum Outcome {
    Stable,
    Retry,
}

fn archive(
    runtime: &Runtime,
    store: &mut Store,
    config: &Config,
    name: &str,
    root: &Path,
    path: &Path,
    extension: &str,
) -> Result<Outcome> {
    let before = match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_file() => metadata,
        Ok(_) => return Ok(Outcome::Stable),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Outcome::Stable),
        Err(error) => return Err(error.into()),
    };
    let max_bytes = config.schedule.max_file_mib * 1024 * 1024;
    if before.len() > max_bytes {
        return Ok(Outcome::Stable);
    }
    let modified = before.modified()?;
    if let Ok(age) = modified.elapsed()
        && age < Duration::from_secs(config.schedule.settle_seconds)
    {
        return Ok(Outcome::Retry);
    }
    let path_key = fingerprint(&before)?;
    let origin = Origin {
        source: name,
        root,
        path,
        fingerprint: &path_key,
    };
    if store.unchanged(&origin)? {
        return Ok(Outcome::Stable);
    }

    // WalkDir skips symlinks. A native event can still target a symlinked ancestor.
    let canonical = path.canonicalize()?;
    let actual_root = root.canonicalize()?;
    if canonical.strip_prefix(&actual_root).ok() != path.strip_prefix(root).ok() {
        return Ok(Outcome::Stable);
    }
    let actual_data = config.data.canonicalize()?;
    if canonical.starts_with(&actual_data) && !canonical.starts_with(actual_data.join("inbox")) {
        return Ok(Outcome::Stable);
    }
    let mut file = File::open(&canonical)?;
    if path_key != fingerprint(&file.metadata()?)? {
        return Ok(Outcome::Retry);
    }

    let archive_dir = config.data.join("archive");
    let mut staging = NamedTempFile::new_in(&archive_dir)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0; 128 * 1024];
    let mut copied = 0_u64;
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        copied += read as u64;
        if copied > max_bytes || runtime.stopping.load(Ordering::Relaxed) {
            return Ok(Outcome::Retry);
        }
        hasher.update(&buffer[..read]);
        staging.write_all(&buffer[..read])?;
    }
    if copied != before.len()
        || path_key != fingerprint(&file.metadata()?)?
        || path_key != fingerprint(&fs::symlink_metadata(path)?)?
    {
        return Ok(Outcome::Retry);
    }

    let _cloud = runtime.cloud_gate.lock().unwrap();
    if !runtime.config().accepts(name, root, path) {
        return Ok(Outcome::Stable);
    }

    let hash = format!("{:x}", hasher.finalize());
    let title = path
        .file_name()
        .and_then(|name| name.to_str())
        .context("Read filename")?;
    let relative = match store.archive(&hash)? {
        Some((relative, _)) => relative,
        None => {
            let date = Local::now();
            PathBuf::from(name)
                .join(format!("{:04}", date.year()))
                .join(format!("{:02}", date.month()))
                .join(format!("{hash}.{extension}"))
        }
    };
    let destination = archive_dir.join(&relative);
    if !destination.is_file() {
        fs::create_dir_all(
            destination
                .parent()
                .context("Archive destination has no parent")?,
        )?;
        staging.as_file().sync_all()?;
        staging
            .persist_noclobber(&destination)
            .with_context(|| format!("Archive {}", destination.display()))?;
        #[cfg(unix)]
        File::open(destination.parent().unwrap())?.sync_all()?;
    }

    store.record(
        &Archive {
            hash: &hash,
            relative: &relative,
            title,
            source: name,
            size: copied,
        },
        &origin,
    )?;
    Ok(Outcome::Stable)
}

fn fingerprint(metadata: &Metadata) -> Result<String> {
    use std::time::UNIX_EPOCH;
    let modified = metadata.modified()?;
    let modified = match modified.duration_since(UNIX_EPOCH) {
        Ok(duration) => duration.as_nanos() as i128,
        Err(error) => -(error.duration().as_nanos() as i128),
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Ok(format!(
            "{modified}:{}:{}:{}:{}:{}",
            metadata.len(),
            metadata.dev(),
            metadata.ino(),
            metadata.ctime(),
            metadata.ctime_nsec()
        ))
    }
    #[cfg(not(unix))]
    {
        Ok(format!("{modified}:{}", metadata.len()))
    }
}

pub fn initialize_data(config: &Config) -> Result<File> {
    let data = &config.data;
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(data)?;
    }
    #[cfg(not(unix))]
    fs::create_dir_all(data)?;
    ensure!(
        fs::symlink_metadata(data)?.is_dir(),
        "Knowledge base directory must be a real directory"
    );
    let archive = data.join("archive");
    fs::create_dir_all(&archive)?;
    ensure!(
        fs::symlink_metadata(&archive)?.is_dir(),
        "Expected a real archive directory"
    );
    let default_inbox = data.join("inbox");
    if config.inboxes().any(|path| path == default_inbox) {
        fs::create_dir_all(&default_inbox)?;
        ensure!(
            fs::symlink_metadata(&default_inbox)?.is_dir(),
            "Expected a real inbox directory"
        );
    }
    path_string(data)?;
    let lock = File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(data.join("service.lock"))?;
    lock.try_lock()
        .context("A kb service is already using this data directory")?;
    Ok(lock)
}
