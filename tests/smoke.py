"""End-to-end test using local Ollama models and synthetic documents."""

import json
from pathlib import Path
import shutil
import socket
import sqlite3
import subprocess
import sys
import tempfile
import time
from urllib.error import HTTPError, URLError
from urllib.request import ProxyHandler, Request, build_opener

import pymupdf
from PIL import Image, ImageDraw, ImageFont
from pptx import Presentation
from pptx.util import Inches


PROJECT = Path(__file__).resolve().parents[1]
KB = PROJECT / "target" / "release" / ("kb.exe" if sys.platform == "win32" else "kb")
ZEPHYR_NOTE = "# Zephyr Observatory\nThe Zephyr Observatory access phrase is Cobalt Fern.\n"
OPENER = build_opener(ProxyHandler({}))


def call(base, path, payload=None, *, headers=None):
    request = Request(
        base + path,
        data=json.dumps(payload).encode() if payload is not None else None,
        headers={"Content-Type": "application/json", **(headers or {})},
    )
    with OPENER.open(request, timeout=180) as response:
        data = response.read()
        return json.loads(data) if response.headers.get_content_type() == "application/json" else data


def wait_for(base, condition, process, trace):
    deadline = time.monotonic() + 100
    last = None
    while time.monotonic() < deadline and process.poll() is None:
        try:
            last = call(base, "/api/status")
            if condition(last):
                return last
        except (OSError, URLError):
            pass
        time.sleep(0.6)
    raise AssertionError(f"Indexing timeout. Status: {last}\n{trace.read_text()}")


def make_fixtures(directory):
    public = directory / "public"
    private = directory / "private"
    public.mkdir()
    private.mkdir()
    (public / "observatory.md").write_text(ZEPHYR_NOTE, encoding="utf-8")
    shutil.copyfile(public / "observatory.md", private / "replica.md")
    (public / "中文.md").write_text("本地知识库支持全文检索与中文分词。", encoding="utf-8")
    (public / "ignored.txt").write_text("ZzzIgnoredMarker", encoding="utf-8")
    pptx = Presentation()
    slide = pptx.slides.add_slide(pptx.slide_layouts[6])
    slide.shapes.add_textbox(Inches(0), Inches(0), Inches(8), Inches(1)).text = "Juniper quarterly brief"
    pptx.save(public / "slides.pptx")
    with pymupdf.open() as pdf:
        page = pdf.new_page(width=350, height=260)
        for left, right, title in ((24, 70, "Day"), (74, 181, "Lunch"), (185, 326, "Dinner")):
            page.draw_rect(pymupdf.Rect(left, 24, right, 44), fill=(0.8, 0.8, 0.8), color=None)
            page.insert_text((left + 5, 38), title, fontsize=10)
        for index, (lunch, dinner) in enumerate(
            (("Rivermint soup", "Lentils | Rice"), ("Barley", "Noodles"), ("Beans", "Mushroom rice"))
        ):
            top = 49 + index * 40
            if index % 2 == 0:
                page.draw_rect(pymupdf.Rect(24, top, 326, top + 38), fill=(0.9, 0.9, 0.9), color=None)
            page.insert_text((29, top + 14), str(50 + index), fontsize=10)
            page.insert_text((79, top + 14), lunch, fontsize=10)
            page.insert_text((190, top + 14), dinner, fontsize=10)
        pdf.save(public / "table.pdf")
    if sys.platform in {"darwin", "win32"}:
        image = Image.new("RGB", (1000, 230), "white")
        ImageDraw.Draw(image).text((50, 60), "Meridian Atlas 7201", font=ImageFont.load_default(size=72), fill="black")
        image.save(directory / "ocr.png")
        with pymupdf.open() as pdf:
            page = pdf.new_page(width=550, height=210)
            page.insert_image(pymupdf.Rect(15, 15, 535, 195), filename=directory / "ocr.png")
            pdf.save(public / "scanned.pdf")
    return public


