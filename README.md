# 本地知识库 · Local Knowledge Base

> **English** · [中文说明](README-CN.md)

A native Rust service that watches selected folders, archives originals by SHA‑256, and makes their contents searchable from the browser, command line, and MCP. SQLite stores the catalog, Tantivy with Jieba handles keyword search, and LanceDB stores local bge-m3 embeddings. Qwen3 generates answers with links to the supporting excerpts and archived originals.

## Set up a development checkout

Use a fresh clone on each device. Recreate `.venv/` and `target/` locally. The Git checkout contains the application, example settings, and the development handoff. Restore or back up your documents and indexes separately.

Authenticate GitHub CLI with an account that has access to the private repository, then clone it.

```sh
gh auth login --hostname github.com
gh repo clone Wayne-Shen/local-knowledge-base
cd local-knowledge-base
```

### Prerequisites

- Install Rust with rustup. [`rust-toolchain.toml`](rust-toolchain.toml) selects Rust **1.100.0** with rustfmt and Clippy. [`Cargo.lock`](Cargo.lock) pins Rust dependencies.
- Install **uv 0.10.11**. Standalone uv installs can use `uv self update 0.10.11`. From the checkout, `uv sync --locked` uses Python **3.12.9** from [`.python-version`](.python-version) and installs the packages in [`uv.lock`](uv.lock). It can download the pinned Python if needed.
- On macOS, install the Xcode Command Line Tools with `xcode-select --install` for native compilation and Vision OCR. On Windows, install the Visual Studio C++ Build Tools and Windows SDK, use the Rust MSVC host toolchain, and install the needed OCR languages under **Settings → Time & language → Language & region**. On Linux, install a C/C++ compiler, linker, and `pkg-config`.
- Install and start **Ollama 0.40.17** for vector search and model answers. The build and Rust unit tests run independently of Ollama. The browser UI builds into the Rust binary and has no Node.js dependency.

### Install and start

From the repository root on macOS or Linux:

```sh
uv sync --locked
cargo build --locked --release
test -e config.yaml || cp config.example.yaml config.yaml
./target/release/kb --version
```

On Windows in PowerShell:

```powershell
uv sync --locked
cargo build --locked --release
if (-not (Test-Path config.yaml)) { Copy-Item config.example.yaml config.yaml }
.\target\release\kb.exe --version
```

Start the Ollama app or run `ollama serve` in a separate terminal, then pull the local models:

```sh
ollama pull bge-m3
ollama pull qwen3:4b
```

Run `./target/release/kb serve` on macOS or Linux, or `.\target\release\kb.exe serve` on Windows. If port 8765 is in use, set another port in your local config and open the matching loopback address.

