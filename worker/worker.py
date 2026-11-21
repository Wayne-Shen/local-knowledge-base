"""Short-lived document and vector worker. Each invocation reads one JSON request."""

import asyncio
from contextlib import redirect_stdout
import json
import math
from pathlib import Path
import subprocess
import sys
import tempfile
from urllib.request import HTTPRedirectHandler, ProxyHandler, Request, build_opener
from urllib.parse import urlsplit
from ipaddress import ip_address

TEXT_EXTENSIONS = {".txt", ".md", ".markdown", ".csv", ".tsv", ".json", ".log", ".rst"}
IMAGE_EXTENSIONS = {".jpg", ".jpeg", ".png", ".tiff", ".tif", ".bmp", ".gif", ".heic", ".webp"}
MAX_TEXT_CHARS = 4_000_000
MODEL = "bge-m3:latest"
VECTOR_DIMENSIONS = 1024
TABLE = "chunks"


def read_text(path):
    from charset_normalizer import from_bytes

    data = path.read_bytes()
    for encoding in ("utf-8-sig", "utf-16"):
        if encoding == "utf-16" and not data.startswith((b"\xff\xfe", b"\xfe\xff")):
            continue
        try:
            return data.decode(encoding)
        except UnicodeDecodeError:
            pass
    decoded = from_bytes(data).best()
    if decoded is None:
        raise ValueError("Cannot identify the text encoding")
    return str(decoded)


async def windows_ocr(paths, languages):
    from PIL import Image, ImageOps
    from winrt.windows.globalization import Language
    from winrt.windows.graphics.imaging import BitmapDecoder
    from winrt.windows.media.ocr import OcrEngine
    from winrt.windows.storage import FileAccessMode, StorageFile

    engines = []
    for tag in languages:
        language = Language(tag)
        if OcrEngine.is_language_supported(language):
            engine = OcrEngine.try_create_from_language(language)
            if engine is not None:
                engines.append(engine)
    if not engines:
        engine = OcrEngine.try_create_from_user_profile_languages()
        if engine is None:
            raise RuntimeError("Install a Windows OCR language in Settings")
        engines.append(engine)

    texts = []
    limit = OcrEngine.max_image_dimension
    with tempfile.TemporaryDirectory(prefix="kb-winocr-") as directory:
        output = Path(directory) / "image.png"
        for path in paths:
            with Image.open(path) as image:
                image = ImageOps.exif_transpose(image)
                image.thumbnail((limit, limit))
                image.convert("RGB").save(output)
            file = await StorageFile.get_file_from_path_async(str(output))
            stream = await file.open_async(FileAccessMode.READ)
            try:
                decoder = await BitmapDecoder.create_async(stream)
                bitmap = await decoder.get_software_bitmap_async()
                try:
                    alternatives = [(await engine.recognize_async(bitmap)).text for engine in engines]
                    texts.append(max(alternatives, key=lambda text: sum(c.isalnum() for c in text)))
                finally:
                    bitmap.close()
            finally:
                stream.close()
    return texts


def ocr(paths, languages, timeout, cache):
    if sys.platform == "darwin":
        script = Path(__file__).with_name("vision.swift")
        Path(cache).mkdir(parents=True, exist_ok=True)
        result = subprocess.run(
            ["/usr/bin/swift", "-module-cache-path", cache, str(script), ",".join(languages), *map(str, paths)],
            capture_output=True, timeout=timeout, check=False,
        )
        if result.returncode:
            raise RuntimeError(result.stderr.decode("utf-8", errors="replace").strip())
        return json.loads(result.stdout)
    if sys.platform == "win32":
        return asyncio.run(windows_ocr(paths, languages))
    raise RuntimeError("Image OCR requires macOS Vision or Windows.Media.Ocr")


def pdf_sections(path, languages, timeout, cache):
    import pymupdf

    sections = []
    total_chars = 0
    with tempfile.TemporaryDirectory(prefix="kb-pdf-") as directory:
        images = {}
        with pymupdf.open(path) as document:
            if document.needs_pass:
                raise ValueError("Encrypted PDF requires a decrypted copy")
            for index, page in enumerate(document):
                text = page.get_text(sort=True)
                total_chars += len(text)
                if total_chars > MAX_TEXT_CHARS:
                    raise ValueError("Document exceeds the extracted text limit")
                sections.append({"locator": f"Page {index + 1}", "text": text})
                if sum(c.isalnum() for c in text) < 24 and (not text.strip() or page.get_image_info()):
                    image = Path(directory) / f"{index + 1}.png"
                    scale = min(2.4, 2800 / max(page.rect.width, page.rect.height, 1))
                    page.get_pixmap(matrix=pymupdf.Matrix(scale, scale), alpha=False).save(image)
                    images[index] = image
        if images:
            for index, recognized in zip(images, ocr(list(images.values()), languages, timeout, cache), strict=True):
                original = sections[index]["text"].strip()
                if recognized.strip() and original not in recognized:
                    recognized = "\n".join(filter(None, [original, recognized]))
                sections[index]["text"] = recognized.strip() or original
    return sections


