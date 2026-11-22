"use strict";

const byId = (id) => document.getElementById(id);
const query = byId("query");
const form = byId("query-form");
const submit = byId("submit");
const output = byId("output");
const feedback = byId("feedback");
const connection = byId("connection");
const scan = byId("scan");
const themeButtons = [byId("theme-figma"), byId("theme-miro")];

const THEME_KEY = "kb-theme";
let currentTheme = "figma";
let themeMutation = 0;
let themeSyncing = 0;

function normalizeTheme(value) {
  return value === "miro" ? "miro" : "figma";
}

function applyTheme(value, persistLocal = true) {
  const next = normalizeTheme(value);
  currentTheme = next;
  document.documentElement.dataset.theme = next;
  for (const button of themeButtons) {
    if (!button) continue;
    button.setAttribute("aria-pressed", String(button.dataset.themeOption === next));
  }
  if (persistLocal) {
    try {
      localStorage.setItem(THEME_KEY, next);
    } catch {}
  }
}

try {
  applyTheme(localStorage.getItem(THEME_KEY), false);
} catch {
  applyTheme("figma", false);
}

async function setTheme(value) {
  const next = normalizeTheme(value);
  if (next === currentTheme) return;
  const sequence = ++themeMutation;
  themeSyncing += 1;
  applyTheme(next);
  try {
    const saved = await api("theme", { theme: next });
    if (sequence === themeMutation && saved?.theme) {
      applyTheme(saved.theme);
    }
  } catch (error) {
    if (sequence === themeMutation) {
      message("界面风格仅在当前浏览器生效：" + error.message, true);
    }
  } finally {
    themeSyncing -= 1;
  }
}

for (const button of themeButtons) {
  if (!button) continue;
  button.addEventListener("click", () => setTheme(button.dataset.themeOption));
}

let mode = "search";
let cloudEnabled = false;
let activeRequest = null;
let archiveFiles = [];
let archiveLoaded = false;
let archiveLoading = false;
let archiveReloadQueued = false;
let archiveTypeFilter = "all";
let archiveStatusSignature = null;
let retryingAll = false;
let lastFailedCount = 0;
const retryingHashes = new Set();

const archiveControls = byId("archive-controls");
const archiveFilter = byId("archive-filter");
const archiveTypes = byId("archive-types");
const archiveRefresh = byId("archive-refresh");
const archiveRetryFailed = byId("archive-retry-failed");
const retryFailedButton = byId("retry-failed");

const FILE_TYPE_ORDER = ["pdf", "markdown", "word", "excel", "powerpoint", "text", "data", "web", "image", "other"];

function classifyFileType(extension) {
  const ext = (extension || "").toLowerCase();
  if (ext === "pdf") return { id: "pdf", label: "PDF 文档" };
  if (ext === "md" || ext === "markdown") return { id: "markdown", label: "Markdown" };
  if (ext === "docx") return { id: "word", label: "Word 文档" };
  if (ext === "xlsx" || ext === "xls") return { id: "excel", label: "Excel 表格" };
  if (ext === "pptx") return { id: "powerpoint", label: "PowerPoint 演示文稿" };
  if (ext === "txt" || ext === "log" || ext === "rst") return { id: "text", label: "纯文本" };
  if (ext === "csv" || ext === "tsv" || ext === "json") return { id: "data", label: "结构化数据" };
  if (ext === "html" || ext === "htm" || ext === "epub") return { id: "web", label: "网页与电子书" };
  if (["jpg", "jpeg", "png", "tiff", "tif", "bmp", "gif", "heic", "webp"].includes(ext)) {
    return { id: "image", label: "图片 (OCR)" };
  }
  return { id: "other", label: "其他文件" };
}

function formatBytes(bytes) {
  if (!Number.isFinite(bytes) || bytes < 0) return "—";
  if (bytes < 1024) return bytes + " B";
  const units = ["KB", "MB", "GB", "TB"];
  let value = bytes / 1024;
  let index = 0;
  while (value >= 1024 && index < units.length - 1) {
    value /= 1024;
    index += 1;
  }
  return (value >= 10 ? value.toFixed(1) : value.toFixed(2)) + " " + units[index];
}

function formatTimestamp(seconds) {
  if (!seconds) return "—";
  return new Date(seconds * 1000).toLocaleString("zh-CN");
}

function docStatus(doc) {
  if (doc.parse_error) return { label: "解析失败", state: "error" };
  if (!doc.parsed) return { label: "待解析", state: "pending" };
  if (doc.passages > 0 && doc.embedded >= doc.passages) return { label: "已向量化", state: "ready" };
  return { label: "已提取", state: "partial" };
}

function element(tag, text, className) {
  const node = document.createElement(tag);
  if (text) node.textContent = text;
  if (className) node.className = className;
  return node;
}

function message(text, error = false) {
  feedback.textContent = text;
  feedback.dataset.error = String(error);
  feedback.hidden = !text;
}

function updateCloudOption() {
  byId("cloud-option").hidden = mode !== "ask" || !cloudEnabled;
  if (byId("cloud-option").hidden) byId("cloud").checked = false;
}

function setMode(next) {
  if (mode === next) return;
  activeRequest?.abort();
  mode = next;
  const asking = mode === "ask";
  const archiving = mode === "archive";
  byId("mode-search").setAttribute("aria-pressed", String(mode === "search"));
  byId("mode-ask").setAttribute("aria-pressed", String(asking));
  byId("mode-archive").setAttribute("aria-pressed", String(archiving));
  byId("workspace-title").textContent = archiving ? "归档文档" : asking ? "向知识库提问" : "搜索你的资料";
  form.hidden = archiving;
  archiveControls.hidden = !archiving;
  output.replaceChildren();
  message("");
  updateCloudOption();
  if (archiving) {
    if (archiveLoaded) renderArchive();
    loadArchive();
    archiveFilter.focus();
    return;
  }
  byId("query-label").textContent = asking ? "问题" : "搜索词";
  query.placeholder = asking ? "输入你想了解的问题" : "输入关键词或短语";
  submit.textContent = asking ? "提问" : "搜索";
  submit.disabled = false;
  query.focus();
}

