"use strict";

const form = document.getElementById("pair-form");
const input = document.getElementById("access-code");
const pairing = document.getElementById("pairing");
const content = document.getElementById("dashboard-content");
const connection = document.getElementById("connection-state");
const notice = document.getElementById("connection-notice");
const lastRefresh = document.getElementById("last-refresh");
const announcement = document.getElementById("dashboard-announcement");
const retryAge = document.getElementById("retry-age");
const workspace = document.getElementById("overview");
const title = document.getElementById("workspace-title");
const ingestReport = document.getElementById("ingest-report");
const sectionLinks = document.querySelectorAll(".rail nav a");
const views = {
  overview: ["overview", "Overview"],
  pipeline: ["coverage", "Coverage"],
  "indexed-sources": ["sources", "Sources"],
  "recent-activity": ["activity", "Activity"],
  "graph-review": ["graph", "Graph review"]
};
let code;
let timer;
let inFlight = false;
let lastRead;
let selectedProposal;
const drawnCharts = new WeakSet();

function drawCharts() {
  const styles = getComputedStyle(workspace);
  const ink = (name) => styles.getPropertyValue("--" + name).trim();
  for (const canvas of content.querySelectorAll("canvas[data-timing-samples], canvas[data-outcome-counts]")) {
    if (drawnCharts.has(canvas)) continue;
    const context = canvas.getContext("2d");
    if (!context) continue;
    if (canvas.dataset.timingSamples) {
      drawTimingChart(context, canvas, ink("gold"), ink("line"));
    } else {
      drawOutcomeRing(context, canvas, ["green", "gold", "abstention", "red"].map(ink), ink("line"));
    }
    drawnCharts.add(canvas);
  }
}

function drawTimingChart(context, { width, height, dataset }, accent, grid) {
  const samples = dataset.timingSamples.split(",").map(Number);
  const max = Math.max(1, ...samples);
  const baseline = height - 2;
  const plotHeight = height - 12;
  context.strokeStyle = grid;
  context.lineWidth = 2;
  context.beginPath();
  for (let tick = 0; tick <= 4; tick++) {
    const y = baseline - tick * plotHeight / 4;
    context.moveTo(0, y);
    context.lineTo(width, y);
  }
  context.stroke();
  const stride = width / Math.max(12, samples.length);
  const barWidth = stride * 0.58;
  const right = width - (stride + barWidth) / 2;
  const fill = context.createLinearGradient(0, height, 0, 0);
  fill.addColorStop(0, "#564937");
  fill.addColorStop(1, accent);
  samples.forEach((sample, index) => {
    const bar = Math.max(2, sample / max * plotHeight);
    const x = right - (samples.length - 1 - index) * stride;
    context.fillStyle = index === samples.length - 1 ? accent : fill;
    context.fillRect(x, baseline - bar, barWidth, bar);
  });
}

function drawOutcomeRing(context, { width, height, dataset }, colors, track) {
  const counts = dataset.outcomeCounts.split(",").map(Number);
  const total = counts.reduce((sum, count) => sum + count, 0);
  const x = width / 2;
  const y = height / 2;
  const radius = Math.min(x, y) - 17;
  const turn = Math.PI * 2;
  context.lineWidth = 22;
  context.strokeStyle = track;
  context.beginPath();
  context.arc(x, y, radius, 0, turn);
  context.stroke();
  let start = -Math.PI / 2;
  counts.forEach((count, index) => {
    if (count === 0) return;
    const end = start + turn * count / total;
    const gap = total === count ? 0 : Math.min(0.055, (end - start) / 4);
    context.strokeStyle = colors[index];
    context.beginPath();
    context.arc(x, y, radius, start + gap, end - gap);
    context.stroke();
    start = end;
  });
}

function showProposal(root, requested) {
  const buttons = [...root.querySelectorAll("[data-review-item]")];
  const selected = buttons.find(button => button.dataset.reviewItem === requested) ?? buttons[0];
  for (const button of buttons) button.setAttribute("aria-pressed", String(button === selected));
  for (const detail of root.querySelectorAll(".review-list > li")) {
    detail.hidden = detail.id !== selected?.dataset.reviewItem;
  }
  selectedProposal = selected?.dataset.reviewItem;
}

function state(label, tone, detail = "", message = label) {
  if (connection.textContent !== label) {
    connection.textContent = label;
    announcement.textContent = message;
  }
  connection.dataset.tone = tone;
  if (notice.textContent !== detail) notice.textContent = detail;
  notice.hidden = detail === "";
}