def pdf_page(document, number):
    index = number - 1
    if not document.is_pdf or document.needs_pass or not 0 <= index < len(document):
        raise ValueError("PDF page is unavailable")
    return document[index]


def pdf_preview(request):
    import pymupdf

    with pymupdf.open(request["path"]) as document:
        page = pdf_page(document, request["page"])
        scale = min(2.5, 1600 / max(page.rect.width, page.rect.height, 1))
        return page.get_pixmap(matrix=pymupdf.Matrix(scale, scale), alpha=False).tobytes("png")


def ruled_tables(page):
    with redirect_stdout(sys.stderr):
        found = page.find_tables(strategy="lines_strict", use_layout=False)
    for table in found.tables:
        if table.col_count < 2 or table.row_count < 2:
            continue
        header = [(cell or "").strip() for cell in table.header.names]
        values = table.extract()
        if not table.header.external:
            values = values[1:]
        rows = [[(cell or "").strip() for cell in row] for row in values]
        rows = [row for row in rows if any(row)]
        if rows and len(header) == table.col_count and all(header):
            yield table.bbox, {"header": header, "rows": rows}


def striped_tables(page):
    """Recover tables drawn as header cards above alternating row backgrounds."""
    import pymupdf

    fills = []
    for drawing in page.get_drawings():
        rect, color = drawing["rect"], drawing.get("fill")
        if color is not None and rect.width >= 20 and rect.height >= 8:
            fills.append((rect, color))

    header_bands = {}
    for rect, color in fills:
        header_bands.setdefault((round(rect.y0), round(rect.y1), color), []).append(rect)
    for header_cells in header_bands.values():
        if len(header_cells) < 2:
            continue
        header_cells.sort(key=lambda cell: cell.x0)
        if any(not -1 <= right.x0 - left.x1 <= 10
               for left, right in zip(header_cells, header_cells[1:])):
            continue
        header = [page.get_textbox(cell).strip() for cell in header_cells]
        if not all(header):
            continue

        x0, x1 = header_cells[0].x0, header_cells[-1].x1
        y1 = max(cell.y1 for cell in header_cells)
        following = sorted(
            [(rect, color) for rect, color in fills
             if abs(rect.x0 - x0) <= 2 and abs(rect.x1 - x1) <= 2 and rect.y0 >= y1 - 1],
            key=lambda item: item[0].y0,
        )
        if not following or following[0][0].y0 - y1 > max(12, header_cells[0].height * 0.5):
            continue
        first, stripe_color = following[0]
        height = first.height
        bands = [(first.y0, first.y1)]
        stripes = 1
        for rect, color in following[1:]:
            gap = rect.y0 - bands[-1][1]
            if color != stripe_color or abs(rect.height - height) > height * 0.18:
                break
            if height * 0.55 <= gap <= height * 1.45:
                bands.append((bands[-1][1], rect.y0))
            elif not -1 <= gap <= height * 0.2:
                break
            bands.append((rect.y0, rect.y1))
            stripes += 1
        if stripes < 2:
            continue

        boundaries = [x0, *((left.x1 + right.x0) / 2
                            for left, right in zip(header_cells, header_cells[1:])), x1]
        rows = [
            [page.get_textbox(pymupdf.Rect(left, top, right, bottom)).strip()
             for left, right in zip(boundaries, boundaries[1:])]
            for top, bottom in bands
        ]
        if all(sum(bool(cell) for cell in row) >= 2 for row in rows):
            yield (x0, header_cells[0].y0, x1, bands[-1][1]), {"header": header, "rows": rows}


def page_tables(page):
    import pymupdf

    candidates = list(striped_tables(page))
    for bounds, table in ruled_tables(page):
        if not any(pymupdf.Rect(bounds).intersects(other) for other, _ in candidates):
            candidates.append((bounds, table))
    candidates.sort(key=lambda entry: (entry[0][1], entry[0][0]))
    return [table for _, table in candidates]


def pdf_tables(request):
    import pymupdf

    with pymupdf.open(request["path"]) as document:
        return page_tables(pdf_page(document, request["page"]))


def pdf_rows(request):
    import pymupdf

    sections = []
    total_chars = 0
    with pymupdf.open(request["path"]) as document:
        if document.needs_pass:
            raise ValueError("Encrypted PDF requires a decrypted copy")
        for page_number, page in enumerate(document, 1):
            for table_number, table in enumerate(page_tables(page), 1):
                headers = [" ".join(header.split()) for header in table["header"]]
                for row_number, values in enumerate(table["rows"], 1):
                    if len(values) != len(headers):
                        continue
                    text = "\n".join(
                        f"{header}: {' '.join(value.split())}"
                        for header, value in zip(headers, values, strict=True)
                        if value.strip()
                    )
                    if not text:
                        continue
                    total_chars += len(text)
                    if total_chars > MAX_TEXT_CHARS:
                        raise ValueError("Detected PDF table rows exceed the extracted text limit")
                    sections.append({
                        "locator": f"Page {page_number}, Table {table_number}, Row {row_number}",
                        "text": text,
                    })
    return sections


