"use strict";

const form = document.getElementById("pair-form");
const input = document.getElementById("access-code");
const pairing = document.getElementById("pairing");
const content = document.getElementById("dashboard-content");
const connection = document.getElementById("connection-state");
const notice = document.getElementById("connection-notice");
const lastRefresh = document.getElementById("last-refresh");
let code;
let timer;
let inFlight = false;
let lastRead;

function state(label, tone, detail = "") {
  if (connection.textContent !== label) connection.textContent = label;
  connection.dataset.tone = tone;
  if (notice.textContent !== detail) notice.textContent = detail;
  notice.hidden = detail === "";
}

function replaceCards(html) {
  const page = new DOMParser().parseFromString(html, "text/html");
  content.replaceChildren(...page.body.children);
}

function showReadTime() {
  if (!lastRead) return;
  const seconds = Number(lastRead.dataset.observedAt);
  if (!Number.isFinite(seconds) || seconds <= 0) return;
  const timestamp = new Date(seconds * 1000);
  lastRefresh.dataset.observedAt = String(seconds);
  lastRefresh.dateTime = timestamp.toISOString();
  lastRefresh.textContent = lastRefresh.dateTime.slice(11, 19) + " UTC";
}

async function refresh() {
  clearTimeout(timer);
  if (!code || document.hidden || inFlight) return;
  inFlight = true;
  try {
    const reply = await fetch("/cards", {
      headers: { Authorization: "Bearer " + code },
      mode: "same-origin", credentials: "omit", cache: "no-store", redirect: "error",
      signal: AbortSignal.timeout(3500)
    });
    if (reply.status === 401) {
      code = undefined;
      lastRead = undefined;
      delete lastRefresh.dataset.observedAt;
      lastRefresh.textContent = "—";
      content.replaceChildren();
      sources.hidden = true;
      sourceRequest?.abort();
      sourceRequest = undefined;
      sourceResults.replaceChildren();
      document.getElementById("indexed-source-id").value = "";
      reviewFile.value = "";
      reviewResults.replaceChildren();
      pairing.hidden = false;
      state("Pairing required", "warn", "Use the access code from this dashboard launch.");
      return;
    }
    if (!reply.ok) throw new Error("refresh");
    const html = await reply.text();
    pairing.hidden = true;
    if (reply.headers.get("X-AnyRAG-Index-State") === "available") {
      replaceCards(html);
      sources.hidden = false;
      lastRead = document.getElementById("snapshot-observation");
      showReadTime();
      state("Connected", "good");
    } else {
      if (!lastRead) replaceCards(html);
      else document.getElementById("graph-review")?.remove();
      state("Stale", "warn", lastRead
        ? "The index could not be read. Showing the last successful refresh."
        : "The index could not be read. The dashboard will retry.");
    }
  } catch {
    document.getElementById("graph-review")?.remove();
    state("Stale", "warn", lastRead
      ? "Refresh failed. Showing the last successful refresh."
      : "Refresh failed. Check that the dashboard command is still running.");
  } finally {
    inFlight = false;
    if (code && !document.hidden) timer = setTimeout(refresh, 2000);
  }
}

form.addEventListener("submit", (event) => {
  event.preventDefault();
  const next = input.value.trim();
  input.value = "";
  if (!/^[a-f0-9]{64}$/.test(next)) {
    state("Pairing required", "warn", "Enter the 64-character access code from the terminal.");
    return;
  }
  code = next;
  state("Connecting", "warn");
  refresh();
});

document.addEventListener("visibilitychange", () => {
  if (document.hidden) clearTimeout(timer);
  else refresh();
});

const sources = document.getElementById("indexed-sources");
const sourceResults = document.getElementById("source-results");
const loadSources = document.getElementById("load-sources");
let sourceRequest;

async function showSources(id) {
  sourceRequest?.abort();
  const request = new AbortController();
  sourceRequest = request;
  sourceResults.textContent = "Loading…";
  try {
    const response = await fetch(id ? "/sources/" + id : "/sources", {
      headers: { Authorization: "Bearer " + code },
      mode: "same-origin", credentials: "omit", cache: "no-store", redirect: "error",
      signal: AbortSignal.any([request.signal, AbortSignal.timeout(3500)])
    });
    if (response.status === 401) {
      if (sourceRequest === request) refresh();
      throw new Error("session expired");
    }
    if (!response.ok) throw new Error("source read");
    const page = new DOMParser().parseFromString(await response.text(), "text/html");
    if (sourceRequest === request) sourceResults.replaceChildren(...page.body.children);
  } catch {
    if (sourceRequest === request) sourceResults.textContent = "The source could not be read. Try again.";
  }
}

loadSources.addEventListener("click", () => showSources());
document.getElementById("open-indexed-source").addEventListener("submit", (event) => {
  event.preventDefault();
  const id = document.getElementById("indexed-source-id").value.trim().toLowerCase();
  if (/^[a-f0-9]{64}$/.test(id)) showSources(id);
});
sourceResults.addEventListener("click", (event) => {
  const id = event.target.closest("button[data-source-id]")?.dataset.sourceId;
  if (id && /^[a-f0-9]{64}$/.test(id)) showSources(id);
});

const reviewFile = document.getElementById("ingest-review-file");
const reviewResults = document.getElementById("ingest-review-results");
reviewFile.addEventListener("change", async () => {
  const file = reviewFile.files[0];
  reviewResults.replaceChildren();
  if (!file) return;
  try {
    if (file.size > 16 * 1024 * 1024) throw new Error("report size");
    const report = JSON.parse(await file.text());
    if (reviewFile.files[0] !== file) return;
    if (!Array.isArray(report.source_review)) throw new Error("report format");
    const rows = report.source_review.filter(row => ["skipped", "failed", "deferred", "recheck_deferred"].includes(row?.status));
    const summary = document.createElement("p");
    summary.textContent = rows.length ? "Showing " + Math.min(rows.length, 100) + " of " + rows.length + " files to review." : "This report has no skipped, failed, or deferred files.";
    const list = document.createElement("ul");
    for (const row of rows.slice(0, 100)) {
      const item = document.createElement("li");
      item.textContent = [row.path, row.status, row.reason].filter(value => typeof value === "string").join(" · ");
      list.append(item);
    }
    reviewResults.replaceChildren(summary, list);
  } catch {
    if (reviewFile.files[0] === file) reviewResults.textContent = "Choose a valid ingest review JSON file of up to 16 MiB.";
  }
});
