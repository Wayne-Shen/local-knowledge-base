use crate::{
    config::Config,
    worker::{EmbeddingJob, Section},
};
use anyhow::{Result, ensure};
use chrono::Utc;
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    time::Duration,
};

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS files (
    hash TEXT PRIMARY KEY,
    archive TEXT NOT NULL,
    title TEXT NOT NULL,
    source TEXT NOT NULL,
    size INTEGER NOT NULL,
    archived_at INTEGER NOT NULL,
    parsed INTEGER NOT NULL DEFAULT 0,
    parse_retry_at INTEGER NOT NULL DEFAULT 0,
    parse_error TEXT,
    summary TEXT,
    tags TEXT,
    annotation_retry_at INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS origins (
    hash TEXT NOT NULL REFERENCES files(hash),
    source TEXT NOT NULL,
    root TEXT NOT NULL,
    path TEXT NOT NULL,
    PRIMARY KEY (hash, source, root, path)
);
CREATE TABLE IF NOT EXISTS observations (
    source TEXT NOT NULL,
    root TEXT NOT NULL,
    path TEXT NOT NULL,
    fingerprint TEXT NOT NULL,
    PRIMARY KEY (source, root, path)
);
CREATE TABLE IF NOT EXISTS chunks (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    hash TEXT NOT NULL REFERENCES files(hash),
    ordinal INTEGER NOT NULL,
    locator TEXT NOT NULL,
    text TEXT NOT NULL,
    indexed INTEGER NOT NULL DEFAULT 0,
    embedded INTEGER NOT NULL DEFAULT 0,
    UNIQUE (hash, ordinal)
);
CREATE INDEX IF NOT EXISTS chunks_pending_index ON chunks(id) WHERE indexed = 0;
CREATE INDEX IF NOT EXISTS chunks_pending_embedding ON chunks(id) WHERE embedded = 0;
CREATE INDEX IF NOT EXISTS files_pending_parse ON files(parse_retry_at) WHERE parsed = 0;
CREATE TABLE IF NOT EXISTS cloud_audit (
    id INTEGER PRIMARY KEY,
    time TEXT NOT NULL,
    purpose TEXT NOT NULL,
    reason TEXT NOT NULL,
    model TEXT NOT NULL,
    chunk_ids TEXT NOT NULL,
    request TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS settings (key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS pdf_table_index (
    hash TEXT PRIMARY KEY REFERENCES files(hash),
    error TEXT,
    retry_at INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS folder_controls (
    path TEXT PRIMARY KEY,
    enabled INTEGER NOT NULL CHECK (enabled IN (0, 1))
);
CREATE TABLE IF NOT EXISTS folder_bookmarks (
    path TEXT PRIMARY KEY,
    bookmark BLOB NOT NULL
);
";

pub struct Store {
    db: Connection,
}

pub struct Archive<'a> {
    pub hash: &'a str,
    pub relative: &'a Path,
    pub title: &'a str,
    pub source: &'a str,
    pub size: u64,
}

pub struct Origin<'a> {
    pub source: &'a str,
    pub root: &'a Path,
    pub path: &'a Path,
    pub fingerprint: &'a str,
}

#[derive(Clone, Debug, Serialize)]
pub struct Hit {
    pub id: i64,
    pub hash: String,
    pub title: String,
    pub extension: String,
    pub source: String,
    pub locator: String,
    pub snippet: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snippet_html: Option<String>,
    pub original_url: String,
    pub page_preview_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image_preview_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub document_preview_url: Option<String>,
    pub summary: Option<String>,
    pub tags: Vec<String>,
}

#[derive(Serialize)]
pub struct Counts {
    pub files: i64,
    pub parsed: i64,
    pub failed: i64,
    pub passages: i64,
    pub embedded: i64,
}

#[derive(Serialize)]
pub struct ParseError {
    pub hash: String,
    pub title: String,
    pub message: String,
}

#[derive(Serialize)]
pub struct ArchivedDocument {
    pub hash: String,
    pub title: String,
    pub extension: String,
    pub source: String,
    pub sources: Vec<String>,
    pub source_labels: Vec<String>,
    pub origin_count: i64,
    pub size: i64,
    pub archived_at: i64,
    pub parsed: bool,
    pub parse_error: Option<String>,
    pub passages: i64,
    pub embedded: i64,
    pub summary: Option<String>,
    pub tags: Vec<String>,
    pub original_url: String,
}

pub struct IndexJob {
    pub id: i64,
    pub filenames: Vec<String>,
    pub text: String,
}

impl Store {
    pub fn open(data: &Path) -> Result<Self> {
        let db = Connection::open(data.join("metadata.sqlite3"))?;
        db.busy_timeout(Duration::from_secs(5))?;
        db.pragma_update(None, "foreign_keys", true)?;
        Ok(Self { db })
    }

    pub fn initialize(&self) -> Result<()> {
        self.db.pragma_update(None, "journal_mode", "WAL")?;
        self.db.execute_batch(SCHEMA)?;
        Ok(())
    }

    pub fn folder_controls(&self) -> Result<BTreeMap<PathBuf, bool>> {
        let mut statement = self
            .db
            .prepare("SELECT path, enabled FROM folder_controls")?;
        let rows = statement.query_map([], |row| {
            Ok((PathBuf::from(row.get::<_, String>(0)?), row.get(1)?))
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn folder_bookmarks(&self) -> Result<BTreeMap<PathBuf, Vec<u8>>> {
        let mut statement = self
            .db
            .prepare("SELECT path, bookmark FROM folder_bookmarks")?;
        let rows = statement.query_map([], |row| {
            Ok((PathBuf::from(row.get::<_, String>(0)?), row.get(1)?))
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn set_folder_control(
        &self,
        path: &Path,
        enabled: Option<bool>,
        bookmark: Option<&[u8]>,
    ) -> Result<()> {
        let path = path_string(path)?;
        let tx = self.db.unchecked_transaction()?;
        if let Some(enabled) = enabled {
            tx.execute(
                "INSERT INTO folder_controls(path, enabled) VALUES (?1, ?2)
                 ON CONFLICT(path) DO UPDATE SET enabled = excluded.enabled",
                params![path, enabled],
            )?;
        } else {
            tx.execute("DELETE FROM folder_controls WHERE path = ?1", [path])?;
        }
        if let Some(bookmark) = bookmark {
            tx.execute(
                "INSERT INTO folder_bookmarks(path, bookmark) VALUES (?1, ?2)
                 ON CONFLICT(path) DO UPDATE SET bookmark = excluded.bookmark",
                params![path, bookmark],
            )?;
        } else {
            tx.execute("DELETE FROM folder_bookmarks WHERE path = ?1", [path])?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn archive(&self, hash: &str) -> Result<Option<(PathBuf, String)>> {
        let record: Option<(String, String)> = self
            .db
            .query_row(
                "SELECT archive, title FROM files WHERE hash = ?1",
                [hash],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        Ok(record.map(|(path, title)| (PathBuf::from(path), title)))
    }

    pub fn resolve_existing_origin(&self, data: &Path, hash: &str) -> Result<Option<PathBuf>> {
        let mut statement = self.db.prepare(
            "SELECT root, path FROM origins WHERE hash = ?1 ORDER BY source, root, path",
        )?;
        let rows = statement.query_map([hash], |row| {
            Ok((
                PathBuf::from(row.get::<_, String>(0)?),
                PathBuf::from(row.get::<_, String>(1)?),
            ))
        })?;
        let actual_data = data.canonicalize()?;
        let actual_inbox = actual_data.join("inbox");
        for row in rows {
            let (root, path) = row?;
            let Ok(metadata) = std::fs::symlink_metadata(&path) else {
                continue;
            };
            if !metadata.is_file() {
                continue;
            }
            let (Ok(canonical), Ok(actual_root)) = (path.canonicalize(), root.canonicalize())
            else {
                continue;
            };
            if canonical.strip_prefix(&actual_root).ok() != path.strip_prefix(&root).ok() {
                continue;
            }
            if canonical.starts_with(&actual_data) && !canonical.starts_with(&actual_inbox) {
                continue;
            }
            let supported = canonical
                .extension()
                .and_then(|ext| ext.to_str())
                .map(str::to_ascii_lowercase)
                .is_some_and(|ext| crate::collector::is_supported(&ext));
            if supported {
                return Ok(Some(canonical));
            }
        }
        Ok(None)
    }

    pub fn unchanged(&self, origin: &Origin<'_>) -> Result<bool> {
        let found: Option<String> = self.db.query_row(
            "SELECT fingerprint FROM observations WHERE source = ?1 AND root = ?2 AND path = ?3",
            params![origin.source, path_string(origin.root)?, path_string(origin.path)?],
            |row| row.get(0),
        ).optional()?;
        Ok(found.as_deref() == Some(origin.fingerprint))
    }

    pub fn record(&mut self, archive: &Archive<'_>, origin: &Origin<'_>) -> Result<()> {
        let root = path_string(origin.root)?;
        let path = path_string(origin.path)?;
        let tx = self.db.transaction()?;
        tx.execute(
            "INSERT OR IGNORE INTO files(hash, archive, title, source, size, archived_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                archive.hash,
                path_string(archive.relative)?,
                archive.title,
                archive.source,
                archive.size,
                Utc::now().timestamp()
            ],
        )?;
        let new_origin = tx.execute(
            "INSERT OR IGNORE INTO origins(hash, source, root, path) VALUES (?1, ?2, ?3, ?4)",
            params![archive.hash, origin.source, root, path],
        )?;
        if new_origin != 0 {
            tx.execute(
                "UPDATE chunks SET indexed = 0 WHERE hash = ?1",
                [archive.hash],
            )?;
        }
        tx.execute(
            "INSERT INTO observations(source, root, path, fingerprint) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(source, root, path) DO UPDATE SET fingerprint = excluded.fingerprint",
            params![origin.source, root, path, origin.fingerprint],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn parse_jobs(&self, limit: usize) -> Result<Vec<(String, PathBuf)>> {
        let mut statement = self.db.prepare(
            "SELECT hash, archive FROM files WHERE parsed = 0 AND parse_retry_at <= ?1
             ORDER BY parse_retry_at, archived_at LIMIT ?2",
        )?;
        let rows = statement.query_map(params![Utc::now().timestamp(), limit], |row| {
            Ok((row.get(0)?, PathBuf::from(row.get::<_, String>(1)?)))
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn parsed(&mut self, hash: &str, sections: Vec<Section>) -> Result<()> {
        let tx = self.db.transaction()?;
        {
            let mut insert = tx.prepare(
                "INSERT INTO chunks(hash, ordinal, locator, text) VALUES (?1, ?2, ?3, ?4)",
            )?;
            let mut ordinal = 0;
            for section in sections {
                for (locator, text) in passages(section) {
                    insert.execute(params![hash, ordinal, locator, text])?;
                    ordinal += 1;
                }
            }
            tx.execute(
                "UPDATE files SET parsed = 1, parse_error = NULL WHERE hash = ?1",
                [hash],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn parse_failed(&self, hash: &str, error: &str) -> Result<()> {
        self.db.execute(
            "UPDATE files SET parse_error = ?2, parse_retry_at = ?3 WHERE hash = ?1",
            params![hash, error, Utc::now().timestamp() + 300],
        )?;
        Ok(())
    }

    pub fn retry_errors(&self) -> Result<()> {
        self.retry_failed(None)?;
        Ok(())
    }

    pub fn retry_failed(&self, hash: Option<&str>) -> Result<Option<usize>> {
        let tx = self.db.unchecked_transaction()?;
        let retried = if let Some(hash) = hash {
            let exists: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM files WHERE hash = ?1)",
                [hash],
                |row| row.get(0),
            )?;
            if !exists {
                return Ok(None);
            }
            let file_changes = tx.execute(
                "UPDATE files SET parse_error = NULL, parse_retry_at = 0
                 WHERE hash = ?1 AND (parsed = 0 OR parse_error IS NOT NULL)",
                [hash],
            )?;
            let table_changes = tx.execute(
                "DELETE FROM pdf_table_index WHERE hash = ?1 AND error IS NOT NULL",
                [hash],
            )?;
            tx.execute(
                "UPDATE files SET annotation_retry_at = 0
                 WHERE hash = ?1 AND parsed = 1 AND summary IS NULL AND annotation_retry_at != 0",
                [hash],
            )?;
            usize::from(file_changes > 0 || table_changes > 0)
        } else {
            let count: i64 = tx.query_row(
                "SELECT count(*) FROM files f
                 LEFT JOIN pdf_table_index t ON t.hash = f.hash
                 WHERE f.parsed = 0 OR f.parse_error IS NOT NULL OR t.error IS NOT NULL",
                [],
                |row| row.get(0),
            )?;
            tx.execute(
                "UPDATE files SET parse_error = NULL, parse_retry_at = 0
                 WHERE parsed = 0 OR parse_error IS NOT NULL",
                [],
            )?;
            tx.execute("DELETE FROM pdf_table_index WHERE error IS NOT NULL", [])?;
            tx.execute(
                "UPDATE files SET annotation_retry_at = 0
                 WHERE parsed = 1 AND summary IS NULL AND annotation_retry_at != 0",
                [],
            )?;
            count.max(0) as usize
        };
        tx.commit()?;
        Ok(Some(retried))
    }

    pub fn pdf_table_jobs(&self, limit: usize) -> Result<Vec<(String, PathBuf)>> {
        let mut statement = self.db.prepare(
            "SELECT f.hash, f.archive FROM files f
             LEFT JOIN pdf_table_index t ON t.hash = f.hash
             WHERE f.parsed = 1 AND lower(f.archive) LIKE '%.pdf'
               AND (t.hash IS NULL OR (t.error IS NOT NULL AND t.retry_at <= ?1))
             ORDER BY coalesce(t.retry_at, 0), f.archived_at, f.hash LIMIT ?2",
        )?;
        let rows = statement.query_map(params![Utc::now().timestamp(), limit], |row| {
            Ok((row.get(0)?, PathBuf::from(row.get::<_, String>(1)?)))
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn store_pdf_rows(&mut self, hash: &str, rows: Vec<Section>) -> Result<()> {
        let tx = self.db.transaction()?;
        {
            let start: i64 = tx.query_row(
                "SELECT coalesce(max(ordinal) + 1, 0) FROM chunks WHERE hash = ?1",
                [hash],
                |row| row.get(0),
            )?;
            let mut insert = tx.prepare(
                "INSERT INTO chunks(hash, ordinal, locator, text) VALUES (?1, ?2, ?3, ?4)",
            )?;
            let sections = rows.into_iter().flat_map(passages);
            for (offset, (locator, text)) in sections.enumerate() {
                insert.execute(params![hash, start + offset as i64, locator, text])?;
            }
            tx.execute(
                "INSERT INTO pdf_table_index(hash) VALUES (?1)
                 ON CONFLICT(hash) DO UPDATE SET error = NULL, retry_at = 0",
                [hash],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn pdf_tables_failed(&self, hash: &str, error: &str) -> Result<()> {
        self.db.execute(
            "INSERT INTO pdf_table_index(hash, error, retry_at) VALUES (?1, ?2, ?3)
             ON CONFLICT(hash) DO UPDATE SET error = excluded.error, retry_at = excluded.retry_at",
            params![hash, error, Utc::now().timestamp() + 300],
        )?;
        Ok(())
    }

    pub fn index_jobs(&self, limit: usize) -> Result<Vec<IndexJob>> {
        let mut statement = self.db.prepare(
            "SELECT c.id, c.hash, f.title, c.text
             FROM chunks c JOIN files f ON f.hash = c.hash
             WHERE c.indexed = 0 ORDER BY c.id LIMIT ?1",
        )?;
        let mut origins = self
            .db
            .prepare("SELECT DISTINCT path FROM origins WHERE hash = ?1")?;
        let mut previous_hash = String::new();
        let mut filenames = Vec::new();
        let mut jobs = Vec::new();
        let mut rows = statement.query([limit])?;
        while let Some(row) = rows.next()? {
            let hash: String = row.get(1)?;
            if hash != previous_hash {
                let title = row.get::<_, String>(2)?;
                let names = origins.query_map([&hash], |row| row.get::<_, String>(0))?;
                let mut extra = std::collections::BTreeSet::new();
                for name in names {
                    let path = name?;
                    if let Some(name) = Path::new(&path).file_name().and_then(|name| name.to_str())
                        && name != title
                    {
                        extra.insert(name.to_owned());
                    }
                }
                filenames = std::iter::once(title).chain(extra).collect();
                previous_hash = hash;
            }
            jobs.push(IndexJob {
                id: row.get(0)?,
                filenames: filenames.clone(),
                text: row.get(3)?,
            });
        }
        Ok(jobs)
    }

    pub fn embedding_jobs(&self, limit: usize) -> Result<Vec<EmbeddingJob>> {
        let mut statement = self
            .db
            .prepare("SELECT id, text FROM chunks WHERE embedded = 0 ORDER BY id LIMIT ?1")?;
        let rows = statement.query_map([limit], |row| {
            Ok(EmbeddingJob {
                id: row.get(0)?,
                text: row.get(1)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn mark_indexed(&mut self, ids: impl IntoIterator<Item = i64>) -> Result<()> {
        self.mark(ids, "UPDATE chunks SET indexed = 1 WHERE id = ?1")
    }

    pub fn mark_embedded(&mut self, ids: impl IntoIterator<Item = i64>) -> Result<()> {
        self.mark(ids, "UPDATE chunks SET embedded = 1 WHERE id = ?1")
    }

    fn mark(&mut self, ids: impl IntoIterator<Item = i64>, sql: &str) -> Result<()> {
        let tx = self.db.transaction()?;
        {
            let mut statement = tx.prepare(sql)?;
            for id in ids {
                statement.execute([id])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn prepare_text_index(&mut self, version: &str, created: bool) -> Result<()> {
        let tx = self.db.transaction()?;
        let stored: Option<String> = tx
            .query_row(
                "SELECT value FROM settings WHERE key = 'text_index_version'",
                [],
                |row| row.get(0),
            )
            .optional()?;
        if created || stored.as_deref() != Some(version) {
            tx.execute("UPDATE chunks SET indexed = 0 WHERE indexed = 1", [])?;
            tx.execute(
                "REPLACE INTO settings VALUES ('text_index_version', ?1)",
                [version],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn request_reindex(&mut self) -> Result<()> {
        let tx = self.db.transaction()?;
        tx.execute("UPDATE chunks SET indexed = 0, embedded = 0", [])?;
        tx.execute("REPLACE INTO settings VALUES ('reset_vectors', 'yes')", [])?;
        tx.commit()?;
        Ok(())
    }

    pub fn reset_vectors_pending(&self) -> Result<bool> {
        Ok(self
            .db
            .query_row(
                "SELECT 1 FROM settings WHERE key = 'reset_vectors'",
                [],
                |_| Ok(true),
            )
            .optional()?
            .unwrap_or(false))
    }

    pub fn finish_vector_reset(&self) -> Result<()> {
        self.db
            .execute("DELETE FROM settings WHERE key = 'reset_vectors'", [])?;
        Ok(())
    }

    pub fn ui_theme(&self) -> Result<String> {
        let theme: Option<String> = self
            .db
            .query_row(
                "SELECT value FROM settings WHERE key = 'ui_theme'",
                [],
                |row| row.get(0),
            )
            .optional()?;
        Ok(match theme.as_deref() {
            Some("miro") => "miro".to_owned(),
            _ => "figma".to_owned(),
        })
    }

    pub fn set_ui_theme(&self, theme: &str) -> Result<()> {
        ensure!(
            matches!(theme, "figma" | "miro"),
            "Unsupported UI theme: {theme}"
        );
        self.db.execute(
            "REPLACE INTO settings(key, value) VALUES ('ui_theme', ?1)",
            [theme],
        )?;
        Ok(())
    }

    pub fn has_vectors(&self) -> Result<bool> {
        Ok(self.db.query_row(
            "SELECT EXISTS(SELECT 1 FROM chunks WHERE embedded = 1)",
            [],
            |row| row.get(0),
        )?)
    }

    pub fn hit(&self, id: i64) -> Result<Option<Hit>> {
        let row = self
            .db
            .query_row(
                "SELECT c.hash, f.title, f.source, c.locator, c.text, f.summary, f.tags, f.archive
             FROM chunks c JOIN files f ON f.hash = c.hash WHERE c.id = ?1",
                [id],
                |row| {
                    let hash: String = row.get(0)?;
                    let tags: Option<String> = row.get(6)?;
                    let locator: String = row.get(3)?;
                    let snippet: String = row.get(4)?;
                    let archive: String = row.get(7)?;
                    let extension = Path::new(&archive)
                        .extension()
                        .and_then(|extension| extension.to_str())
                        .map(str::to_ascii_lowercase)
                        .unwrap_or_default();
                    let page_preview_url = (extension == "pdf")
                        .then(|| {
                            locator
                                .strip_prefix("Page ")
                                .and_then(|locator| locator.split(',').next())
                                .and_then(|number| number.parse::<u32>().ok())
                                .filter(|number| *number > 0)
                                .map(|page| format!("/preview/{hash}/{page}"))
                        })
                        .flatten();
                    let image_preview_url =
                        is_image_extension(&extension).then(|| format!("/preview/{hash}/image"));
                    let document_preview_url = (extension != "pdf"
                        && !is_image_extension(&extension))
                    .then(|| format!("/preview/{hash}/document"));
                    let snippet_html = is_markdown_extension(&extension)
                        .then(|| crate::markdown::render_document(&snippet));
                    Ok(Hit {
                        id,
                        original_url: format!("/original/{hash}"),
                        page_preview_url,
                        image_preview_url,
                        document_preview_url,
                        hash,
                        title: row.get(1)?,
                        extension,
                        source: row.get(2)?,
                        locator,
                        snippet,
                        snippet_html,
                        summary: row.get(5)?,
                        tags: tags
                            .and_then(|value| serde_json::from_str(&value).ok())
                            .unwrap_or_default(),
                    })
                },
            )
            .optional()?;
        Ok(row)
    }

    pub fn document_passages(&self, hash: &str, limit: usize) -> Result<Vec<(String, String)>> {
        let mut statement = self.db.prepare(
            "SELECT locator, text FROM chunks WHERE hash = ?1 ORDER BY ordinal LIMIT ?2",
        )?;
        let rows = statement.query_map(params![hash, limit], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn annotation_job(&self) -> Result<Option<Vec<Hit>>> {
        let hash: Option<String> = self.db.query_row(
            "SELECT hash FROM files WHERE parsed = 1 AND summary IS NULL
                AND annotation_retry_at <= ?1 AND EXISTS (SELECT 1 FROM chunks WHERE chunks.hash = files.hash)
             ORDER BY annotation_retry_at, archived_at LIMIT 1",
            [Utc::now().timestamp()], |row| row.get(0),
        ).optional()?;
        let Some(hash) = hash else { return Ok(None) };
        let mut statement = self
            .db
            .prepare("SELECT id FROM chunks WHERE hash = ?1 ORDER BY ordinal LIMIT 6")?;
        let ids = statement.query_map([hash], |row| row.get::<_, i64>(0))?;
        let mut hits = Vec::new();
        for id in ids {
            if let Some(hit) = self.hit(id?)? {
                hits.push(hit);
            }
        }
        Ok(Some(hits))
    }

    pub fn store_annotation(&self, hash: &str, summary: &str, tags: &[String]) -> Result<()> {
        self.db.execute(
            "UPDATE files SET summary = ?2, tags = ?3 WHERE hash = ?1",
            params![hash, summary, serde_json::to_string(tags)?],
        )?;
        Ok(())
    }

    pub fn defer_annotation(&self, hash: &str) -> Result<()> {
        self.db.execute(
            "UPDATE files SET annotation_retry_at = ?2 WHERE hash = ?1",
            params![hash, Utc::now().timestamp() + 300],
        )?;
        Ok(())
    }

    pub fn cloud_allowed(&self, config: &Config, hash: &str) -> Result<bool> {
        let mut statement = self
            .db
            .prepare("SELECT source, root, path FROM origins WHERE hash = ?1")?;
        let mut rows = statement.query([hash])?;
        let mut seen = false;
        while let Some(row) = rows.next()? {
            seen = true;
            let source: String = row.get(0)?;
            let Some(source) = config.sources.get(&source) else {
                return Ok(false);
            };
            let root = PathBuf::from(row.get::<_, String>(1)?);
            let path = PathBuf::from(row.get::<_, String>(2)?);
            if !source.allow_cloud || !source.roots.contains(&root) || !source.accepts(&root, &path)
            {
                return Ok(false);
            }
        }
        Ok(seen)
    }

    pub fn audit(
        &self,
        purpose: &str,
        reason: &str,
        model: &str,
        ids: &[i64],
        request: &str,
    ) -> Result<()> {
        self.db.execute(
            "INSERT INTO cloud_audit(time, purpose, reason, model, chunk_ids, request)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                Utc::now().to_rfc3339(),
                purpose,
                reason,
                model,
                serde_json::to_string(ids)?,
                request
            ],
        )?;
        Ok(())
    }

    pub fn counts(&self) -> Result<Counts> {
        let (files, parsed, failed) = self.db.query_row(
            "SELECT count(*), count(*) FILTER (WHERE f.parsed = 1),
                    count(*) FILTER (WHERE f.parse_error IS NOT NULL OR t.error IS NOT NULL)
             FROM files f LEFT JOIN pdf_table_index t ON t.hash = f.hash",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        let (passages, embedded) = self.db.query_row(
            "SELECT count(*), count(*) FILTER (WHERE embedded = 1) FROM chunks",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        Ok(Counts {
            files,
            parsed,
            failed,
            passages,
            embedded,
        })
    }

    pub fn errors(&self) -> Result<Vec<ParseError>> {
        let mut statement = self.db.prepare(
            "SELECT hash, title, message FROM (
                SELECT hash, title, parse_error AS message, parse_retry_at AS retry_at
                    FROM files WHERE parse_error IS NOT NULL
                UNION ALL
                SELECT f.hash, f.title, 'PDF table indexing: ' || t.error, t.retry_at
                    FROM pdf_table_index t JOIN files f ON f.hash = t.hash WHERE t.error IS NOT NULL
             ) ORDER BY retry_at DESC LIMIT 8",
        )?;
        let rows = statement.query_map([], |row| {
            Ok(ParseError {
                hash: row.get(0)?,
                title: row.get(1)?,
                message: row.get(2)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn archived_files(&self) -> Result<Vec<ArchivedDocument>> {
        let mut statement = self.db.prepare(
            "SELECT
                f.hash,
                f.archive,
                f.title,
                f.source,
                f.size,
                f.archived_at,
                f.parsed,
                coalesce(
                    f.parse_error,
                    CASE WHEN t.error IS NOT NULL THEN 'PDF table indexing: ' || t.error END
                ),
                f.summary,
                f.tags,
                coalesce(c.passages, 0),
                coalesce(c.embedded, 0),
                coalesce(o.origin_count, 0)
             FROM files f
             LEFT JOIN pdf_table_index t ON t.hash = f.hash
             LEFT JOIN (
                SELECT hash, count(*) AS passages,
                       count(*) FILTER (WHERE embedded = 1) AS embedded
                FROM chunks
                GROUP BY hash
             ) c ON c.hash = f.hash
             LEFT JOIN (
                SELECT hash, count(*) AS origin_count
                FROM origins
                GROUP BY hash
             ) o ON o.hash = f.hash
             ORDER BY f.archived_at DESC, f.title COLLATE NOCASE ASC",
        )?;
        let mut origin_stmt = self
            .db
            .prepare("SELECT DISTINCT source, root FROM origins WHERE hash = ?1")?;
        let mut files = Vec::new();
        let mut rows = statement.query([])?;
        while let Some(row) = rows.next()? {
            let hash: String = row.get(0)?;
            let archive: String = row.get(1)?;
            let title: String = row.get(2)?;
            let source: String = row.get(3)?;
            let parsed: i64 = row.get(6)?;
            let tags: Option<String> = row.get(9)?;
            let mut sources = std::collections::BTreeSet::new();
            let mut source_labels = std::collections::BTreeSet::new();
            let origin_rows = origin_stmt.query_map([&hash], |origin| {
                Ok((origin.get::<_, String>(0)?, origin.get::<_, String>(1)?))
            })?;
            for origin in origin_rows {
                let (origin_source, origin_root) = origin?;
                let label = source_display_label(&origin_source, Some(&origin_root));
                sources.insert(origin_source);
                source_labels.insert(label);
            }
            if sources.is_empty() {
                source_labels.insert(source_display_label(&source, None));
                sources.insert(source.clone());
            }
            let extension = Path::new(&archive)
                .extension()
                .or_else(|| Path::new(&title).extension())
                .and_then(|ext| ext.to_str())
                .map(str::to_ascii_lowercase)
                .unwrap_or_default();
            files.push(ArchivedDocument {
                original_url: format!("/original/{hash}"),
                hash,
                title,
                extension,
                source,
                sources: sources.into_iter().collect(),
                source_labels: source_labels.into_iter().collect(),
                origin_count: row.get(12)?,
                size: row.get(4)?,
                archived_at: row.get(5)?,
                parsed: parsed != 0,
                parse_error: row.get(7)?,
                passages: row.get(10)?,
                embedded: row.get(11)?,
                summary: row.get(8)?,
                tags: tags
                    .and_then(|value| serde_json::from_str(&value).ok())
                    .unwrap_or_default(),
            });
        }
        Ok(files)
    }

    pub fn print_audit(&self, limit: usize) -> Result<()> {
        ensure!(
            (1..=1000).contains(&limit),
            "Audit limit must be between 1 and 1000"
        );
        let mut statement = self.db.prepare(
            "SELECT time, purpose, reason, model, chunk_ids, request FROM cloud_audit ORDER BY id DESC LIMIT ?1",
        )?;
        let mut rows = statement.query([limit])?;
        while let Some(row) = rows.next()? {
            let ids: serde_json::Value = serde_json::from_str(&row.get::<_, String>(4)?)?;
            let request: serde_json::Value = serde_json::from_str(&row.get::<_, String>(5)?)?;
            println!(
                "{}",
                serde_json::json!({
                    "time": row.get::<_, String>(0)?,
                    "purpose": row.get::<_, String>(1)?,
                    "reason": row.get::<_, String>(2)?,
                    "model": row.get::<_, String>(3)?,
                    "chunk_ids": ids,
                    "request": request,
                })
            );
        }
        Ok(())
    }
}

pub fn is_image_extension(extension: &str) -> bool {
    matches!(
        extension,
        "jpg" | "jpeg" | "png" | "tiff" | "tif" | "bmp" | "gif" | "heic" | "webp"
    )
}

fn is_markdown_extension(extension: &str) -> bool {
    matches!(
        extension,
        "md" | "markdown" | "docx" | "pptx" | "xlsx" | "xls" | "html" | "htm" | "epub"
    )
}

pub fn path_string(path: &Path) -> Result<&str> {
    path.to_str()
        .ok_or_else(|| anyhow::anyhow!("Filename must use Unicode: {}", path.display()))
}

fn source_display_label(source: &str, root: Option<&str>) -> String {
    if let Some(digest) = source.strip_prefix("folder-")
        && digest.len() == 64
        && digest.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        if let Some(name) = root
            .and_then(|path| Path::new(path).file_name())
            .and_then(|name| name.to_str())
            .filter(|name| !name.is_empty())
        {
            return name.to_owned();
        }
        return format!("folder-{}", &digest[..8]);
    }
    source.to_owned()
}

fn passages(section: Section) -> Vec<(String, String)> {
    const SIZE: usize = 900;
    const OVERLAP: usize = 120;
    let characters: Vec<char> = section.text.chars().collect();
    let newlines: Vec<usize> = characters
        .iter()
        .enumerate()
        .filter_map(|(i, c)| (*c == '\n').then_some(i))
        .collect();
    let mut passages = Vec::new();
    let mut start = 0;
    while start < characters.len() {
        let mut end = (start + SIZE).min(characters.len());
        if end < characters.len() {
            let tail = &characters[start + SIZE * 3 / 4..end];
            if let Some(index) = tail
                .iter()
                .rposition(|c| c.is_whitespace() || "。！？".contains(*c))
            {
                end = start + SIZE * 3 / 4 + index + 1;
            }
        }
        let text: String = characters[start..end].iter().collect();
        let text = text.trim();
        if !text.is_empty() {
            let first = newlines.partition_point(|i| *i < start) + 1;
            let last = newlines.partition_point(|i| *i < end.saturating_sub(1)) + 1;
            let lines = if first == last {
                format!("line {first}")
            } else {
                format!("lines {first}–{last}")
            };
            let locator = match section.locator.as_str() {
                "Text" => lines,
                _ if characters.len() <= SIZE => section.locator.clone(),
                _ => format!("{}, {}", section.locator, lines),
            };
            passages.push((locator, text.to_owned()));
        }
        if end == characters.len() {
            break;
        }
        start = end - OVERLAP;
    }
    passages
}

#[cfg(test)]
mod tests {
    use super::{Archive, Origin, Store};
    use crate::worker::Section;
    use anyhow::Result;
    use std::path::Path;

    #[test]
    fn stores_and_validates_ui_theme() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let store = Store::open(directory.path())?;
        store.initialize()?;
        assert_eq!(store.ui_theme()?, "figma");
        store.set_ui_theme("miro")?;
        assert_eq!(store.ui_theme()?, "miro");
        store.set_ui_theme("figma")?;
        assert_eq!(store.ui_theme()?, "figma");
        assert!(store.set_ui_theme("unknown").is_err());
        Ok(())
    }

    #[test]
    fn lists_archived_files_with_metadata_and_counts() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let mut store = Store::open(directory.path())?;
        store.initialize()?;
        let hash = "a".repeat(64);
        let archive = Archive {
            hash: &hash,
            relative: Path::new("inbox/2026/11/sample.pdf"),
            title: "Sample Report.pdf",
            source: "inbox",
            size: 2048,
        };
        store.record(
            &archive,
            &Origin {
                source: "inbox",
                root: Path::new("/tmp/inbox"),
                path: Path::new("/tmp/inbox/Sample Report.pdf"),
                fingerprint: "1:2048",
            },
        )?;
        let folder_source = format!("folder-{}", "b".repeat(64));
        store.record(
            &archive,
            &Origin {
                source: &folder_source,
                root: Path::new("/tmp/WeChatFiles"),
                path: Path::new("/tmp/WeChatFiles/Copy.pdf"),
                fingerprint: "2:2048",
            },
        )?;
        store.parsed(
            &hash,
            vec![Section {
                locator: "Page 1".into(),
                text: "Hello archive metadata.".into(),
            }],
        )?;
        store.mark_embedded([1])?;
        store.store_annotation(&hash, "Short summary", &["finance".into(), "q4".into()])?;

        let files = store.archived_files()?;
        assert_eq!(files.len(), 1);
        let doc = &files[0];
        assert_eq!(doc.title, "Sample Report.pdf");
        assert_eq!(doc.extension, "pdf");
        assert_eq!(doc.size, 2048);
        assert_eq!(doc.origin_count, 2);
        assert_eq!(doc.sources, vec![folder_source, "inbox".into()]);
        assert_eq!(doc.source_labels, vec!["WeChatFiles", "inbox"]);
        assert!(doc.parsed);
        assert_eq!(doc.passages, 1);
        assert_eq!(doc.embedded, 1);
        assert_eq!(doc.summary.as_deref(), Some("Short summary"));
        assert_eq!(doc.tags, vec!["finance", "q4"]);
        assert_eq!(doc.original_url, format!("/original/{hash}"));
        Ok(())
    }

    #[test]
    fn resolves_existing_origin_and_skips_missing_or_unsupported_files() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let data = directory.path().join("data");
        let docs = directory.path().join("docs");
        std::fs::create_dir_all(&data)?;
        std::fs::create_dir_all(&docs)?;
        let mut store = Store::open(&data)?;
        store.initialize()?;

        let hash = "c".repeat(64);
        let missing = docs.join("missing.md");
        let existing = docs.join("present.md");
        std::fs::write(&existing, "hello")?;
        let archive = Archive {
            hash: &hash,
            relative: Path::new("docs/2026/11/present.md"),
            title: "present.md",
            source: "docs",
            size: 5,
        };
        store.record(
            &archive,
            &Origin {
                source: "a_missing",
                root: &docs,
                path: &missing,
                fingerprint: "1:5",
            },
        )?;
        store.record(
            &archive,
            &Origin {
                source: "b_present",
                root: &docs,
                path: &existing,
                fingerprint: "2:5",
            },
        )?;
        assert_eq!(
            store.resolve_existing_origin(&data, &hash)?,
            Some(existing.canonicalize()?)
        );

        std::fs::remove_file(&existing)?;
        assert_eq!(store.resolve_existing_origin(&data, &hash)?, None);
        Ok(())
    }

    #[test]
    fn retries_single_and_all_failed_files() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let mut store = Store::open(directory.path())?;
        store.initialize()?;
        let first = "1".repeat(64);
        let second = "2".repeat(64);
        for (hash, title) in [(&first, "first.pdf"), (&second, "second.pdf")] {
            store.record(
                &Archive {
                    hash,
                    relative: Path::new(title),
                    title,
                    source: "inbox",
                    size: 512,
                },
                &Origin {
                    source: "inbox",
                    root: Path::new("/tmp/inbox"),
                    path: &Path::new("/tmp/inbox").join(title),
                    fingerprint: "1:512",
                },
            )?;
            store.parse_failed(hash, "Encrypted PDF requires a decrypted copy")?;
        }
        assert_eq!(store.counts()?.failed, 2);
        assert!(store.parse_jobs(4)?.is_empty());
        assert_eq!(store.retry_failed(Some(&"9".repeat(64)))?, None);

        assert_eq!(store.retry_failed(Some(&first))?, Some(1));
        assert_eq!(store.counts()?.failed, 1);
        let jobs = store.parse_jobs(4)?;
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].0, first);

        store.parse_failed(&first, "Still failing")?;
        assert_eq!(store.counts()?.failed, 2);
        assert_eq!(store.retry_failed(None)?, Some(2));
        assert_eq!(store.counts()?.failed, 0);
        assert_eq!(store.parse_jobs(4)?.len(), 2);
        Ok(())
    }
}