async function api(path, data, signal) {
  const options = { signal };
  if (data !== undefined) {
    options.method = "POST";
    options.headers = { "Content-Type": "application/json" };
    options.body = JSON.stringify(data);
  }
  const response = await fetch("/api/" + path, options);
  if (!response.headers.get("content-type")?.includes("application/json")) {
    throw new Error("服务响应异常（" + response.status + "）");
  }
  const result = await response.json();
  if (!response.ok) throw new Error(result.error || "请求失败");
  return result;
}

function originalLink(hit, text = hit.title) {
  const link = element("a", text);
  if (/^\/original\/[a-f0-9]{64}$/.test(hit.original_url)) link.href = hit.original_url;
  return link;
}

function openOriginalButton(item, text = item.title, statusNode = null, reveal = false, className = "open-file-link") {
  const button = element("button", text, className);
  button.type = "button";
  button.title = reveal ? "在系统中显示原文件所在位置" : "从原位置直接打开文件";
  button.addEventListener("click", async () => {
    if (!/^[a-f0-9]{64}$/.test(item.hash || "")) return;
    button.disabled = true;
    if (statusNode) {
      statusNode.textContent = reveal ? "正在打开所在位置…" : "正在打开原文件…";
      statusNode.dataset.error = "false";
    }
    try {
      await api("open", { hash: item.hash, reveal });
      if (statusNode) {
        statusNode.textContent = reveal ? "已打开所在位置" : "已打开原文件";
        statusNode.dataset.error = "false";
        setTimeout(() => {
          if (statusNode.textContent === "已打开所在位置" || statusNode.textContent === "已打开原文件") {
            statusNode.textContent = "";
          }
        }, 3000);
      }
    } catch (error) {
      if (statusNode) {
        statusNode.textContent = error.message;
        statusNode.dataset.error = "true";
      } else {
        message(error.message, true);
      }
    } finally {
      button.disabled = false;
    }
  });
  return button;
}

const IMAGE_EXTENSIONS = new Set(["jpg", "jpeg", "png", "tiff", "tif", "bmp", "gif", "heic", "webp"]);

function isImageExtension(ext) {
  return IMAGE_EXTENSIONS.has((ext || "").toLowerCase());
}

function copyActionControl(label, getText) {
  const wrap = element("div", "", "preview-copy-wrap");
  const bar = element("div", "", "pdf-table-actions");
  const copy = element("button", label, "secondary");
  copy.type = "button";
  const status = element("span", "", "pdf-table-status");
  status.setAttribute("role", "status");
  const manual = element("textarea", "", "markdown-copy");
  manual.readOnly = true;
  manual.hidden = true;
  manual.rows = 5;
  manual.setAttribute("aria-label", label);
  copy.addEventListener("click", async () => {
    const text = getText();
    try {
      await navigator.clipboard.writeText(text);
      manual.hidden = true;
      status.textContent = "已复制";
    } catch {
      manual.value = text;
      manual.hidden = false;
      manual.focus();
      manual.select();
      status.textContent = "已选中内容，可手动复制。";
    }
  });
  bar.append(copy, status);
  wrap.append(bar, manual);
  return wrap;
}

function tableToData(table) {
  const headerCells = [...table.querySelectorAll("thead tr:first-child th, thead tr:first-child td")];
  let header = headerCells.map((cell) => cell.textContent.trim());
  const bodyRows = [...table.querySelectorAll("tbody tr")];
  let rows = bodyRows.map((tr) => [...tr.children].map((cell) => cell.textContent.trim()));
  if (!header.length) {
    const allRows = [...table.querySelectorAll("tr")];
    if (allRows.length) {
      header = [...allRows[0].children].map((cell) => cell.textContent.trim());
      rows = allRows.slice(1).map((tr) => [...tr.children].map((cell) => cell.textContent.trim()));
    }
  }
  return { header, rows };
}

function enhanceRenderedTables(container) {
  for (const table of container.querySelectorAll("table")) {
    const section = element("div", "", "pdf-table");
    const data = tableToData(table);
    const scroller = element("div", "", "answer-table");
    table.replaceWith(section);
    scroller.append(table);
    if (data.header.length) {
      section.append(copyActionControl("复制 Markdown", () => markdownTable(data)));
    }
    section.append(scroller);
  }
}

function renderSnippetView(hit) {
  if (typeof hit.snippet_html === "string" && hit.snippet_html.trim()) {
    const sheet = element("div", "", "doc-excerpt-sheet answer-content");
    // Server renders Markdown with raw HTML escaped and arbitrary links/images stripped.
    sheet.innerHTML = hit.snippet_html;
    enhanceRenderedTables(sheet);
    return sheet;
  }
  const sheet = element("div", "", "doc-excerpt-sheet doc-excerpt-plain");
  sheet.append(element("p", hit.snippet, "hit-snippet"));
  return sheet;
}

function renderSpreadsheetPreview(data) {
  const viewer = element("div", "", "sheet-viewer");
  const sheets = data.sheets || [];
  if (!sheets.length) {
    viewer.append(element("p", "未在表格文件中检测到可显示的数据行。", "pdf-table-status"));
    return viewer;
  }
  let activeIndex = 0;
  const tabBar = element("div", "", "sheet-tabs");
  tabBar.setAttribute("role", "tablist");
  const body = element("div", "", "sheet-panel");

  const renderActiveSheet = () => {
    const sheet = sheets[activeIndex];
    for (const [index, btn] of [...tabBar.children].entries()) {
      btn.setAttribute("aria-selected", String(index === activeIndex));
    }
    body.replaceChildren();
    const meta = element("div", "", "sheet-meta-bar");
    const info = element(
      "span",
      sheet.name + " · " + sheet.total_rows + " 行 · " + sheet.header.length + " 列"
        + (sheet.truncated ? "（已截断）" : ""),
      "sheet-meta-info",
    );
    meta.append(info);
    body.append(meta, renderPdfTable({ header: sheet.header, rows: sheet.rows }));
  };

  if (sheets.length > 1) {
    sheets.forEach((sheet, index) => {
      const tab = element("button", sheet.name, "sheet-tab");
      tab.type = "button";
      tab.setAttribute("role", "tab");
      tab.setAttribute("aria-selected", String(index === 0));
      tab.addEventListener("click", () => {
        if (activeIndex === index) return;
        activeIndex = index;
        renderActiveSheet();
      });
      tabBar.append(tab);
    });
    viewer.append(tabBar);
  }
  viewer.append(body);
  renderActiveSheet();
  return viewer;
}

