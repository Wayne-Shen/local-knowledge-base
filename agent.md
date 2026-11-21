# Project Agent Rules

## Start of job

Read [HANDOFF.md](HANDOFF.md) and check `git status --short --branch`. Use the latest shared revision of the task branch when available. Reconcile any existing local work before editing.

## Architecture

| Area | Responsibility |
| --- | --- |
| `src/main.rs`, `src/server.rs`, `src/mcp.rs` | CLI, loopback Axum service, and stdio MCP. The CLI and MCP use the running service for search and answers. |
| `src/config.rs`, `src/collector.rs`, `src/folder_picker*` | YAML settings, native watchers, scan scheduling, SHA-256 archive intake, and platform folder access. |
| `src/store.rs`, `src/search.rs` | SQLite catalog and provenance, Tantivy keyword and phrase search with Jieba, and stable citation IDs. |
| `src/worker.rs`, `worker/` | Rust-to-Python JSON request and response handling, document and PDF processing, native OCR, and LanceDB embeddings. |
| `src/models.rs`, `src/rerank.rs`, `src/markdown.rs` | Local inference, optional reranking, permission checks and audit for DeepSeek, and safe answer rendering. |
| `web/`, `scripts/`, `tests/smoke.py` | Embedded HTML, CSS, and JavaScript, platform login installers, and end-to-end testing with synthetic documents. |

## Coding conventions

- Use the pinned Rust, Python, and uv versions. Keep dependency changes and the affected lockfiles in the same commit. Use `--locked` for normal installs and builds.
- Keep responsibilities in their current modules and follow the surrounding code conventions. Use `cargo fmt` on changed Rust code. Match the existing Python and JavaScript style.
- Build file paths with native path APIs. Resolve relative settings from the config file and preserve existing configuration and catalog compatibility. Keep platform-specific code behind its platform condition.
- Keep the HTTP service and local model endpoints on loopback. Preserve per-source cloud opt-in, rules for duplicate documents, and the attempted-request audit. Use synthetic fixtures for tests.
- Keep live configs, credentials, source paths, archives, logs, and indexes outside Git. Update the example config and README when changing a user-facing setting or command.

## Mandatory End-of-Job Protocol

Follow these steps before finishing every task, including documentation changes and interrupted work.

1. Verify the current tree. Run `uv sync --locked`, `cargo build --locked --release`, and `cargo test --locked --release`. Run the relevant feature checks, including `uv run --locked python tests/smoke.py` for changes to ingest, search, models, or the worker when Ollama and both local models are available. If a check fails, fix and rerun it when possible. Record every failed or blocked check, the reason, and the command to resume.
2. Update [HANDOFF.md](HANDOFF.md). Refresh **Current Status** with the verification date, platform, commands, and outcomes. Add a concise **Completed Jobs** entry with the branch, task, and decisions. Refresh **Active Context / Gotchas** and prioritize **Next Steps**. For unfinished work, include the exact next action and relevant file or command. Keep credentials and personal paths out of the handoff.
3. Keep the handoff in the same change as the work. Check the diff and worktree for accidental files. If the task authorizes a commit or push, include the handoff and verify the shared branch. Record where another agent can find any work that remains local.