function replaceCards(html) {
  const next = new DOMParser().parseFromString(html, "text/html")
    .getElementById("snapshot-observation");
  const current = document.getElementById("snapshot-observation");
  if (!next) return;
  showProposal(next, selectedProposal);
  if (!current) {
    content.replaceChildren(next);
    return;
  }
  const focused = document.activeElement?.dataset.focusKey;
  const reviewHadFocus = current.querySelector("#graph-review")?.contains(document.activeElement);
  const open = new Set([...current.querySelectorAll("details[open][id]")].map(node => node.id));
  for (const details of next.querySelectorAll("details[id]")) details.open = open.has(details.id);
  const ordered = [...next.children];
  for (const [index, section] of ordered.entries()) {
    const existing = current.children[index];
    if (!existing) current.append(section);
    else if (!existing.isEqualNode(section)) existing.replaceWith(section);
  }
  while (current.children.length > ordered.length) current.lastElementChild.remove();
  Object.assign(current.dataset, next.dataset);
  if (focused && document.activeElement === document.body) {
    const target = [...current.querySelectorAll("[data-focus-key]")]
      .find(node => node.dataset.focusKey === focused);
    const review = reviewHadFocus ? current.querySelector("#graph-review") : null;
    const replacement = target ??
      review?.querySelector(".review-list li:not([hidden]) button") ??
      review?.querySelector("[aria-pressed='true']") ?? review?.querySelector("h2");
    replacement?.focus({ preventScroll: true });
  }
}

function updateNav(hash = location.hash) {
  const active = document.getElementById(hash.slice(1));
  const section = active?.closest("#pipeline, #indexed-sources, #recent-activity, #graph-review, #ingest-report");
  const id = section?.hidden ? "overview" :
    section?.id === "ingest-report" ? "recent-activity" : section?.id ?? "overview";
  const [view, heading] = views[id];
  workspace.dataset.view = view;
  if (title.textContent !== heading) title.textContent = heading;
  for (const link of sectionLinks) {
    const target = document.getElementById(link.hash.slice(1));
    link.hidden = !target || target.hidden;
    if (link.hash === "#" + id) link.setAttribute("aria-current", "location");
    else link.removeAttribute("aria-current");
  }
}

function showReadTime() {
  if (!lastRead) return;
  const seconds = Number(lastRead.dataset.observedAt);
  if (!Number.isFinite(seconds) || seconds <= 0) return;
  const timestamp = new Date(seconds * 1000);
  lastRefresh.dataset.observedAt = String(seconds);
  lastRefresh.dateTime = timestamp.toISOString();
  lastRefresh.title = lastRefresh.dateTime.replace("T", " ").slice(0, 19) + " UTC";
  lastRefresh.textContent = lastRefresh.title;
}