function renderSlidesPreview(data) {
  const wrap = element("div", "", "slide-deck-wrap");
  const slides = data.slides || [];
  if (!slides.length) {
    wrap.append(element("p", "未在演示文稿中检测到可显示的幻灯片。", "pdf-table-status"));
    return wrap;
  }
  const total = data.total_slides || slides.length;
  wrap.append(element("p", "共 " + total + " 页幻灯片" + (data.truncated ? "（显示前 " + slides.length + " 页）" : ""), "sheet-meta-info"));
  const deck = element("div", "", "slide-deck");
  for (const slide of slides) {
    const card = element("article", "", "slide-card");
    const top = element("div", "", "slide-card-header");
    top.append(
      element("span", "Slide " + slide.number + " / " + total, "slide-number"),
      element("h4", slide.title, "slide-title"),
    );
    card.append(top);
    if (slide.blocks?.length) {
      const list = element("ul", "", "slide-bullets");
      for (const block of slide.blocks) {
        const li = element("li", block.text);
        if (block.level > 0) li.dataset.level = String(Math.min(block.level, 3));
        list.append(li);
      }
      card.append(list);
    }
    if (slide.tables?.length) {
      for (const table of slide.tables) card.append(renderPdfTable(table));
    }
    deck.append(card);
  }
  wrap.append(deck);
  return wrap;
}

function renderDocumentPreviewBody(data) {
  const container = element("div", "", "doc-preview-body");
  if (data.kind === "image" && /^\/preview\/[a-f0-9]{64}\/image$/.test(data.image_url || "")) {
    const img = element("img", "", "source-page");
    img.src = data.image_url;
    img.alt = "图片预览 · " + (data.title || "");
    img.decoding = "async";
    container.append(img);
    if (data.passages?.length) {
      const ocrDetails = element("details", "", "source-text");
      ocrDetails.append(element("summary", "提取文本 (OCR)"));
      for (const passage of data.passages) {
        ocrDetails.append(element("p", passage.text, "hit-snippet"));
      }
      container.append(ocrDetails);
    }
  } else if (data.kind === "spreadsheet") {
    container.append(renderSpreadsheetPreview(data));
  } else if (data.kind === "slides") {
    container.append(renderSlidesPreview(data));
  } else if (data.kind === "markdown" && typeof data.html === "string") {
    const sheet = element("div", "", "doc-sheet answer-content");
    // Server renders Markdown with raw HTML escaped and arbitrary links/images stripped.
    sheet.innerHTML = data.html;
    enhanceRenderedTables(sheet);
    container.append(sheet);
  } else if (data.kind === "code" || data.kind === "text") {
    const sheet = element("div", "", data.kind === "code" ? "code-sheet" : "text-sheet");
    const head = element("div", "", "sheet-meta-bar");
    head.append(element("span", (data.language || data.extension || "text").toUpperCase(), "sheet-meta-info"));
    sheet.append(head, copyActionControl("复制文本", () => data.text || ""));
    const pre = element("pre", "", "doc-pre");
    pre.append(element("code", data.text || ""));
    sheet.append(pre);
    container.append(sheet);
  } else {
    container.append(element("p", "暂不支持此格式的在线预览，请点击文件名从原位置打开。", "pdf-table-status"));
  }
  if (data.truncated) {
    container.append(element("p", "预览内容已截断以保持流畅，点击文件名可从原位置打开完整文件。", "pdf-table-status"));
  }
  return container;
}

function renderPreviewDisclosure(item, label = "预览文档") {
  const details = element("details", "", "doc-preview");
  details.append(element("summary", label));
  const body = element("div", "", "doc-preview-content");
  details.append(body);

  let loaded = false;
  details.addEventListener("toggle", async () => {
    if (!details.open || loaded) return;
    loaded = true;
    const ext = (item.extension || "").toLowerCase();
    const pdfUrl = /^\/preview\/[a-f0-9]{64}\/[1-9]\d*$/.test(item.page_preview_url || "")
      ? item.page_preview_url
      : ext === "pdf" && /^[a-f0-9]{64}$/.test(item.hash || "")
        ? "/preview/" + item.hash + "/1"
        : null;
    if (pdfUrl) {
      const status = element("p", "正在渲染 PDF 页面…", "pdf-table-status");
      const page = element("img", "", "source-page");
      page.alt = "PDF 预览 · " + (item.title || "");
      page.decoding = "async";
      page.hidden = true;
      page.addEventListener("load", () => {
        page.hidden = false;
        status.remove();
      });
      page.addEventListener("error", () => {
        status.textContent = "PDF 页面渲染失败。";
      });
      page.src = pdfUrl;
      body.replaceChildren(status, page);
      if (item.snippet) {
        body.append(pdfExtractedText(pdfUrl, element("p", item.snippet, "hit-snippet")));
      } else {
        const tables = pdfExtractedText(pdfUrl, element("p", "点击文件名或下载原文件查看完整内容。", "hit-snippet"));
        body.append(tables);
      }
      return;
    }

    const docUrl = /^[a-f0-9]{64}$/.test(item.hash || "") ? "/preview/" + item.hash + "/document" : null;
    if (!docUrl) {
      body.replaceChildren(element("p", "无法预览此文档。", "pdf-table-status"));
      return;
    }
    body.replaceChildren(element("p", "正在渲染文档预览…", "pdf-table-status"));
    try {
      const response = await fetch(docUrl);
      if (!response.ok) throw new Error("文档预览加载失败（" + response.status + "）");
      const data = await response.json();
      body.replaceChildren(renderDocumentPreviewBody(data));
    } catch (error) {
      loaded = false;
      body.replaceChildren(element("p", error.message || "文档预览加载失败，请重新展开重试。", "pdf-table-status"));
    }
  });
  return details;
}

