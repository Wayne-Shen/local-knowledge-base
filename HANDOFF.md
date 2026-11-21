# Development Handoff

## Current Status

- **Verified 2026-11-21 on macOS arm64.** A fresh source-only copy installed the pinned Python environment with `uv sync --locked --offline`. `cargo build --locked --offline --release` and `cargo test --locked --offline --release` passed using the pinned Rust version and cached dependencies. All six Rust tests passed. Python compilation and worker imports passed. `node --check web/app.js` and `sh -n scripts/launchd.sh` passed.
- **End-to-end smoke passed on macOS.** The freshly synced Python environment ran `tests/smoke.py` against the release binary and Ollama 0.40.17. Synthetic fixtures verified archive intake, deduplication, keyword and vector retrieval, PDF tables and OCR, cited model answers, MCP, cloud source policy, and folder persistence.
- **Verified 2026-11-21 on Debian Bookworm arm64.** In a clean container, `cargo build --locked --release`, `cargo test --locked --release`, `uv sync --locked`, Python compilation, and worker imports passed. The Rust toolchain and locked dependencies were downloaded from their registries.
- **Repository.** The GitHub repository `Wayne-Shen/local-knowledge-base` has private visibility. Use `main` for the baseline handoff. Native Windows, graphical folder-picker interaction, and login-job installers still need direct verification.

## Completed Jobs

- **2026-11-21 · Cross-device setup on `main`.** Audited the application subtree, cleared generated build output, the local Python environment, and bytecode caches. Kept live configuration local. Added ignores for local data and credentials, documented `KB_CONFIG`, and retained the existing OS credential store and optional reranker key file. Kept existing lockfiles and pinned Rust, Python, and uv. Added setup instructions and the persistent handoff protocol.

## Active Context / Gotchas

- Recreate the environment and release binary on each device. Use `config.example.yaml` as the starting point for an ignored `config.yaml`. The default browser and Ollama ports are 8765 and 11434.
- Treat the archive, SQLite catalog, indexes, and cloud audit as local data. Back up data with the service stopped. Configure watched folders and any OS credentials per device. macOS security-scoped folders need to be selected again on a new Mac. Linux lacks the native OCR and credential-store integrations.
- Ollama model digests from the smoke test:
  - `bge-m3:latest` · `7907646426070047a77226ac3e684fbbe8410524f7b4a74d02837e43f2146bab`
  - `qwen3:4b` · `359d7dd4bcdab3d86b87d73ac27966f4dbb9f5efdfcc75d34a8764a09474fae7`
  - Run `kb reindex` after a bge-m3 digest change if reusing an existing catalog.

## Next Steps

1. Verify a fresh Windows MSVC checkout with the locked build, unit tests, and smoke test. Then check Windows OCR and Task Scheduler from a graphical session.
2. Verify the macOS graphical picker and login installer on a second Mac if those features are needed there.