def parse(request):
    path = Path(request["path"])
    extension = path.suffix.lower()
    if extension in TEXT_EXTENSIONS:
        sections = [{"locator": "Text", "text": read_text(path)}]
    elif extension == ".pdf":
        sections = pdf_sections(path, request["languages"], request["timeout"], request["cache"])
    elif extension in IMAGE_EXTENSIONS:
        sections = [{"locator": "Image OCR", "text": ocr([path], request["languages"], request["timeout"], request["cache"])[0]}]
    else:
        from markitdown import MarkItDown, StreamInfo

        with path.open("rb") as file:
            converted = MarkItDown(enable_plugins=False).convert_stream(
                file, stream_info=StreamInfo(extension=extension)
            )
        sections = [{"locator": "Extracted text", "text": converted.text_content}]
    if sum(len(section["text"]) for section in sections) > MAX_TEXT_CHARS:
        raise ValueError("Document exceeds the extracted text limit")
    return sections


class BlockRedirect(HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        return None


def ollama(url, endpoint, payload=None):
    parsed = urlsplit(url)
    if (
        parsed.scheme not in {"http", "https"}
        or not parsed.hostname
        or not ip_address(parsed.hostname).is_loopback
        or parsed.username is not None
        or parsed.password is not None
        or parsed.path not in {"", "/"}
        or parsed.query
        or parsed.fragment
    ):
        raise ValueError("Ollama must run on loopback")
    request = Request(
        url.rstrip("/") + endpoint,
        data=json.dumps(payload).encode("utf-8") if payload is not None else None,
        headers={"Content-Type": "application/json"},
    )
    with build_opener(ProxyHandler({}), BlockRedirect()).open(request, timeout=100) as response:
        return json.load(response)


def model_digest(url):
    for model in ollama(url, "/api/tags")["models"]:
        if model.get("model", model.get("name")) == MODEL:
            if not model.get("remote_host") and not model.get("remote_model") and model.get("digest"):
                return model["digest"]
            break
    raise RuntimeError("Install the local bge-m3 model in Ollama")


def embeddings(url, texts):
    vectors = ollama(url, "/api/embed", {
        "model": MODEL, "input": texts, "truncate": False, "keep_alive": "2m",
    })["embeddings"]
    if len(vectors) != len(texts) or any(
        len(vector) != VECTOR_DIMENSIONS or not all(math.isfinite(value) for value in vector)
        for vector in vectors
    ):
        raise ValueError("Ollama returned invalid bge-m3 vectors")
    return vectors


def vector_table(request, digest, create):
    import lancedb
    import pyarrow as pa

    directory = Path(request["db"])
    if not create and not (directory / f"{TABLE}.lance").is_dir():
        return None
    database = lancedb.connect(str(directory))
    if (directory / f"{TABLE}.lance").is_dir():
        table = database.open_table(TABLE)
        if (table.schema.metadata or {}).get(b"model_digest") != digest.encode():
            raise ValueError("The bge-m3 model changed. Run kb reindex to rebuild the embeddings.")
        return table
    schema = pa.schema(
        [pa.field("id", pa.int64()), pa.field("vector", pa.list_(pa.float32(), VECTOR_DIMENSIONS))],
        metadata={b"model_digest": digest.encode()},
    )
    return database.create_table(TABLE, schema=schema)


def embed(request):
    url = request["ollama"]
    digest = model_digest(url)
    table = vector_table(request, digest, create=True)
    rows = request["rows"]
    vectors = embeddings(url, [row["text"] for row in rows])
    if model_digest(url) != digest:
        raise ValueError("The bge-m3 model changed during embedding")
    data = [{"id": row["id"], "vector": vector} for row, vector in zip(rows, vectors, strict=True)]
    table.merge_insert("id").when_matched_update_all().when_not_matched_insert_all().execute(data)
    return None


def search(request):
    url = request["ollama"]
    digest = model_digest(url)
    table = vector_table(request, digest, create=False)
    if table is None:
        return []
    vector = embeddings(url, [request["query"]])[0]
    if model_digest(url) != digest:
        raise ValueError("The bge-m3 model changed during search")
    rows = table.search(vector).distance_type("cosine").select(["id"]).limit(request["limit"]).to_list()
    return [row["id"] for row in rows if row["_distance"] <= 0.72]


if __name__ == "__main__":
    try:
        job = json.load(sys.stdin.buffer)
        operations = {"parse": parse, "embed": embed, "search": search, "pdf_preview": pdf_preview,
                      "pdf_tables": pdf_tables, "pdf_rows": pdf_rows}
        result = operations[job["action"]](job)
        if isinstance(result, bytes):
            sys.stdout.buffer.write(result)
        else:
            sys.stdout.buffer.write(json.dumps(result, ensure_ascii=False).encode("utf-8"))
    except Exception as error:
        sys.stderr.buffer.write(f"{type(error).__name__}: {error}\n".encode("utf-8", errors="replace"))
        sys.exit(1)