function renderHit(hit) {
  const article = element("article", "", "hit");
  const openStatus = element("span", "", "open-status");
  openStatus.setAttribute("role", "status");
  const heading = element("h2");
  heading.append(openOriginalButton(hit, hit.title, openStatus));
  const actions = element("div", "", "hit-actions");
  actions.append(
    openOriginalButton(hit, "打开所在位置", openStatus, true, "open-action-link"),
    originalLink(hit, "下载原文件"),
    openStatus,
  );
  article.append(
    heading,
    element("p", [hit.source, hit.locator].join(" · "), "hit-meta"),
    renderSnippetView(hit),
  );
  if (hit.summary) article.append(element("p", hit.summary, "hit-meta"));
  if (hit.tags?.length) article.append(element("p", hit.tags.join(" · "), "hit-meta"));
  const previewLabel = hit.page_preview_url
    ? "预览 PDF 原页"
    : hit.image_preview_url
      ? "预览原图"
      : "预览完整文档";
  article.append(renderPreviewDisclosure(hit, previewLabel), actions);
  return article;
}

function appendCitedText(node, text, sourceCount) {
  for (const part of text.split(/(\[\d+\])/)) {
    const number = Number(/^\[(\d+)\]$/.exec(part)?.[1]);
    if (number > 0 && number <= sourceCount) {
      const id = "source-" + number;
      const link = element("a", "[" + number + "]", "citation");
      link.href = "#" + id;
      link.setAttribute("aria-label", "查看出处 " + number);
      link.addEventListener("click", event => {
        const source = byId(id);
        if (!source) return;
        event.preventDefault();
        source.open = true;
        source.querySelector("summary").focus();
      });
      node.append(link);
    } else {
      node.append(document.createTextNode(part));
    }
  }
}

function linkCitations(container, sourceCount) {
  const walker = document.createTreeWalker(container, NodeFilter.SHOW_TEXT);
  const markers = [];
  while (walker.nextNode()) {
    const text = walker.currentNode;
    if (/\[\d+\]/.test(text.data) && !text.parentElement.closest("code, pre")) {
      markers.push(text);
    }
  }
  for (const text of markers) {
    const fragment = document.createDocumentFragment();
    appendCitedText(fragment, text.data, sourceCount);
    text.replaceWith(fragment);
  }
}

