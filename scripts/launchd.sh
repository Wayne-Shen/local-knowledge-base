#!/bin/sh
set -eu
umask 077

project=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
label=com.local-knowledge-base.kb
domain=gui/$(id -u)
agent="$HOME/Library/LaunchAgents/$label.plist"
app_dir="$HOME/Library/Application Support/local-knowledge-base"
log_dir="$HOME/Library/Logs/LocalKnowledgeBase"
release="$project/target/release/kb"

case "${1:-}" in
  install)
    test -x "$release" || { echo 'Build the release binary first.' >&2; exit 1; }
    test -x "$project/.venv/bin/python" || { echo 'Run uv sync first.' >&2; exit 1; }
    config="${KB_CONFIG:-$app_dir/config.yaml}"
    case "$config" in /*) ;; *) config="$(pwd)/$config";; esac
    if [ -n "${KB_CONFIG:-}" ]; then
      test -f "$config" || { echo "Config not found: $config" >&2; exit 1; }
    fi

    mkdir -p "$HOME/Library/LaunchAgents" "$app_dir/releases" "$log_dir"
    package=$(mktemp -d "$app_dir/releases/kb.XXXXXX")
    staging=$(mktemp -d "$app_dir/.install.XXXXXX")
    activated=
    trap 'rm -rf "$staging"; if [ -z "$activated" ]; then rm -rf "$package"; fi' EXIT
    bundle="$package/Local Knowledge Base.app"
    mkdir -p "$bundle/Contents/MacOS"
    install -m 600 "$project/src/folder_picker/Info.plist" "$bundle/Contents/Info.plist"
    install -m 700 "$release" "$bundle/Contents/MacOS/kb"
    codesign --force --sign - "$bundle"
    ln -s 'Local Knowledge Base.app/Contents/MacOS/kb' "$package/kb"
    mkdir "$package/worker"
    install -m 600 "$project/worker/worker.py" "$project/worker/vision.swift" "$package/worker/"
    ditto --norsrc "$project/.venv" "$package/.venv"
    "$package/.venv/bin/python" -I -c 'import lancedb, markitdown, pymupdf'

    "$package/.venv/bin/python" - "$project" "$app_dir" "$log_dir" "$staging" "$label" "$config" <<'PY'
import plistlib
import sys
from pathlib import Path

project, app, logs, staging = map(Path, sys.argv[1:5])
label, config = sys.argv[5:]
default = app / "config.yaml"
if Path(config) == default and not default.exists():
    sample = (project / "config.example.yaml").read_text()
    with default.open("x") as file:
        file.write(sample.replace("./.venv/", "./current/.venv/").replace("./worker/", "./current/worker/"))
with (staging / "agent.plist").open("wb") as stream:
    plistlib.dump({
        "Label": label,
        "ProgramArguments": [str(app / "current/kb"), "--config", config, "serve"],
        "RunAtLoad": True,
        "KeepAlive": True,
        "ThrottleInterval": 15,
        "StandardOutPath": str(logs / "kb.log"),
        "StandardErrorPath": str(logs / "kb.log"),
    }, stream)
PY
    plutil -lint "$staging/agent.plist"
    previous=$(readlink "$app_dir/current" || true)
    if launchctl print "$domain/$label" >/dev/null 2>&1; then
      launchctl bootout "$domain/$label"
    fi
    ln -s "$package" "$staging/current"
    mv -fh "$staging/current" "$app_dir/current"
    activated=yes
    install -m 600 "$staging/agent.plist" "$agent"
    launchctl enable "$domain/$label"
    # launchd can still be unregistering the old job after bootout returns.
    attempts=0
    until bootstrap_error=$(launchctl bootstrap "$domain" "$agent" 2>&1); do
      attempts=$((attempts + 1))
      if [ "$attempts" -ge 6 ]; then
        printf '%s\n' "$bootstrap_error" >&2
        exit 1
      fi
      sleep 1
    done
    case "$previous" in "$app_dir"/releases/kb.*) rm -rf "$previous";; esac
    echo "Installed $label"
    echo "Config: $config"
    ;;
  status)
    launchctl print "$domain/$label"
    ;;
  remove)
    if launchctl print "$domain/$label" >/dev/null 2>&1; then
      launchctl bootout "$domain/$label"
    fi
    rm -f "$agent"
    ;;
  *)
    echo "Usage: $0 {install|status|remove}" >&2
    exit 2
    ;;
esac