def run_mcp(config, protocol):
    modern = protocol == "2026-07-28"
    intro = {"method": "server/discover"} if modern else {
        "method": "initialize", "params": {"protocolVersion": protocol, "capabilities": {}},
    }
    meta = {"_meta": {"io.modelcontextprotocol/protocolVersion": protocol}} if modern else {}
    frames = [
        {"id": 1, **intro},
        {"method": "notifications/initialized"},
        {"id": 2, "method": "tools/list", "params": meta},
        {"id": 3, "method": "tools/call", "params": {
            **meta, "name": "kb_search", "arguments": {"query": "Zephyr", "keyword_only": True},
        }},
    ]
    response = subprocess.run(
        [str(KB), "--config", str(config), "mcp"],
        input="".join(json.dumps({"jsonrpc": "2.0", **frame}) + "\n" for frame in frames),
        text=True, capture_output=True, timeout=20, check=True,
    )
    first, listed, called = [json.loads(line)["result"] for line in response.stdout.splitlines()]
    if modern:
        assert protocol in first["supportedVersions"] and listed["resultType"] == "complete", first
    else:
        assert first["protocolVersion"] == protocol, first
    assert {tool["name"] for tool in listed["tools"]} == {"kb_search", "kb_ask"}, listed
    hit = called["structuredContent"]["hits"][0]
    assert hit["original_url"].startswith("http://127.0.0.1:") and "Cobalt Fern" in hit["snippet"], called


def stop_service(process):
    if process.poll() is None:
        process.terminate()
        try:
            process.wait(timeout=12)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait()