function markdownTable({ header, rows }) {
  const cell = value => value.trim().replace(/\s*\r?\n\s*/g, " ")
    .replace(/[\\|*_\[\]`<>~&]/g, "\\$&");
  const line = values => "| " + values.map(cell).join(" | ") + " |";
  return [line(header), line(header.map(() => "---")), ...rows.map(line)].join("\n");
}

function renderPdfTable(data) {
  const section = element("div", "", "pdf-table");
  const actions = element("div", "", "pdf-table-actions");
  const copy = element("button", "复制 Markdown", "secondary");
  copy.type = "button";
  const status = element("span", "", "pdf-table-status");
  status.setAttribute("role", "status");
  const manual = element("textarea", "", "markdown-copy");
  manual.readOnly = true;
  manual.hidden = true;
  manual.rows = 5;
  manual.setAttribute("aria-label", "Markdown 表格");
  copy.addEventListener("click", async () => {
    const markdown = markdownTable(data);
    try {
      await navigator.clipboard.writeText(markdown);
      manual.hidden = true;
      status.textContent = "已复制";
    } catch {
      manual.value = markdown;
      manual.hidden = false;
      manual.focus();
      manual.select();
      status.textContent = "已选中 Markdown，可手动复制。";
    }
  });
  actions.append(copy, status);

  const scroller = element("div", "", "answer-table");
  const table = element("table");
  const head = table.createTHead().insertRow();
  for (const title of data.header) {
    const heading = element("th", title);
    heading.scope = "col";
    head.append(heading);
  }
  const body = table.createTBody();
  for (const values of data.rows) {
    const row = body.insertRow();
    for (const value of values) row.append(element("td", value));
  }
  scroller.append(table);
  section.append(actions, manual, scroller);
  return section;
}

function pdfExtractedText(preview, snippet) {
  const extracted = element("details", "", "source-text");
  const tables = element("div");
  const status = element("p", "", "pdf-table-status");
  status.setAttribute("role", "status");
  const raw = element("details", "", "source-raw");
  raw.open = true;
  raw.append(element("summary", "引用片段"), snippet);
  extracted.append(element("summary", "提取文本"), status, tables, raw);

  let requested = false;
  extracted.addEventListener("toggle", async () => {
    if (!extracted.open || requested) return;
    requested = true;
    status.textContent = "正在提取表格…";
    try {
      const response = await fetch(preview + "/tables");
      if (!response.ok) throw new Error("PDF table extraction failed");
      const data = await response.json();
      tables.replaceChildren(...data.map(renderPdfTable));
      raw.open = data.length === 0;
      status.textContent = data.length ? "" : "此页未识别到可复制的表格。";
    } catch {
      requested = false;
      status.textContent = "表格提取失败。可展开引用片段，或重新打开提取文本后重试。";
    }
  });
  return extracted;
}

function renderSource(hit, number) {
  const source = element("details", "", "answer-source");
  source.id = "source-" + number;
  const label = element("summary");
  const name = element("span", "", "source-label");
  name.append(element("span", "[" + number + "]", "source-number"), element("span", hit.title, "source-title"));
  label.append(name, element("span", hit.locator, "source-location"));
  source.append(label);
  const excerpt = element("div", "", "source-excerpt");
  const snippet = element("p", hit.snippet, "hit-snippet");
  if (/^\/preview\/[a-f0-9]{64}\/[1-9]\d*$/.test(hit.page_preview_url)) {
    const page = element("img", "", "source-page");
    page.alt = "PDF 原页 · " + hit.locator.split(",")[0];
    page.decoding = "async";
    page.hidden = true;
    const extracted = pdfExtractedText(hit.page_preview_url, snippet);
    page.addEventListener("load", () => { page.hidden = false; });
    page.addEventListener("error", () => { extracted.open = true; });
    source.addEventListener("toggle", function loadPage() {
      if (!source.open) return;
      page.src = hit.page_preview_url;
      source.removeEventListener("toggle", loadPage);
    });
    excerpt.append(page, extracted);
  } else if (/^\/preview\/[a-f0-9]{64}\/image$/.test(hit.image_preview_url || "")) {
    const img = element("img", "", "source-page");
    img.alt = "原图 · " + hit.title;
    img.decoding = "async";
    img.hidden = true;
    const extracted = element("details", "", "source-text");
    extracted.append(element("summary", "提取文本 (OCR)"), snippet);
    img.addEventListener("load", () => { img.hidden = false; });
    img.addEventListener("error", () => { extracted.open = true; });
    source.addEventListener("toggle", function loadImage() {
      if (!source.open) return;
      img.src = hit.image_preview_url;
      source.removeEventListener("toggle", loadImage);
    });
    excerpt.append(img, extracted);
  } else {
    excerpt.append(renderSnippetView(hit), renderPreviewDisclosure(hit, "预览完整文档"));
  }
  const sourceActions = element("div", "", "source-actions");
  const openStatus = element("span", "", "open-status");
  openStatus.setAttribute("role", "status");
  sourceActions.append(
    openOriginalButton(hit, "打开原文件", openStatus, false, "open-action-link"),
    openOriginalButton(hit, "打开所在位置", openStatus, true, "open-action-link"),
    originalLink(hit, "下载原文件"),
    openStatus,
  );
  excerpt.append(sourceActions);
  source.append(excerpt);
  return source;
}

function renderAnswer(result) {
  const answer = element("article", "", "answer");
  const header = element("div", "", "answer-header");
  header.append(element("h2", "回答"));
  if (result.model) {
    header.append(element("p", [result.provider === "deepseek" ? "DeepSeek" : "本地 Ollama", result.model].join(" · "), "provider"));
  }
  answer.append(header);

  const content = element("div", "", "answer-content");
  if (result.provider === "none") {
    content.append(element("p", "知识库中没有找到相关片段。"));
  } else if (typeof result.html === "string") {
    // The service renders Markdown with raw HTML escaped and arbitrary links removed.
    content.innerHTML = result.html;
    linkCitations(content, result.sources.length);
    for (const table of content.querySelectorAll("table")) {
      const scroller = element("div", "", "answer-table");
      table.replaceWith(scroller);
      scroller.append(table);
    }
  } else {
    content.append(element("p", result.text));
  }
  answer.append(content);
  output.append(answer);

  if (result.sources.length) {
    const sources = element("section", "", "sources");
    sources.append(element("h2", "原文出处"));
    result.sources.forEach((hit, index) => sources.append(renderSource(hit, index + 1)));
    output.append(sources);
  }
}

form.addEventListener("submit", async (event) => {
  event.preventDefault();
  const value = query.value.trim();
  if (!value) return;
  if (new TextEncoder().encode(value).length > 2000) {
    message("请输入更短的查询，最多 2000 字节。", true);
    return;
  }
  activeRequest?.abort();
  const controller = new AbortController();
  activeRequest = controller;
  const asking = mode === "ask";
  const keywordOnly = byId("keyword").checked;
  const payload = asking
    ? { question: value, cloud: !byId("cloud-option").hidden && byId("cloud").checked, keyword_only: keywordOnly }
    : { query: value, limit: 10, keyword_only: keywordOnly };
  submit.disabled = true;
  output.replaceChildren();
  message(asking ? "正在检索资料并生成回答…" : "正在搜索…");
  try {
    const result = await api(mode, payload, controller.signal);
    if (controller.signal.aborted) return;
    if (asking) {
      renderAnswer(result);
      message(result.warning || "");
    } else {
      result.hits.forEach((hit) => output.append(renderHit(hit)));
      const label = result.hits.length
        ? result.hits.length + " 个片段 · " + (result.mode === "hybrid" ? "混合检索" : "关键词检索")
          + (result.reranked ? " · 本地重排" : "")
        : "没有找到相关片段。";
      message([label, result.warning].filter(Boolean).join(" "));
    }
  } catch (error) {
    if (!controller.signal.aborted) message(error.message, true);
  } finally {
    if (activeRequest === controller) {
      activeRequest = null;
      submit.disabled = false;
    }
  }
});

query.addEventListener("keydown", (event) => {
  if (event.key === "Enter" && (event.metaKey || event.ctrlKey)) form.requestSubmit();
});
function renderArchiveMetaItem(label, value, isCode = false, titleText = "") {
  const item = element("div", "", "archive-meta-item");
  item.append(element("dt", label));
  const detail = element("dd");
  if (isCode) {
    const code = element("code", value);
    if (titleText) code.title = titleText;
    detail.append(code);
  } else {
    detail.textContent = value;
  }
  item.append(detail);
  return item;
}

function renderArchiveCard(doc) {
  const card = element("article", "", "archive-card");
  const openStatus = element("span", "", "open-status");
  openStatus.setAttribute("role", "status");
  const header = element("div", "", "archive-card-header");
  const title = element("h3", "", "archive-card-title");
  title.append(openOriginalButton(doc, doc.title, openStatus));
  const badges = element("div", "", "archive-badges");
  if (doc.extension) {
    badges.append(element("span", "." + doc.extension, "archive-ext"));
  }
  const status = docStatus(doc);
  const badge = element("span", status.label, "archive-status");
  badge.dataset.state = status.state;
  badges.append(badge);
  header.append(title, badges);
  card.append(header);

  const meta = element("dl", "", "archive-meta");
  const sourceNames = (doc.source_labels?.length ? doc.source_labels : doc.sources?.length ? doc.sources : [doc.source]).join("、");
  const sourceLabel = doc.origin_count > 1 ? sourceNames + "（" + doc.origin_count + " 处来源）" : sourceNames;
  meta.append(
    renderArchiveMetaItem("来源", sourceLabel),
    renderArchiveMetaItem("大小", formatBytes(doc.size)),
    renderArchiveMetaItem("归档时间", formatTimestamp(doc.archived_at)),
    renderArchiveMetaItem("索引进度", doc.passages + " 片段 · " + doc.embedded + " 向量"),
    renderArchiveMetaItem("SHA-256", doc.hash.slice(0, 12), true, doc.hash),
  );
  card.append(meta);

  if (doc.parse_error) {
    card.append(element("p", "解析错误：" + doc.parse_error, "archive-error"));
  }
  if (doc.summary) {
    card.append(element("p", doc.summary, "archive-summary"));
  }
  if (doc.tags?.length) {
    const tagList = element("div", "", "archive-tags");
    for (const tag of doc.tags) tagList.append(element("span", tag, "archive-tag"));
    card.append(tagList);
  }

  if (doc.parsed || doc.extension === "pdf" || isImageExtension(doc.extension)) {
    const previewLabel = doc.extension === "pdf"
      ? "预览首页"
      : isImageExtension(doc.extension)
        ? "预览图片"
        : "预览文档";
    card.append(renderPreviewDisclosure(doc, previewLabel));
  }

  const actions = element("div", "", "archive-card-actions");
  if (doc.parse_error) {
    const isRetrying = retryingAll || retryingHashes.has(doc.hash);
    const retry = element("button", isRetrying ? "正在重试…" : "重试", "secondary archive-retry");
    retry.type = "button";
    retry.disabled = isRetrying;
    retry.setAttribute("aria-label", "重试 " + doc.title);
    retry.addEventListener("click", () => retryFailed(doc.hash, doc.title));
    actions.append(retry);
  }
  actions.append(
    openOriginalButton(doc, "打开所在位置", openStatus, true, "open-action-link"),
    originalLink(doc, "下载原文件"),
    openStatus,
  );
  card.append(actions);
  return card;
}

function renderArchive() {
  if (mode !== "archive") return;
  const keyword = archiveFilter.value.trim().toLowerCase();
  const failedDocs = archiveFiles.filter((doc) => Boolean(doc.parse_error));
  if (archiveRetryFailed) {
    archiveRetryFailed.hidden = failedDocs.length === 0;
    archiveRetryFailed.disabled = retryingAll || archiveLoading;
    archiveRetryFailed.textContent = retryingAll
      ? "正在重试…"
      : "重试失败文件 (" + failedDocs.length + ")";
  }
  const countsByType = new Map();
  for (const doc of archiveFiles) {
    const type = classifyFileType(doc.extension);
    const entry = countsByType.get(type.id) || { ...type, count: 0 };
    entry.count += 1;
    countsByType.set(type.id, entry);
  }
  if (archiveTypeFilter === "failed" && !failedDocs.length) {
    archiveTypeFilter = "all";
  } else if (
    archiveTypeFilter !== "all"
    && archiveTypeFilter !== "failed"
    && !countsByType.has(archiveTypeFilter)
  ) {
    archiveTypeFilter = "all";
  }

  const orderedTypes = [...countsByType.values()].sort(
    (left, right) => right.count - left.count
      || FILE_TYPE_ORDER.indexOf(left.id) - FILE_TYPE_ORDER.indexOf(right.id),
  );

  archiveTypes.replaceChildren();
  const addTypeButton = (id, label, count) => {
    const button = element("button", label + " (" + count + ")", "archive-type-pill");
    button.type = "button";
    button.dataset.type = id;
    button.setAttribute("aria-pressed", String(archiveTypeFilter === id));
    button.addEventListener("click", () => {
      if (archiveTypeFilter === id) return;
      archiveTypeFilter = id;
      renderArchive();
    });
    archiveTypes.append(button);
  };
  addTypeButton("all", "全部", archiveFiles.length);
  if (failedDocs.length) {
    addTypeButton("failed", "解析失败", failedDocs.length);
  }
  for (const type of orderedTypes) addTypeButton(type.id, type.label, type.count);

  const filtered = archiveFiles.filter((doc) => {
    const type = classifyFileType(doc.extension);
    if (archiveTypeFilter === "failed") {
      if (!doc.parse_error) return false;
    } else if (archiveTypeFilter !== "all" && type.id !== archiveTypeFilter) {
      return false;
    }
    if (!keyword) return true;
    const haystack = [
      doc.title,
      doc.extension,
      "." + doc.extension,
      type.label,
      doc.source,
      ...(doc.sources || []),
      ...(doc.source_labels || []),
      doc.summary || "",
      ...(doc.tags || []),
      doc.hash,
    ].join(" ").toLowerCase();
    return haystack.includes(keyword);
  });

  output.replaceChildren();
  if (!archiveFiles.length) {
    message("知识库中暂无归档文档。将文件放入投递目录或监控文件夹后会自动归档。");
    return;
  }
  if (!filtered.length) {
    message("没有符合当前筛选条件的归档文档。");
    return;
  }

  const grouped = new Map();
  for (const doc of filtered) {
    const type = classifyFileType(doc.extension);
    let group = grouped.get(type.id);
    if (!group) {
      group = { ...type, docs: [], extensions: new Set() };
      grouped.set(type.id, group);
    }
    group.docs.push(doc);
    if (doc.extension) group.extensions.add("." + doc.extension);
  }
  const sortedGroups = [...grouped.values()].sort(
    (left, right) => right.docs.length - left.docs.length
      || FILE_TYPE_ORDER.indexOf(left.id) - FILE_TYPE_ORDER.indexOf(right.id),
  );

  const totalBytes = filtered.reduce((sum, doc) => sum + (doc.size || 0), 0);
  message(
    "共 " + filtered.length + " 个归档文档 · " + sortedGroups.length + " 种文件类型 · 总计 " + formatBytes(totalBytes),
  );

  for (const group of sortedGroups) {
    const section = element("details", "", "archive-group");
    section.open = true;
    const summary = element("summary", "", "archive-group-header");
    const heading = element("div", "", "archive-group-title");
    heading.append(element("span", group.label, "archive-group-name"));
    const exts = [...group.extensions].sort().join(" · ");
    if (exts) heading.append(element("span", exts, "archive-group-exts"));
    summary.append(heading, element("span", group.docs.length + " 个文件", "archive-group-count"));
    section.append(summary);

    const list = element("div", "", "archive-cards");
    for (const doc of group.docs) list.append(renderArchiveCard(doc));
    section.append(list);
    output.append(section);
  }
}

async function loadArchive(quiet = false) {
  if (archiveLoading) {
    archiveReloadQueued = true;
    return;
  }
  archiveLoading = true;
  archiveRefresh.disabled = true;
  if (archiveRetryFailed) archiveRetryFailed.disabled = true;
  if (!quiet && !archiveLoaded && mode === "archive") {
    message("正在加载归档文档…");
  }
  try {
    const data = await api("archive");
    archiveFiles = Array.isArray(data.files) ? data.files : [];
    archiveLoaded = true;
    if (mode === "archive") renderArchive();
  } catch (error) {
    if (mode === "archive") message(error.message, true);
  } finally {
    archiveLoading = false;
    archiveRefresh.disabled = false;
    if (archiveRetryFailed) archiveRetryFailed.disabled = retryingAll;
    if (archiveReloadQueued) {
      archiveReloadQueued = false;
      loadArchive(true);
    }
  }
}

async function retryFailed(hash = null, title = "") {
  if (retryingAll || (hash && retryingHashes.has(hash))) return;
  if (hash) {
    retryingHashes.add(hash);
  } else {
    retryingAll = true;
  }
  if (retryFailedButton) retryFailedButton.disabled = true;
  if (archiveRetryFailed) archiveRetryFailed.disabled = true;
  if (mode === "archive") renderArchive();
  try {
    const result = await api("retry", hash ? { hash } : {});
    archiveStatusSignature = null;
    await Promise.all([
      refreshStatus(false),
      archiveLoaded || mode === "archive" ? loadArchive(true) : Promise.resolve(),
    ]);
    if (mode === "archive") {
      message(
        title
          ? "已重新排队解析：" + title
          : "已重新排队 " + (result.retried || 0) + " 个失败文件。",
      );
    } else {
      byId("last-scan").textContent = title
        ? "已重新排队：" + title
        : "已安排重试失败文件";
    }
    setTimeout(() => {
      refreshStatus(false);
      if (archiveLoaded || mode === "archive") loadArchive(true);
    }, 1100);
  } catch (error) {
    if (mode === "archive") {
      message(error.message, true);
    } else {
      byId("last-scan").textContent = error.message;
    }
  } finally {
    if (hash) {
      retryingHashes.delete(hash);
    } else {
      retryingAll = false;
    }
    if (retryFailedButton) retryFailedButton.disabled = false;
    if (archiveRetryFailed) archiveRetryFailed.disabled = archiveLoading;
    if (mode === "archive") renderArchive();
  }
}

byId("mode-search").addEventListener("click", () => setMode("search"));
byId("mode-ask").addEventListener("click", () => setMode("ask"));
byId("mode-archive").addEventListener("click", () => setMode("archive"));
archiveFilter.addEventListener("input", () => renderArchive());
archiveRefresh.addEventListener("click", () => loadArchive(false));
if (archiveRetryFailed) {
  archiveRetryFailed.addEventListener("click", () => retryFailed());
}
if (retryFailedButton) {
  retryFailedButton.addEventListener("click", () => retryFailed());
}

const folderDialog = byId("folder-dialog");
const folderForm = byId("folder-form");
const folderInput = byId("folder-path");
const folderList = byId("folder-list");
const folderPicker = byId("pick-folder");
let folderBusy = false;

function folderMessage(text, error = false) {
  const feedback = byId("folder-feedback");
  feedback.textContent = text;
  feedback.dataset.error = String(error);
  feedback.hidden = !text;
}

function setFolderBusy(busy) {
  folderBusy = busy;
  folderInput.disabled = busy;
  byId("add-folder").disabled = busy;
  folderPicker.disabled = busy;
  for (const button of folderList.querySelectorAll("button")) button.disabled = busy;
}

function renderFolders(folders) {
  folderList.replaceChildren();
  for (const folder of folders) {
    const row = element("li", "", "folder-row");
    const details = element("div", "", "folder-label");
    const name = folder.inbox ? "投递目录" : folder.path.split(/[\\/]/).filter(Boolean).at(-1) || folder.path;
    details.append(element("strong", name), element("code", folder.path));
    if (folder.permission_error) {
      details.append(element("p", "已暂停监控。请在 Mac 上重新选择此文件夹。", "folder-permission"));
    }
    const remove = element("button", "移除", "secondary");
    remove.type = "button";
    remove.setAttribute("aria-label", "移除 " + folder.path);
    remove.addEventListener("click", () => changeFolder("remove", folder.path));
    row.append(details, remove);
    folderList.append(row);
  }
  byId("folder-empty").hidden = folders.length !== 0;
  byId("folder-removal-hint").hidden = folders.length === 0;
}

async function changeFolder(action, path) {
  if (!path || folderBusy) return;
  setFolderBusy(true);
  folderMessage(action === "add" ? "正在添加文件夹…" : "正在移除文件夹…");
  try {
    renderFolders(await api("folders", { action, path }));
    if (action === "add") folderInput.value = "";
    folderMessage(action === "add" ? "已添加文件夹。" : "已移除文件夹。");
  } catch (error) {
    folderMessage(error.message, true);
  } finally {
    setFolderBusy(false);
    if (folderDialog.open) folderInput.focus();
  }
}

byId("manage-folders").addEventListener("click", async () => {
  folderDialog.showModal();
  if (folderBusy) return;
  setFolderBusy(true);
  folderMessage("正在读取监控文件夹…");
  try {
    renderFolders(await api("folders"));
    folderMessage("");
  } catch (error) {
    folderMessage(error.message, true);
  } finally {
    setFolderBusy(false);
    if (folderDialog.open) folderInput.focus();
  }
});
byId("close-folders").addEventListener("click", () => folderDialog.close());
folderForm.addEventListener("submit", event => {
  event.preventDefault();
  changeFolder("add", folderInput.value.trim());
});
folderPicker.addEventListener("click", async () => {
  if (folderBusy) return;
  setFolderBusy(true);
  folderMessage("请在 macOS 窗口中选择要监控的文件夹…");
  try {
    const initial = folderInput.value.trim();
    const { folders, selected } = await api("folders/pick", initial ? { initial } : {});
    renderFolders(folders);
    if (selected) folderInput.value = "";
    folderMessage(selected ? "已授权并监控所选文件夹。" : "已取消选择。");
  } catch (error) {
    folderMessage(error.message, true);
  } finally {
    setFolderBusy(false);
    if (folderDialog.open) folderPicker.focus();
  }
});

let lastScanTime = null;
let pendingScan = false;
scan.addEventListener("click", async () => {
  scan.disabled = true;
  try {
    await api("scan", {});
    pendingScan = true;
    byId("last-scan").textContent = "已安排扫描";
  } catch (error) {
    byId("last-scan").textContent = error.message;
  } finally {
    scan.disabled = false;
  }
});

async function refreshStatus(scheduleNext = true) {
  try {
    const status = await api("status");
    const { counts, progress } = status;
    byId("folder-picker").hidden = !status.folder_picker;
    byId("file-count").textContent = counts.files;
    byId("passage-count").textContent = counts.passages;
    byId("vector-count").textContent = counts.embedded;
    byId("inbox").hidden = !status.inbox;
    byId("inbox-path").textContent = status.inbox || "";
    const lastScan = progress.last_scan
      ? "上次扫描 " + new Date(progress.last_scan).toLocaleString("zh-CN")
      : "等待首次扫描";
    if (progress.last_scan !== lastScanTime) pendingScan = false;
    lastScanTime = progress.last_scan;
    byId("last-scan").textContent = progress.scanning ? "正在扫描…" : pendingScan ? "已安排扫描" : lastScan;
    const generalErrors = [
      progress.config_error,
      progress.scan_error,
      progress.embedding_error,
      progress.annotation_error,
    ].filter(Boolean);
    const fileErrors = Array.isArray(status.errors) ? status.errors : [];
    const hasErrors = generalErrors.length > 0 || fileErrors.length > 0;
    const issues = byId("issues");
    issues.hidden = !hasErrors;
    if (counts.failed > 0 && lastFailedCount === 0) {
      issues.open = true;
    }
    lastFailedCount = counts.failed;
    byId("issues-label").textContent = counts.failed ? counts.failed + " 个文件处理失败" : "索引详情";
    const issuesActions = byId("issues-actions");
    if (issuesActions && retryFailedButton) {
      const canRetryAll = counts.failed > 0 || fileErrors.length > 0;
      issuesActions.hidden = !canRetryAll;
      retryFailedButton.disabled = retryingAll;
      retryFailedButton.textContent = retryingAll
        ? "正在重试…"
        : counts.failed > 1
          ? "重试全部失败文件 (" + counts.failed + ")"
          : "重试失败文件";
    }
    const issueItems = [
      ...generalErrors.map((error) => {
        const item = element("li", "", "issue-item");
        item.append(element("span", error, "issue-text"));
        return item;
      }),
      ...fileErrors.map((issue) => {
        const item = element("li", "", "issue-item");
        item.append(element("span", issue.title + " · " + issue.message, "issue-text"));
        if (issue.hash) {
          const isRetrying = retryingAll || retryingHashes.has(issue.hash);
          const retry = element("button", isRetrying ? "正在重试…" : "重试", "secondary issue-retry");
          retry.type = "button";
          retry.disabled = isRetrying;
          retry.setAttribute("aria-label", "重试 " + issue.title);
          retry.addEventListener("click", () => retryFailed(issue.hash, issue.title));
          item.append(retry);
        }
        return item;
      }),
    ];
    byId("issues-list").replaceChildren(...issueItems);
    connection.textContent = "本地服务已连接";
    connection.dataset.state = "live";
    if (status.theme && themeSyncing === 0) applyTheme(status.theme);
    const nextSignature = [
      counts.files,
      counts.parsed,
      counts.failed,
      counts.embedded,
      ...fileErrors.map((issue) => (issue.hash || issue.title) + ":" + issue.message),
    ].join(":");
    const hasPendingArchiveDoc = archiveFiles.some((doc) => !doc.parsed && !doc.parse_error);
    if (
      mode === "archive"
      && ((archiveStatusSignature !== null && nextSignature !== archiveStatusSignature)
        || (hasPendingArchiveDoc && counts.files <= counts.parsed + counts.failed))
    ) {
      loadArchive(true);
    }
    archiveStatusSignature = nextSignature;
    cloudEnabled = status.cloud_enabled;
    updateCloudOption();
  } catch {
    connection.textContent = "本地服务未连接";
    connection.dataset.state = "error";
    if (!pendingScan) byId("last-scan").textContent = "无法读取索引状态";
  } finally {
    if (scheduleNext) setTimeout(refreshStatus, 3500);
  }
}

refreshStatus();