Open the [local knowledge base](http://127.0.0.1:8765). With the example config, copy a file into `data/inbox/` and search for a phrase from it. Keyword search becomes available after extraction. Vector search becomes available as embedding batches finish. The Web interface shows both counts and processing errors (with one-click retry for individual or all failed files), includes an **归档文档** tab to browse archived documents with their metadata organized by file type (click any filename to open the file directly from its original location, reveal its folder, or use **下载原文件** to download the archived copy), and includes a top-bar switcher between **Figma** and **Miro** styles (persisted in the local catalog). Model answers render as Markdown, including tables. PDF citations show the archived page on demand, and other file types (images, Word, Markdown, HTML/EPUB, Excel/CSV/TSV spreadsheets, PowerPoint decks, JSON, and plain text) render with dedicated in-browser previews and copyable Markdown tables across Search, Ask, and Archive views. Open **提取文本** to view detected tables, copy them as Markdown, or read the cited text.

### Local configuration and credentials

Edit your ignored `config.yaml` on each device. Start with the inbox and set any device-specific paths locally. Use an ignored directory such as `data/` for local archives, or place them outside the checkout.

[`.env.example`](.env.example) documents the optional `KB_CONFIG` environment variable. By default the app reads `config.yaml` in the current directory. To load a different file, pass `--config PATH`, or export `KB_CONFIG` using your shell. In PowerShell, use `$env:KB_CONFIG = "./config.yaml"`. If you copy the template to `.env` on macOS or Linux, source it with automatic export enabled.

```sh
set -a
. ./.env
set +a
```

Store optional DeepSeek credentials with `kb set-key` on each supported device. If your local reranker requires a token, save it under the ignored `.secrets/` directory and set `models.reranker.api_key_file` in your local config.

### Run at login

Stop a foreground service before installing a login job. Both jobs run under your account and pick up ongoing changes to `config.yaml`.

| Platform | Install and start | Show status | Uninstall |
| --- | --- | --- | --- |
| macOS | `./scripts/launchd.sh install` | `./scripts/launchd.sh status` | `./scripts/launchd.sh remove` |
| Windows | `.\scripts\scheduled-task.ps1 Install` | `.\scripts\scheduled-task.ps1 Status` | `.\scripts\scheduled-task.ps1 Remove` |

On macOS the installer copies the built binary and worker to `~/Library/Application Support/local-knowledge-base/current/` and creates an inbox-only config at `~/Library/Application Support/local-knowledge-base/config.yaml` on first install. Edit that config for the login service. Its default inbox is `~/Library/Application Support/local-knowledge-base/data/inbox/` and logs are in `~/Library/Logs/LocalKnowledgeBase/kb.log`.

Both installers accept `KB_CONFIG` for a config stored elsewhere. For macOS login jobs, keep the runtime config and data on the startup disk or grant the service macOS access to their volume. The Windows job uses the release binary and worker from this project directory. Removing a login job preserves its knowledge base.

## Search, ask and diagnose

Search prioritizes full filenames, filenames without their extension, and contiguous phrases. Double quotes can boost a phrase within a longer query. PDF tables supported by the preview are indexed row by row alongside page text, with the column headers and a link to the original page. Existing PDFs are backfilled automatically.

Enable optional local reranking with `models.reranker.url` and `models.reranker.api_key_file`. For Qwen3-Reranker, run a local `llama-server` with compatible GGUF weights, `--reranking --pooling rank`, and `--host 127.0.0.1`. The service reorders up to 30 candidates, keeps exact filename and phrase matches ahead of other results, and falls back to retrieval ranking when the reranker is unavailable. Keyword-only mode skips reranking.

```sh
./target/release/kb search "中文分词"
./target/release/kb search --keyword-only "exact phrase"
./target/release/kb ask "这个项目的主要风险是什么？"
./target/release/kb status
./target/release/kb scan
./target/release/kb reindex
```

`search` and `ask` accept `--json`. Use `--config PATH` or set `KB_CONFIG` to run the CLI from another directory. `scan` queues a full scan and retries failed extraction and embedding. `reindex` rebuilds the two search indexes from SQLite passages. It preserves the originals and citation IDs. If the digest behind the Ollama bge-m3 tag changes, run `reindex` before using the new model.

### MCP over stdio

Start the knowledge base service and configure your MCP client with the absolute paths to the release binary and config. The server offers `kb_search` and `kb_ask` over stdio.

```json
{
  "mcpServers": {
    "kb": {
      "command": "/path/to/local-knowledge-base/target/release/kb",
      "args": ["--config", "/path/to/local-knowledge-base/config.yaml", "mcp"]
    }
  }
}
```

## Configure sources

On macOS, open **管理文件夹 → 在 Mac 上选择文件夹** and choose the exact folder in the system picker. For WeChat, select the attachments subfolder you want monitored. The service saves a read-only bookmark for the chosen folder and stops scanning if macOS can no longer restore it. Use the picker again to renew access. The picker requires a graphical login session.

Run with the example config to start with only the inbox. Choose **管理文件夹** in the web app to add or remove a watched folder. Enter its absolute path or a path beginning with `~/`. Changes take effect without a restart and persist in the knowledge base catalog. Removing a watch keeps the files already archived. You can also define sources, presets, and filters in `config.yaml`. Paths there are relative to the config file unless absolute. `~` expands to your home folder. `paths.macos`, `paths.windows`, and `paths.linux` override `paths.all` on their respective platforms.

- `inbox` defaults to `data_dir/inbox/`. To use a separate existing folder, set `paths.all` or a platform-specific path on the inbox source. Set `data_dir` to the folder that should contain the archive and indexes.
- `wechat` looks under the macOS WeChat container or Windows `Documents\xwechat_files`. If WeChat stores attachments elsewhere, set that platform's path to the attachment directory. On macOS, use the system picker for a protected attachments folder.
- `feishu` defaults to the system Downloads folder's `Feishu` subdirectory. Use a platform override for the download location selected in Feishu.
- Custom directories are scanned recursively. Use `include` and `exclude` globs relative to each root, for example `"**/*.pdf"` and `"**/drafts/**"`. The collector skips hidden entries, nested symlinks, and unsupported formats.

Native filesystem notifications pick up changes, and periodic full scans reconcile missed events. Files should settle before archiving. Changed files produce new archived versions, and removing a source file leaves its archive searchable. Relative source and worker paths, model settings, and schedules reload from the file. Changing `data_dir` or `port` requires restarting the service.

The worker handles plain text, Markdown, CSV, TSV, JSON, logs, reStructuredText, HTML, EPUB, Word `.docx`, Excel `.xlsx` and `.xls`, PowerPoint `.pptx`, and PDFs. Apple Vision and Windows.Media.Ocr handle images and scanned PDF pages. Scanned PDFs and images require one of those two platforms. The status pane reports files that fail to extract.

## DeepSeek and local audit

The default setup keeps all model requests on loopback. To enable DeepSeek, run `kb set-key` with the same config as the service, enable `models.fallback.enabled`, and set `allow_cloud: true` on each source you want to allow. The credential is saved in the macOS Keychain or Windows Credential Manager under the configured account.

Questions and optional summaries normally use the local Ollama model. DeepSeek is considered when the local model fails, the retrieved excerpts exceed the local context, or `kb ask --cloud` explicitly requests it. Each archived file's known origins must still permit cloud use. A duplicate from a local-only source keeps the archived file local. Cloud requests include the question or summary instruction and the eligible numbered excerpts. The local audit stores each attempted request body, the routing reason, and the excerpt IDs before transmission. Review it with `kb audit --limit 20`. Set `models.annotate: true` to compute document summaries and tags in the background.

## Data layout and verification

```text
data/
  inbox/                 Manual intake
  archive/<source>/<year>/<month>/<sha256>.<ext>
  metadata.sqlite3       Archive catalog, passages, provenance, annotations and cloud audit
  tantivy-v2/            Keyword and exact phrase index
  lance/                 Vector index
```

Back up the data directory with the service stopped. It contains original documents and extracted text. The Web server listens only on IPv4 loopback and is intended for local clients on the same computer.

Run these checks from the repository root after an install. The smoke test uses the release binary, Ollama on `127.0.0.1:11434`, and both local models. It starts a temporary knowledge base, generates synthetic fixtures, and verifies ingest, retrieval, cited answers, and source policy.

```sh
cargo build --locked --release
cargo test --locked --release
uv run --locked python -m compileall -q worker tests
uv run --locked python tests/smoke.py
```

Ollama model tags can change. See [`HANDOFF.md`](HANDOFF.md) for the model digests used in verification. When moving an existing index to a new device, match the bge-m3 digest or run `kb reindex` after restoring the catalog and archive.

## Pick up work on another device

Pull the branch that contains your last handoff. Read [`agent.md`](agent.md) for project rules and [`HANDOFF.md`](HANDOFF.md) for verification results, open work, and the next action. Keep source changes and their handoff update together in Git.