function showStale(detail) {
  const review = document.getElementById("graph-review");
  if (review && !review.dataset.stale) {
    const height = review.getBoundingClientRect().height;
    const replacement = document.createElement("p");
    replacement.textContent = "Graph review will return after the next successful refresh.";
    review.replaceChildren(replacement);
    review.dataset.stale = "true";
    review.style.minHeight = `${height}px`;
  }
  const label = document.getElementById("verdict-label");
  if (label && lastRefresh.title) label.textContent = "As of " + lastRefresh.title;
  state("Stale", "warn", lastRead ? "" : detail, "Dashboard refresh failed. Retrying.");
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
      lastRefresh.removeAttribute("datetime");
      lastRefresh.removeAttribute("title");
      lastRefresh.textContent = "—";
      retryAge.textContent = "";
      content.replaceChildren();
      sources.hidden = true;
      ingestReport.hidden = true;
      selectedProposal = undefined;
      sourceRequest?.abort();
      sourceRequest = undefined;
      sourceResults.replaceChildren();
      sourceStatus.textContent = "";
      document.getElementById("indexed-source-id").value = "";
      reviewFile.value = "";
      reviewResults.replaceChildren();
      pairing.hidden = false;
      state("Pairing required", "warn", "Use the access code from this dashboard launch.");
      input.focus();
      return;
    }
    if (!reply.ok) throw new Error("refresh");
    const html = await reply.text();
    const pairingHadFocus = pairing.contains(document.activeElement);
    pairing.hidden = true;
    if (reply.headers.get("X-AnyRAG-Index-State") === "available") {
      const wasConnected = connection.textContent === "Connected";
      const previousSync = lastRead?.dataset.syncAt;
      const previousIngest = lastRead?.dataset.ingest;
      replaceCards(html);
      drawCharts();
      sources.hidden = false;
      ingestReport.hidden = false;
      lastRead = document.getElementById("snapshot-observation");
      showReadTime();
      state("Connected", "good", "", "Dashboard connected.");
      if (wasConnected && previousSync !== lastRead.dataset.syncAt && lastRead.dataset.syncAt !== "0") {
        announcement.textContent = "Full sync completed at " + new Date(Number(lastRead.dataset.syncAt) * 1000).toISOString().slice(11, 19) + " UTC.";
      } else if (wasConnected && previousIngest !== lastRead.dataset.ingest &&
          /-(partial|error)$/.test(lastRead.dataset.ingest)) {
        announcement.textContent = "Latest ingest needs review.";
      }
      if (pairingHadFocus) title.focus({ preventScroll: true });
    } else {
      if (!lastRead) replaceCards(html);
      showStale("The index could not be read. The dashboard will retry.");
    }
  } catch {
    showStale("Refresh failed. Check that the dashboard command is still running.");
  } finally {
    retryAge.textContent = connection.textContent === "Stale" && lastRead
      ? "Retrying · " + Math.max(0, Math.floor(Date.now() / 1000) - Number(lastRead.dataset.observedAt)) + "s since refresh"
      : "";
    updateNav();
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

window.addEventListener("hashchange", () => updateNav());
for (const link of sectionLinks) {
  link.addEventListener("click", (event) => {
    if (event.ctrlKey || event.shiftKey || event.metaKey || event.altKey) return;
    event.preventDefault();
    if (location.hash !== link.hash) history.pushState(null, "", link.hash);
    updateNav(link.hash);
    window.scrollTo(0, 0);
    title.focus({ preventScroll: true });
  });
}
updateNav();
content.addEventListener("click", (event) => {
  const proposal = event.target.closest("button[data-review-item]");
  if (proposal) {
    showProposal(content, proposal.dataset.reviewItem);
    return;
  }
  const source = event.target.closest("button[data-review-source]")?.dataset.reviewSource;
  if (source && /^[a-f0-9]{64}$/.test(source)) {
    location.hash = "#indexed-sources";
    updateNav();
    showSources({ id: source });
    return;
  }
  const link = event.target.closest(".attention-list a, a[data-live-action], a[data-chart-link]");
  if (!link || event.ctrlKey || event.shiftKey || event.metaKey || event.altKey) return;
  updateNav(link.hash);
  const target = document.getElementById(link.hash.slice(1));
  const details = target?.matches("details") ? target : target?.querySelector(".activity-details");
  if (details) {
    details.open = true;
    details.querySelector("summary").focus({ preventScroll: true });
  }
});

const sources = document.getElementById("indexed-sources");
const sourceResults = document.getElementById("source-results");
const sourceStatus = document.getElementById("source-status");
const loadSources = document.getElementById("load-sources");
let sourceRequest;
let sourcePage = 1;

async function showSources({ id, page = 1 } = {}) {
  sourceRequest?.abort();
  const request = new AbortController();
  sourceRequest = request;
  const focused = document.activeElement;
  sourceStatus.textContent = "Loading…";
  try {
    const sourcePath = id ? "/sources/" + id : "/sources";
    const path = page === 1 ? sourcePath : sourcePath + "/page/" + page;
    const response = await fetch(path, {
      headers: { Authorization: "Bearer " + code },
      mode: "same-origin", credentials: "omit", cache: "no-store", redirect: "error",
      signal: AbortSignal.any([request.signal, AbortSignal.timeout(3500)])
    });
    if (response.status === 401) {
      if (sourceRequest === request) refresh();
      throw new Error("session expired");
    }
    if (!response.ok) throw new Error("source read");
    const parsed = new DOMParser().parseFromString(await response.text(), "text/html");
    if (sourceRequest === request) {
      if (!id) sourcePage = page;
      const moveFocus = document.activeElement === focused;
      sourceResults.replaceChildren(...parsed.body.children);
      sourceStatus.textContent = "";
      if (moveFocus) sourceResults.querySelector("h3, button")?.focus();
    }
  } catch {
    if (sourceRequest === request) sourceStatus.textContent = "The source could not be read. Try again.";
  }
}

loadSources.addEventListener("click", () => showSources());
document.getElementById("open-indexed-source").addEventListener("submit", (event) => {
  event.preventDefault();
  const id = document.getElementById("indexed-source-id").value.trim().toLowerCase();
  if (/^[a-f0-9]{64}$/.test(id)) showSources({ id });
});
sourceResults.addEventListener("click", (event) => {
  const button = event.target.closest("button");
  if (!button) return;
  const id = button.dataset.sourceId;
  const page = button.dataset.sourcePage;
  if (id && /^[a-f0-9]{64}$/.test(id)) showSources({ id, page: Number(button.dataset.passagePage ?? 1) });
  if (page && /^\d+$/.test(page)) showSources({ page: Number(page) });
  if (button.id === "back-to-sources") showSources({ page: sourcePage });
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
