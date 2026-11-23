import { mountDiagnostics } from "./diagnostics.js";
import { mountOperations, routeDiagram } from "./operations.js";

const byId = id => document.getElementById(id);
const workspace = byId("workspace");
const queueForm = byId("queue-form");
const contextForm = byId("context-form");
const decisionForm = byId("decision-form");
const status = byId("status");
const previewText = byId("context-text");
const approve = byId("approve");
const hex = /^[a-f0-9]{64}$/;
const collectionName = /^[a-z][a-z0-9_-]{0,63}$/;
const reasons = {
  current_scoped_review: "Current scoped review",
  stale_scoped_context: "Context changed. Re-review required.",
  high_impact_predicate: "High-impact relationship",
  model_only: "Model proposal",
  untrusted_declaration_path: "Declaration requires review",
};

let view;
let selected;
let context;
let anchors = [];
let retry;
let operations;
let viewGeneration = 0;

function announce(text, error = false) {
  status.textContent = text;
  status.toggleAttribute("data-error", error);
}

function node(tag, text, css) {
  const el = document.createElement(tag);
  if (text !== undefined) el.textContent = text;
  if (css) el.className = css;
  return el;
}

function button(text, handler) {
  const el = node("button", text);
  el.type = "button";
  el.addEventListener("click", handler);
  return el;
}

function required(condition, message) {
  if (!condition) throw new Error(message);
}

async function request(path, body) {
  const generation = viewGeneration;
  const response = await fetch(path, {
    method: body === undefined ? "GET" : "POST",
    body: body === undefined ? undefined : JSON.stringify(body),
    headers: body === undefined ? { Accept: "application/json" } : {
      Accept: "application/json",
      "Content-Type": "application/json",
      "X-AnyRAG-Review": "1",
    },
    credentials: "same-origin",
    mode: "same-origin",
    redirect: "error",
    cache: "no-store",
    signal: AbortSignal.timeout(10000),
  });
  const payload = await response.json().catch(() => null);
  if (generation !== viewGeneration) {
    throw Object.assign(new Error("The dashboard view changed. Reopen the current evidence."), {
      status: 409, code: "GRAPH_REVIEW_STALE",
    });
  }
  if (response.ok && payload) return payload;
  const error = new Error("Request failed.");
  error.status = response.status;
  error.code = payload?.error?.code;
  if (error.status === 401 || error.code === "ACCESS_CHANGED") operations?.forget();
  throw error;
}

function clearDetail() {
  selected = undefined;
  context = undefined;
  anchors = [];
  retry = undefined;
  contextForm.reset();
  decisionForm.reset();
  for (const id of ["primary", "existing-context", "anchors", "context-text", "context-source"]) {
    byId(id).replaceChildren();
  }
  for (const id of ["detail", "approval", "context-preview"]) byId(id).hidden = true;
  updateApproval();
}

function clearQueue() {
  view = undefined;
  clearDetail();
  byId("proposals").replaceChildren();
  byId("queue").hidden = true;
  byId("next").hidden = true;
}

function updateApproval() {
  approve.disabled = !selected || anchors.length === 0 || !decisionForm.elements.confirm.checked;
}

async function task(operation) {
  if (workspace.disabled) return;
  workspace.disabled = true;
  try {
    await operation();
  } catch (error) {
    if (error.status === 401 || error.status === 403) {
      clearQueue();
      announce("Review access is unavailable. Check your session and collection grant.", true);
    } else if (error.status === 409 &&
               ["GRAPH_REVIEW_STALE", "ACCESS_CHANGED", "GRAPH_REVIEW_CONFLICT"].includes(error.code)) {
      clearQueue();
      announce("The review or access changed. Load the queue and reopen the current evidence.", true);
    } else {
      const safeStatus = {
        400: "The service rejected this review. Check the exact scope and anchors.",
        404: "This citation is unavailable in the current index or to your session.",
        409: "The service is busy. Try again.",
        503: "The service is unavailable. Try again.",
      };
      announce(safeStatus[error.status] || error.message || "Request failed.", true);
    }
  } finally {
    workspace.disabled = false;
    updateApproval();
  }
}

function facts(row) {
  return [
    node("h3", [row.subject, row.predicate, row.object].join(" → ")),
    node("p", reasons[row.reason] || row.reason || "Pending review", "metadata"),
    node("p", row.scope?.length ? "Setting " + row.scope.join(", ") : "Setting not assigned", "metadata"),
    node("blockquote", row.quote),
    node("p", row.path + " · Characters " + row.start + "–" + row.end, "metadata"),
    node("p", "Revision " + row.revision, "metadata"),
  ];
}

