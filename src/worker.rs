use crate::config::Config;
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::{
    fs::File,
    io::{Read, Seek, Write},
    path::Path,
    process::{Command, Stdio},
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};
use wait_timeout::ChildExt;

static STOPPING: AtomicBool = AtomicBool::new(false);

pub fn stop() {
    STOPPING.store(true, Ordering::Relaxed);
}

#[derive(Debug, Deserialize)]
pub struct Section {
    pub locator: String,
    pub text: String,
}

#[derive(Debug, Serialize)]
pub struct EmbeddingJob {
    pub id: i64,
    pub text: String,
}

#[derive(Serialize)]
#[serde(tag = "action", rename_all = "snake_case")]
enum Request<'a> {
    Parse {
        path: &'a Path,
        languages: &'a [String],
        timeout: u64,
        cache: &'a Path,
    },
    PdfPreview {
        path: &'a Path,
        page: u32,
    },
    PdfTables {
        path: &'a Path,
        page: u32,
    },
    PdfRows {
        path: &'a Path,
    },
    Embed {
        db: &'a Path,
        ollama: &'a str,
        rows: &'a [EmbeddingJob],
    },
    Search {
        db: &'a Path,
        ollama: &'a str,
        query: &'a str,
        limit: usize,
    },
}

pub fn parse(config: &Config, path: &Path) -> Result<Vec<Section>> {
    run(
        config,
        Request::Parse {
            path,
            languages: &config.worker.ocr_languages,
            timeout: config.worker.timeout_seconds.saturating_sub(2).max(1),
            cache: &config.data.join("ocr-cache"),
        },
    )
}

pub fn embed(config: &Config, rows: &[EmbeddingJob]) -> Result<()> {
    run(
        config,
        Request::Embed {
            db: &config.data.join("lance"),
            ollama: &config.models.primary.url,
            rows,
        },
    )
}

pub fn search(config: &Config, query: &str, limit: usize) -> Result<Vec<i64>> {
    run(
        config,
        Request::Search {
            db: &config.data.join("lance"),
            ollama: &config.models.primary.url,
            query,
            limit,
        },
    )
}

#[derive(Deserialize, Serialize)]
pub struct PdfTable {
    header: Vec<String>,
    rows: Vec<Vec<String>>,
}

pub fn pdf_tables(config: &Config, path: &Path, page: u32) -> Result<Vec<PdfTable>> {
    run(config, Request::PdfTables { path, page })
}

pub fn pdf_rows(config: &Config, path: &Path) -> Result<Vec<Section>> {
    run(config, Request::PdfRows { path })
}

pub fn pdf_preview(config: &Config, path: &Path, page: u32) -> Result<Vec<u8>> {
    let mut output = execute(config, Request::PdfPreview { path, page })?;
    ensure!(
        output.metadata()?.len() <= 8 * 1024 * 1024,
        "PDF preview exceeds the output limit"
    );
    let mut png = Vec::new();
    output.read_to_end(&mut png)?;
    ensure!(png.starts_with(b"\x89PNG\r\n\x1a\n"), "Invalid PDF preview");
    Ok(png)
}

fn run<T: DeserializeOwned>(config: &Config, request: Request<'_>) -> Result<T> {
    let output = execute(config, request)?;
    serde_json::from_reader(output.take(32 * 1024 * 1024))
        .context("Read processing worker response")
}

fn execute(config: &Config, request: Request<'_>) -> Result<File> {
    let mut input = tempfile::tempfile()?;
    serde_json::to_writer(&input, &request)?;
    input.flush()?;
    input.rewind()?;
    let mut output = tempfile::tempfile()?;
    let mut errors = tempfile::tempfile()?;
    let mut child = Command::new(&config.worker.python)
        .arg("-I")
        .arg(&config.worker.script)
        .env("HF_HUB_OFFLINE", "1")
        .env("TRANSFORMERS_OFFLINE", "1")
        .env("RAYON_NUM_THREADS", "2")
        .stdin(Stdio::from(input))
        .stdout(Stdio::from(output.try_clone()?))
        .stderr(Stdio::from(errors.try_clone()?))
        .spawn()
        .with_context(|| format!("Start worker with {}", config.worker.python.display()))?;
    let deadline = Instant::now() + Duration::from_secs(config.worker.timeout_seconds);
    let status = loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() || STOPPING.load(Ordering::Relaxed) {
            let _ = child.kill();
            let _ = child.wait();
            bail!("Processing worker stopped or timed out");
        }
        if let Some(status) = child.wait_timeout(remaining.min(Duration::from_millis(500)))? {
            break status;
        }
    };
    if !status.success() {
        let mut message = Vec::new();
        errors.rewind()?;
        errors.take(2048).read_to_end(&mut message)?;
        bail!(
            "Processing worker failed: {}",
            String::from_utf8_lossy(&message).trim()
        );
    }
    output.rewind()?;
    Ok(output)
}