def run(directory, port):
    public = make_fixtures(directory)
    settings = {
        "data_dir": str(directory / "data"),
        "port": port,
        "worker": {
            "python": {"all": str(Path(sys.executable).absolute())},
            "script": str(PROJECT / "worker" / "worker.py"),
            "ocr_languages": ["en-US", "zh-Hans"],
        },
        "sources": {
            "public": {"preset": "inbox", "paths": {"all": [str(public)]}, "exclude": ["**/ignored.txt"], "allow_cloud": True},
            "private": {"paths": {"all": [str(directory / "private")]}, "allow_cloud": False},
        },
        "schedule": {"scan_seconds": 1800, "settle_seconds": 1, "embedding_seconds": 2, "embedding_batch": 8},
    }
    config = directory / "config.yaml"
    config.write_text(json.dumps(settings))
    trace = directory / "service.log"
    base = f"http://127.0.0.1:{port}"
    supports_ocr = sys.platform in {"darwin", "win32"}
    expected = 5 if supports_ocr else 4
    expected_passages = expected + 3  # One searchable passage for each PDF table row.
    with trace.open("w") as log:
        process = subprocess.Popen([str(KB), "--config", str(config), "serve"], stdout=log, stderr=log)
        try:
            status = wait_for(base, lambda status: status["counts"]["embedded"] == expected_passages, process, trace)
            assert status["counts"]["files"] == expected and not status["errors"], status
            assert status["inbox"] == str(public) and status["data_dir"] == str(directory / "data"), status
            assert status["theme"] == "figma", status
            assert call(base, "/api/theme", {"theme": "miro"}) == {"theme": "miro"}
            assert call(base, "/api/status")["theme"] == "miro"
            try:
                call(base, "/api/theme", {"theme": "invalid"})
            except HTTPError as error:
                assert error.code in (400, 422), error.code
            else:
                raise AssertionError("An unsupported UI theme was accepted")
            try:
                call(base, "/api/theme", {"theme": "figma"}, headers={"Origin": "https://elsewhere.invalid"})
            except HTTPError as error:
                assert error.code == 403
            else:
                raise AssertionError("A cross-origin request changed the UI theme")
            assert not (directory / "data" / "inbox").exists()
            assert len(list((directory / "data" / "archive").glob("*/*/*/*"))) == expected
            archived = call(base, "/api/archive")["files"]
            assert len(archived) == expected, archived
            by_title = {item["title"]: item for item in archived}
            obs = by_title.get("observatory.md") or by_title["replica.md"]
            assert obs["extension"] == "md" and obs["origin_count"] == 2, obs
            assert obs["sources"] == ["private", "public"] and obs["parsed"] is True, obs
            assert obs["passages"] == 1 and obs["embedded"] == 1 and obs["size"] == len(ZEPHYR_NOTE.encode()), obs
            assert by_title["slides.pptx"]["extension"] == "pptx"
            assert by_title["table.pdf"]["extension"] == "pdf" and by_title["table.pdf"]["passages"] == 4
            try:
                call(base, "/api/retry", {"hash": "invalid"})
            except HTTPError as error:
                assert error.code == 400, error.code
            else:
                raise AssertionError("An invalid retry hash was accepted")
            try:
                call(base, "/api/retry", {"hash": "0" * 64})
            except HTTPError as error:
                assert error.code == 404, error.code
            else:
                raise AssertionError("An unknown retry hash was accepted")
            try:
                call(base, "/api/retry", {}, headers={"Origin": "https://elsewhere.invalid"})
            except HTTPError as error:
                assert error.code == 403, error.code
            else:
                raise AssertionError("A cross-origin request triggered retry")
            with sqlite3.connect(directory / "data" / "metadata.sqlite3") as catalog:
                catalog.execute(
                    "INSERT INTO pdf_table_index(hash, error, retry_at) VALUES (?, ?, ?)",
                    (obs["hash"], "Simulated failure", 9999999999),
                )
            failed_status = call(base, "/api/status")
            assert failed_status["counts"]["failed"] == 1 and failed_status["errors"][0]["hash"] == obs["hash"], failed_status
            retried = call(base, "/api/retry", {"hash": obs["hash"]})
            assert retried == {"queued": "retry", "retried": 1, "hash": obs["hash"]}, retried
            cleared_status = call(base, "/api/status")
            assert cleared_status["counts"]["failed"] == 0 and not cleared_status["errors"], cleared_status
            print("Archive, SHA-256 deduplication, document parsing and embeddings passed", flush=True)

            def search(query, keyword_only=True):
                return call(base, "/api/search", {"query": query, "keyword_only": keyword_only})

            original = search("Zephyr")["hits"][0]
            assert call(base, original["original_url"]).decode() == ZEPHYR_NOTE
            for filename in ("observatory.md", "replica.md", "OBSERVATORY"):
                assert search(filename)["hits"][0]["hash"] == original["hash"], filename
            assert search("中文分词")["hits"][0]["title"] == "中文.md"
            pptx_hit = search("Juniper")["hits"][0]
            assert pptx_hit["title"] == "slides.pptx"
            pptx_preview = call(base, pptx_hit["document_preview_url"])
            assert pptx_preview["kind"] == "slides" and pptx_preview["total_slides"] == 1, pptx_preview
            assert "Juniper quarterly brief" in pptx_preview["slides"][0]["title"], pptx_preview
            md_preview = call(base, original["document_preview_url"])
            assert md_preview["kind"] == "markdown" and "<h1>Zephyr Observatory</h1>" in md_preview["html"], md_preview
            assert "<h1>Zephyr Observatory</h1>" in (original.get("snippet_html") or ""), original
            assert search("ZzzIgnoredMarker")["hits"] == []
            semantic = search("observatory entrance secret", keyword_only=False)
            assert semantic["mode"] == "hybrid" and semantic["warning"] is None, semantic
            assert "Cobalt Fern" in semantic["hits"][0]["snippet"], semantic
            match = search("Rivermint")["hits"][0]
            tables = call(base, match["page_preview_url"] + "/tables")
            assert tables == [{
                "header": ["Day", "Lunch", "Dinner"],
                "rows": [["50", "Rivermint soup", "Lentils | Rice"],
                         ["51", "Barley", "Noodles"], ["52", "Beans", "Mushroom rice"]],
            }], tables
            row = next(hit for hit in search("Lentils Rice")["hits"] if hit["locator"] == "Page 1, Table 1, Row 1")
            assert row["page_preview_url"] == match["page_preview_url"], row
            assert row["snippet"] == "Day: 50\nLunch: Rivermint soup\nDinner: Lentils | Rice", row
            if supports_ocr:
                scanned = search("7201")["hits"][0]
                assert scanned["locator"] == "Page 1"
                assert scanned["page_preview_url"] == f'/preview/{scanned["hash"]}/1'
                assert call(base, scanned["page_preview_url"]).startswith(b"\x89PNG\r\n\x1a\n")
                assert call(base, scanned["page_preview_url"] + "/tables") == []
            print("Chinese keyword search, LanceDB retrieval, PDF table coordinates and scanned PDF OCR passed", flush=True)

            answer = call(base, "/api/ask", {
                "question": "What access phrase does Zephyr Observatory use?", "keyword_only": True,
            })
            assert answer["provider"] == "ollama" and "cobalt fern" in answer["text"].lower(), answer
            assert "[1]" in answer["text"] and answer["sources"][0]["original_url"] == original["original_url"], answer
            assert "[1]" in answer["html"] and "<p>" in answer["html"], answer
            run_mcp(config, "2025-11-25")
            run_mcp(config, "2026-07-28")
            print("Qwen3 cited answers and both MCP protocol revisions passed", flush=True)

            (public / "watched.txt").write_text("Nativewatcher greets the stone meadow.", encoding="utf-8")
            wait_for(
                base,
                lambda status: status["counts"]["files"] == expected + 1 and search("Nativewatcher")["hits"],
                process, trace,
            )
            extra = directory / "extra"
            extra.mkdir()
            (extra / "hot.txt").write_text("Hothatch config reload discovery.", encoding="utf-8")
            settings["sources"]["extra"] = {"paths": {"all": [str(extra)]}}
            settings["models"] = {"fallback": {"enabled": True}}
            pending = config.with_suffix(".next")
            pending.write_text(json.dumps(settings))
            pending.replace(config)
            wait_for(base, lambda status: status["cloud_enabled"] and status["counts"]["parsed"] == expected + 2, process, trace)
            wait_for(base, lambda _: search("Hothatch")["hits"], process, trace)
            assert search("Hothatch")["hits"][0]["title"] == "hot.txt"
            try:
                call(base, "/api/ask", {"question": "Zephyr", "cloud": True, "keyword_only": True})
            except HTTPError as error:
                assert "No retrieved passages are eligible" in error.read().decode(), error
            else:
                raise AssertionError("A duplicate from a private source was eligible for DeepSeek")
            with sqlite3.connect(directory / "data" / "metadata.sqlite3") as database:
                assert database.execute("select count(*) from cloud_audit").fetchone() == (0,)
            try:
                call(base, "/api/scan", {}, headers={"Origin": "https://elsewhere.invalid"})
            except HTTPError as error:
                assert error.code == 403
            else:
                raise AssertionError("The local API accepted a cross-origin request")
            try:
                call(base, "/api/open", {"hash": original["hash"]}, headers={"Origin": "https://elsewhere.invalid"})
            except HTTPError as error:
                assert error.code == 403
            else:
                raise AssertionError("The local API accepted a cross-origin open request")
            try:
                call(base, "/api/open", {"hash": "not-a-hash"})
            except HTTPError as error:
                assert error.code == 400
            else:
                raise AssertionError("An invalid hash was accepted by /api/open")
            (extra / "hot.txt").unlink()
            hot_hash = search("Hothatch")["hits"][0]["hash"]
            try:
                call(base, "/api/open", {"hash": hot_hash})
            except HTTPError as error:
                assert error.code == 404
            else:
                raise AssertionError("Missing original file should return 404 from /api/open")
            print("Native watching, hot reload, duplicate cloud policy and origin checks passed", flush=True)
            call(base, "/api/reindex", {})
            wait_for(base, lambda status: status["counts"]["embedded"] < expected_passages + 2, process, trace)
            wait_for(base, lambda status: status["counts"]["embedded"] == expected_passages + 2, process, trace)
            assert search("Zephyr")["hits"][0]["id"] == original["id"]
            print("Full-text and vector rebuild from archived catalog passed", flush=True)
            settings["models"] = {"annotate": True}
            pending.write_text(json.dumps(settings))
            pending.replace(config)
            wait_for(base, lambda _: search("Zephyr")["hits"][0]["summary"], process, trace)
            annotated = search("Zephyr")["hits"][0]
            assert annotated["tags"] and annotated["summary"], annotated
            print("Optional local document summaries and tags passed", flush=True)

            def folders():
                return {folder["path"] for folder in call(base, "/api/folders")}

            watched = directory / "managed"
            watched.mkdir()
            kept = directory / "persisted"
            kept.mkdir()
            config_before = config.read_bytes()
            assert str(extra) in folders()
            call(base, "/api/folders", {"action": "add", "path": str(watched)})
            assert str(watched) in folders()
            nested = watched / "nested"
            nested.mkdir()
            (nested / "discovered.md").write_text("Acornwatch document", encoding="utf-8")
            wait_for(base, lambda _: search("Acornwatch")["hits"], process, trace)
            archived_url = search("Acornwatch")["hits"][0]["original_url"]
            try:
                call(base, "/api/folders", {"action": "add", "path": str(watched)})
            except HTTPError as error:
                assert error.code == 409
            else:
                raise AssertionError("Duplicate watch was accepted")
            try:
                call(base, "/api/folders", {"action": "add", "path": str(kept)},
                     headers={"Origin": "https://elsewhere.invalid"})
            except HTTPError as error:
                assert error.code == 403
            else:
                raise AssertionError("A cross-origin request changed a watch")
            try:
                call(base, "/api/folders/pick", {}, headers={"Origin": "https://elsewhere.invalid"})
            except HTTPError as error:
                assert error.code == 403
            else:
                raise AssertionError("A cross-origin request opened the native picker")

            call(base, "/api/folders", {"action": "remove", "path": str(watched)})
            call(base, "/api/folders", {"action": "remove", "path": str(extra)})
            call(base, "/api/folders", {"action": "add", "path": str(kept)})
            assert str(watched) not in folders() and str(extra) not in folders()
            assert config.read_bytes() == config_before
            assert call(base, archived_url) == b"Acornwatch document"

            (watched / "ignored.md").write_text("DoNotCollect sentinel", encoding="utf-8")
            stop_service(process)
            if sys.platform == "darwin":
                with sqlite3.connect(directory / "data" / "metadata.sqlite3") as catalog:
                    catalog.execute("INSERT INTO folder_bookmarks VALUES (?, ?)", (str(kept), b"invalid"))
                (kept / "unavailable.md").write_text("UnrestoredBookmark sentinel", encoding="utf-8")
            process = subprocess.Popen([str(KB), "--config", str(config), "serve"], stdout=log, stderr=log)
            wait_for(base, lambda state: state["progress"]["last_scan"] is not None, process, trace)
            assert call(base, "/api/status")["theme"] == "miro"
            assert str(kept) in folders() and str(extra) not in folders() and str(watched) not in folders()
            assert search("DoNotCollect")["hits"] == []
            if sys.platform == "darwin":
                saved = next(folder for folder in call(base, "/api/folders") if folder["path"] == str(kept))
                assert saved["permission_error"], saved
                assert search("UnrestoredBookmark")["hits"] == []
                call(base, "/api/folders", {"action": "remove", "path": str(kept)})
                with sqlite3.connect(directory / "data" / "metadata.sqlite3") as catalog:
                    assert catalog.execute("SELECT 1 FROM folder_bookmarks WHERE path = ?", (str(kept),)).fetchone() is None
            assert call(base, archived_url) == b"Acornwatch document"
            call(base, "/api/folders", {"action": "add", "path": str(extra)})
            assert str(extra) in folders()
            print("Web folder add, recursive intake, removal, archive retention and restart persistence passed", flush=True)
        finally:
            stop_service(process)


if __name__ == "__main__":
    with tempfile.TemporaryDirectory(prefix="kb-smoke-") as temporary:
        with socket.socket() as available:
            available.bind(("127.0.0.1", 0))
            port = available.getsockname()[1]
        run(Path(temporary).resolve(), port)