async function loadQueue(collection, scoped, after) {
  clearQueue();
  required(collectionName.test(collection), "Enter the collection name from your review profile.");
  const result = await request("/v1/graph-review/list", { collection, scoped, limit: 20, after });
  required(result.collection === collection && Array.isArray(result.proposals), "Unexpected review queue response.");
  view = { collection, scoped, next: result.next_cursor };
  const list = byId("proposals");
  for (const row of result.proposals) {
    const item = node("li");
    item.append(...facts(row), button("Reopen source", () => task(() => inspect(row))));
    list.append(item);
  }
  byId("queue").hidden = false;
  byId("next").hidden = !view.next;
  announce(result.proposals.length ? "" : "No reviews in this queue.");
}

async function reopen(id) {
  required(hex.test(id), "Enter a valid citation chunk ID.");
  const { evidence } = await request("/v1/evidence/" + id);
  required(evidence?.chunk_id === id && evidence.collection === view.collection &&
           hex.test(evidence.revision) && typeof evidence.text === "string" &&
           Number.isSafeInteger(evidence.start) && Number.isSafeInteger(evidence.end) &&
           evidence.start >= 0 && evidence.end - evidence.start === Array.from(evidence.text).length,
           "The citation is invalid or belongs to another collection.");
  return evidence;
}

async function inspect(row) {
  clearDetail();
  const evidence = await reopen(row.chunk_id);
  const characters = Array.from(evidence.text);
  required(evidence.revision === row.revision && evidence.path === row.path &&
           Number.isSafeInteger(row.start) && Number.isSafeInteger(row.end) &&
           row.start >= evidence.start && row.end <= evidence.end && row.start < row.end &&
           characters.slice(row.start - evidence.start, row.end - evidence.start).join("") === row.quote,
           "The primary quote no longer matches the reopened revision. Reload the queue.");
  selected = row;
  const route = routeDiagram([row]);
  if (route) byId("primary").append(route);
  byId("primary").append(...facts(row));
  for (const entry of row.context || []) {
    const item = node("li");
    item.append(
      node("p", entry.quote === null ? "Context retired" : "Indexed context", "metadata"),
      node("p", "Revision " + entry.anchor.revision + " · Characters " + entry.anchor.start + "–" + entry.anchor.end, "metadata"),
    );
    if (entry.quote !== null) item.append(node("blockquote", entry.quote));
    byId("existing-context").append(item);
  }
  byId("detail").hidden = false;
  byId("approval").hidden = view.scoped && row.reason !== "stale_scoped_context";
  decisionForm.elements.settings.value = (row.scope || []).join("\n");
  approve.textContent = view.scoped ? "Replace scoped review" : "Approve scoped relationship";
  byId("detail-heading").focus();
  announce("Primary quote reopened against the indexed revision.");
}

function selectedSpan(evidence, selection, full) {
  if (full) return { start: evidence.start, end: evidence.end, quote: evidence.text };
  required(selection.rangeCount === 1 && !selection.isCollapsed, "Select the exact setting quote in the reopened passage.");
  const range = selection.getRangeAt(0);
  required(range.startContainer === previewText.firstChild && range.endContainer === previewText.firstChild,
           "Select text inside the reopened passage.");
  const start = Array.from(evidence.text.slice(0, range.startOffset)).length;
  const end = Array.from(evidence.text.slice(0, range.endOffset)).length;
  const quote = Array.from(evidence.text).slice(start, end).join("");
  required(quote === range.toString(), "Select complete characters in the reopened passage.");
  return { start: evidence.start + start, end: evidence.start + end, quote };
}

async function addQuote(full) {
  required(context && anchors.length < 4, "Reopen a context citation. A review can use up to four quotes.");
  const { start, end, quote } = selectedSpan(context, document.getSelection(), full);
  required(end - start >= 1 && end - start <= 512 && quote.trim(),
           "Select a quote containing 1–512 Unicode characters.");
  const digest = await crypto.subtle.digest("SHA-256", new TextEncoder().encode(quote));
  const quote_sha256 = Array.from(new Uint8Array(digest), byte => byte.toString(16).padStart(2, "0")).join("");
  const { chunk_id, revision, path } = context;
  required(!anchors.some(item => item.anchor.chunk_id === chunk_id &&
                                 item.anchor.start === start && item.anchor.end === end),
           "This quote is already selected.");
  const item = { anchor: { chunk_id, revision, start, end, quote_sha256 }, quote, path };
  anchors.push(item);
  const row = node("li");
  row.append(node("blockquote", item.quote),
             node("p", path + " · Characters " + start + "–" + end + " · Revision " + revision, "metadata"),
             button("Remove quote", () => {
               anchors = anchors.filter(other => other !== item);
               row.remove();
               decisionForm.elements.confirm.checked = false;
               updateApproval();
             }));
  byId("anchors").append(row);
  decisionForm.elements.confirm.checked = false;
  announce("Exact context quote added.");
}

