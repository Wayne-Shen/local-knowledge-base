"use strict";

const byId = (id) => document.getElementById(id);
const query = byId("query");
const form = byId("query-form");
const submit = byId("submit");
const output = byId("output");
const feedback = byId("feedback");
const connection = byId("connection");
const scan = byId("scan");

let mode = "search";
let cloudEnabled = false;
let activeRequest = null;

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
  byId("mode-search").setAttribute("aria-pressed", String(!asking));
  byId("mode-ask").setAttribute("aria-pressed", String(asking));
  byId("workspace-title").textContent = asking ? "向知识库提问" : "搜索你的资料";
  byId("query-label").textContent = asking ? "问题" : "搜索词";
  query.placeholder = asking ? "输入你想了解的问题" : "输入关键词或短语";
  submit.textContent = asking ? "提问" : "搜索";
  submit.disabled = false;
  output.replaceChildren();
  message("");
  updateCloudOption();
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

function renderHit(hit) {
  const article = element("article", "", "hit");
  const heading = element("h2");
  heading.append(originalLink(hit));
  article.append(
    heading,
    element("p", [hit.source, hit.locator].join(" · "), "hit-meta"),
    element("p", hit.snippet, "hit-snippet"),
  );
  if (hit.summary) article.append(element("p", hit.summary, "hit-meta"));
  if (hit.tags?.length) article.append(element("p", hit.tags.join(" · "), "hit-meta"));
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
  } else {
    excerpt.append(snippet);
  }
  excerpt.append(originalLink(hit, "下载原文件"));
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
byId("mode-search").addEventListener("click", () => setMode("search"));
byId("mode-ask").addEventListener("click", () => setMode("ask"));

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

async function refreshStatus() {
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
    const errors = [
      progress.config_error,
      progress.scan_error,
      progress.embedding_error,
      progress.annotation_error,
      ...status.errors.map((issue) => issue.title + " · " + issue.message),
    ].filter(Boolean);
    byId("issues").hidden = !errors.length;
    byId("issues-label").textContent = counts.failed ? counts.failed + " 个文件处理失败" : "索引详情";
    byId("issues-list").replaceChildren(...errors.map((error) => element("li", error)));
    connection.textContent = "本地服务已连接";
    connection.dataset.state = "live";
    cloudEnabled = status.cloud_enabled;
    updateCloudOption();
  } catch {
    connection.textContent = "本地服务未连接";
    connection.dataset.state = "error";
    if (!pendingScan) byId("last-scan").textContent = "无法读取索引状态";
  } finally {
    setTimeout(refreshStatus, 3500);
  }
}

refreshStatus();