queueForm.addEventListener("submit", event => {
  event.preventDefault();
  task(() => loadQueue(queueForm.elements.collection.value.trim(), queueForm.elements.mode.value === "scoped"));
});
queueForm.addEventListener("change", () => {
  clearQueue();
  announce("Load the queue for this selection.");
});
byId("next").addEventListener("click", () => task(() => loadQueue(view.collection, view.scoped, view.next)));

contextForm.addEventListener("submit", event => {
  event.preventDefault();
  task(async () => {
    context = undefined;
    previewText.replaceChildren();
    byId("context-source").replaceChildren();
    byId("context-preview").hidden = true;
    context = await reopen(contextForm.elements.chunk.value.trim());
    previewText.textContent = context.text;
    byId("context-source").textContent = context.path + " · Revision " + context.revision;
    byId("context-preview").hidden = false;
    announce("Context citation reopened.");
  });
});
byId("use-selection").addEventListener("pointerdown", event => event.preventDefault());
byId("use-selection").addEventListener("click", () => task(() => addQuote(false)));
byId("use-passage").addEventListener("click", () => task(() => addQuote(true)));

decisionForm.elements.confirm.addEventListener("change", updateApproval);
decisionForm.elements.settings.addEventListener("input", () => {
  decisionForm.elements.confirm.checked = false;
  updateApproval();
});

decisionForm.addEventListener("submit", event => {
  event.preventDefault();
  task(async () => {
    required(selected && anchors.length && decisionForm.elements.confirm.checked &&
             (!view.scoped || selected.reason === "stale_scoped_context"),
             "Reopen and confirm the evidence before approving.");
    const scope = decisionForm.elements.settings.value.split("\n").map(value => value.trim());
    required(scope.length <= 8 && scope.every(tag => tag && Array.from(tag).length <= 96 && !/\p{Cc}/u.test(tag)) &&
             new Set(scope).size === scope.length, "Enter up to eight distinct setting tags, one per line.");
    const { proposal_id, revision, fingerprint, policy_version } = selected;
    const payload = {
      collection: view.collection, proposal_id, revision, fingerprint, policy_version, decision: "approve",
      applicability: { scope, context: anchors.map(item => item.anchor) },
      ...(view.scoped ? { replace_scope: true } : {}),
    };
    const key = JSON.stringify(payload);
    if (retry?.key !== key) retry = { key, id: crypto.randomUUID() };
    const result = await request("/v1/graph-review/decide", { ...payload, idempotency_key: retry.id });
    required(result.status === "approved_scoped", "The service did not confirm scoped approval.");
    clearQueue();
    announce("Scoped approval recorded. Load the queue to see its current state.");
  });
});

workspace.disabled = !window.isSecureContext || !globalThis.crypto?.subtle;
if (workspace.disabled) announce("Open this page through your private HTTPS gateway.", true);
const lookupWorkspace = byId("lookup-workspace");
const lookupForm = byId("lookup-form");
const lookupStatus = byId("lookup-status");
const lookupResult = byId("lookup-result");
const lookupPreview = byId("lookup-preview");
let pendingLookup;

function clearLookup() {
  pendingLookup = undefined;
  lookupPreview.hidden = true;
  lookupResult.replaceChildren();
  lookupStatus.textContent = "";
}

async function lookupTask(operation) {
  if (lookupWorkspace.disabled) return;
  lookupWorkspace.disabled = true;
  try {
    await operation();
  } catch (error) {
    clearLookup();
    lookupStatus.textContent = error.code === "RELATIONSHIP_CHANGED"
      ? "The lookup changed. Preview it again before confirming."
      : error.status ? "The lookup could not be completed. Check your access and request." : error.message;
  } finally {
    lookupWorkspace.disabled = false;
    if (!lookupPreview.hidden) byId("lookup-confirm").focus();
  }
}

function addLookupCitation(text, chunk) {
  if (!hex.test(chunk)) return;
  const link = node("a", text);
  link.href = "/v1/evidence/" + chunk;
  const row = node("p");
  row.append(link);
  lookupResult.append(row);
}

lookupForm.addEventListener("input", clearLookup);
lookupForm.addEventListener("submit", event => {
  event.preventDefault();
  lookupTask(async () => {
    clearLookup();
    const root = lookupForm.elements.root.value.trim();
    const path_predicates = lookupForm.elements.predicates.value.split(",").map(value => value.trim());
    required(root && path_predicates.length <= 3 && path_predicates.every(value => value), "Enter an entity and one to three ordered relationship types.");
    const via = lookupForm.elements.via.value.split("\n").map(value => value.trim() || null);
    required(via.every(value => value === null) || via.length <= path_predicates.length - 1,
      "Enter only the intermediate positions that fit this chain.");
    const setting = lookupForm.elements.scope.value.trim();
    const requestBody = { root, path_predicates, terminal_predicates: path_predicates.slice(-1),
      min_steps: path_predicates.length, max_steps: path_predicates.length, scope: setting ? [setting] : [] };
    const alias = lookupForm.elements.rootAlias.value.trim();
    if (alias) requestBody.root_alias = alias;
    if (via.some(Boolean)) requestBody.via = path_predicates.slice(1).map((_, index) => via[index] ?? null);
    const response = await request("/v1/relationship/guided", { request: requestBody });
    if (response.status !== "confirm_relationship") {
      lookupStatus.textContent = response.status === "ambiguous" ? "Several current lookups match. Refine the entity or setting." : "No single current result matches this lookup.";
      return;
    }
    const confirmed = response.interpretation;
    pendingLookup = { request: requestBody, confirmed };
    const chain = [confirmed.root, ...confirmed.path_predicates.flatMap((type, index) => {
      const role = type.replaceAll("_", " ");
      return confirmed.via?.[index] ? [role, confirmed.via[index]] : [role];
    })].join(" → ");
    byId("lookup-interpretation").textContent = confirmed.scope.length
      ? chain + " · Setting " + confirmed.scope.join(", ") : chain;
    const identity = response.root_identity;
    byId("lookup-identity").textContent = identity?.kind === "reviewed_alias"
      ? "Reviewed name " + identity.alias + " → " + identity.canonical
      : "Starting entity supplied by the caller.";
    lookupPreview.hidden = false;
  });
});
byId("lookup-confirm").addEventListener("click", () => lookupTask(async () => {
  if (!pendingLookup) return;
  const payload = pendingLookup;
  clearLookup();
  const result = await request("/v1/relationship/guided", payload);
  lookupStatus.textContent = ({value: result.value, explicit_negative: "The source explicitly denies this relationship.",
    documented_absence: "The cited record documents the absence of a value."})[result.status] || "No supported result.";
  const route = routeDiagram(result.steps);
  if (route) lookupResult.append(route);
  for (const step of result.steps ?? []) {
    lookupResult.append(node("blockquote", step.quote));
    addLookupCitation("Reopen relationship citation", step.citation?.chunk_id);
    for (const binding of step.bindings ?? []) addLookupCitation("Reopen setting citation", binding.chunk_id);
  }
  if (result.fact) {
    addLookupCitation("Reopen documented fact", result.fact.primary?.chunk_id);
    for (const binding of result.fact.context ?? []) addLookupCitation("Reopen fact context", binding.chunk_id);
  }
  for (const binding of result.name_bindings ?? []) addLookupCitation("Reopen name binding", binding.chunk_id);
}));
lookupWorkspace.disabled = workspace.disabled;

function clearReviewedEvidence() {
  viewGeneration++;
  clearQueue();
  clearLookup();
  queueForm.reset();
  lookupForm.reset();
  lookupStatus.textContent = "";
  announce("");
}

function openReview(collection, scoped) {
  queueForm.elements.collection.value = collection;
  queueForm.elements.mode.value = scoped ? "scoped" : "pending";
  return task(() => loadQueue(collection, scoped));
}

const diagnostics = mountDiagnostics(request, {
  onTrace: result => operations?.record("trace", result),
  onCitation: result => operations?.record("access", result),
  openSources: () => operations?.showView("sources"),
  onAccessChanged: () => operations?.forget(),
});
operations = mountOperations({ request, openReview, clearReviewedEvidence, diagnostics });

window.addEventListener("pageshow", event => {
  if (event.persisted) {
    operations.forget();
    operations.refresh();
    announce("Load the queue to check current access.");
  }
});
