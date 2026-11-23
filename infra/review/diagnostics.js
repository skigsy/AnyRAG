const citationId = /^[a-f0-9]{64}$/;
const modes = { lexical: "Keyword", vector: "Vector", hybrid: "Hybrid" };
const outcomes = { evidence: "Evidence returned", weak_match: "Weak match", abstained: "No evidence returned" };
const omissions = {
  context_budget: "Context budget",
  item_limit: "Evidence item limit",
  missing_id_or_text: "Missing ID or passage",
  duplicate: "Duplicate evidence",
};

function element(tag, text, className) {
  const node = document.createElement(tag);
  if (text !== undefined) node.textContent = text;
  if (className) node.className = className;
  return node;
}

function field(caption, control) {
  const label = element("label", caption);
  label.htmlFor = control.id;
  label.append(control);
  return label;
}

function input(id, attributes) {
  return Object.assign(element("input"), { id }, attributes);
}

function action(text, handler) {
  const button = Object.assign(element("button", text), { type: "button" });
  button.addEventListener("click", handler);
  return button;
}

function panel(host, title, caption) {
  const section = element("section", undefined, "panel diagnostic-panel");
  const status = element("p", undefined, "diagnostic-status");
  status.setAttribute("role", "status");
  const form = Object.assign(element("form", undefined, "diagnostic-form"), { autocomplete: "off" });
  const result = element("div", undefined, "diagnostic-result");
  section.append(element("h2", title), element("p", caption, "metadata"), form, status, result);
  host.append(section);
  return { status, form, result };
}

function message(panel, text, tone = "neutral") {
  panel.status.textContent = text;
  panel.status.dataset.tone = tone;
}

function known(map, value, fallback) {
  return Object.hasOwn(map, value) ? map[value] : fallback;
}

function records(value) {
  return Array.isArray(value) ? value : [];
}

function evidenceIdentity(row) {
  if (typeof row?.chunk_id !== "string" || !citationId.test(row.chunk_id)) return undefined;
  return {
    id: row.chunk_id,
    collection: typeof row.collection === "string" ? row.collection : "",
    revision: typeof row.revision === "string" ? row.revision : "",
    path: typeof row.path === "string" ? row.path : "",
  };
}

function traceSnapshot(payload) {
  const answer = payload.answer;
  const rawDelivery = answer?.delivery;
  const sent = records(rawDelivery?.sent);
  const omitted = records(rawDelivery?.omitted);
  const cited = new Set(records(answer?.citations));
  const candidates = records(payload.evidence).map(evidenceIdentity).filter(Boolean);
  const groups = Object.entries(payload.graph_paths ?? {}).flatMap(([collection, group]) => {
    const paths = records(group?.paths).flatMap(path => {
      if (!Array.isArray(path) || path.length < 2 || path.length > 3) return [];
      const steps = path.map(row => {
        const identity = evidenceIdentity({ ...row, collection });
        const assertion = row?.assertion;
        if (!identity || ![assertion?.subject, assertion?.predicate, assertion?.object].every(part =>
          typeof part === "string" && part.length > 0)) return undefined;
        const candidate = candidates.find(item =>
          item.id === identity.id && item.collection === collection && item.revision === identity.revision);
        const admitted = sent.some(item =>
          item.chunk_id === identity.id && item.collection === collection && item.revision === identity.revision);
        const omission = omitted.find(item =>
          item.chunk_id === identity.id && item.collection === collection && item.revision === identity.revision);
        let stage = rawDelivery ? "No delivery recorded" : "Retrieval evidence";
        let tone = "neutral";
        if (admitted) {
          stage = cited.has(identity.id) ? "Cited by answer" : "Sent to model";
          tone = "good";
        } else if (omission) {
          stage = "Omitted · " + known(omissions, omission.reason, "Unspecified reason");
          tone = "warn";
        }
        return {
          identity: { ...identity, path: candidate?.path ?? "" },
          direction: [assertion.subject, assertion.predicate, assertion.object].join(" → "),
          stage,
          tone,
        };
      });
      return steps.every(Boolean) ? [steps] : [];
    });
    return paths.length || group?.truncated
      ? [{ collection, paths, truncated: group.truncated === true }]
      : [];
  });
  const omissionCounts = new Map();
  for (const item of omitted) {
    const label = known(omissions, item.reason, "Unspecified reason");
    omissionCounts.set(label, (omissionCounts.get(label) ?? 0) + 1);
  }
  return {
    candidates,
    groups,
    requested: known(modes, payload.mode, "Unknown"),
    effective: known(modes, payload.effective_mode ?? payload.mode, "Unknown"),
    outcome: known(outcomes, payload.status, "Query completed"),
    graph: payload.graph === true,
    rebuilding: Object.values(payload.graph_coverage ?? {}).some(item => item?.status === "publication_reconciling"),
    delivery: rawDelivery ? { sent: sent.length, omitted: omitted.length, omissionCounts } : undefined,
    cited: Array.isArray(answer?.citations) ? cited.size : undefined,
    answerUnavailable: answer?.status === "unavailable",
  };
}

function traceSummary(trace) {
  const omitted = trace.delivery?.omitted ?? 0;
  if (omitted > 0) {
    const context = trace.delivery.omissionCounts.get(omissions.context_budget) ?? 0;
    return {
      title: "Evidence omitted from the answer context",
      detail: context > 0
        ? `${context} omitted for context budget · ${omitted} omitted in total`
        : `${omitted} retrieved passages omitted`,
      tone: "warn",
    };
  }
  return {
    title: trace.outcome,
    detail: `${trace.candidates.length} retrieved passages · ${trace.effective} search`,
    tone: trace.answerUnavailable || trace.rebuilding ? "warn" : "neutral",
  };
}

function showTrace(host, trace, collection, reopen) {
  const metrics = element("dl", undefined, "diagnostic-metrics ops-evidence-flow");
  for (const [label, value] of [
    ["Mode", trace.requested === trace.effective ? trace.effective : `${trace.requested} → ${trace.effective}`],
    ["Retrieved", trace.candidates.length],
    ["Sent", trace.delivery?.sent ?? "—"],
    ["Omitted", trace.delivery?.omitted ?? "—"],
    ["Cited", trace.cited ?? "—"],
  ]) {
    const item = element("div");
    item.append(element("dt", label), element("dd", value));
    metrics.append(item);
  }
  host.replaceChildren(metrics);
  if (trace.rebuilding) host.append(element("p", "Graph publication is reconciling for at least one granted collection.", "metadata"));
  if (trace.answerUnavailable) host.append(element("p", "The configured endpoint did not return a usable answer.", "metadata"));
  if (trace.delivery?.omissionCounts.size) {
    const summary = [...trace.delivery.omissionCounts].map(([label, count]) => `${label} · ${count}`);
    host.append(element("p", summary.join("  /  "), "diagnostic-omissions"));
  }

  const groups = trace.groups.filter(group => !collection || group.collection === collection);
  host.append(element("h3", collection ? `Graph paths · ${collection}` : "Graph paths"));
  if (!groups.some(group => group.paths.length)) {
    host.append(element("p", trace.graph
      ? "No ordered multi-hop path was returned in this view."
      : "This query ran without graph traversal.", "metadata"));
  }
  for (const group of groups) {
    const block = element("section", undefined, "diagnostic-collection");
    if (!collection) block.append(element("h4", group.collection));
    for (const [offset, steps] of group.paths.slice(0, 4).entries()) {
      const path = element("ol", undefined, "diagnostic-path");
      path.setAttribute("aria-label", `Directed path ${offset + 1} in ${group.collection}`);
      for (const step of steps) {
        const hop = element("li", undefined, "diagnostic-hop");
        const stage = element("span", step.stage, "diagnostic-stage");
        stage.dataset.tone = step.tone;
        hop.append(element("strong", step.direction), stage);
        if (step.identity.path) hop.append(element("span", step.identity.path, "metadata"));
        hop.append(action("Check citation", () => reopen(step.identity)));
        path.append(hop);
      }
      block.append(path);
    }
    if (group.paths.length > 4 || group.truncated) {
      block.append(element("p", "Path preview is limited.", "metadata"));
    }
    host.append(block);
  }

  const candidates = trace.candidates.filter(item => !collection || item.collection === collection);
  if (candidates.length) {
    const details = element("details", undefined, "diagnostic-candidates");
    const list = element("ul");
    details.append(element("summary", `Candidate passages · ${candidates.length}`), list);
    for (const item of candidates) {
      const row = element("li");
      row.append(element("span", [item.collection, item.path || item.id.slice(0, 12)].filter(Boolean).join(" · ")),
        action("Check citation", () => reopen(item)));
      list.append(row);
    }
    host.append(details);
  }
}

function showCitation(host, evidence) {
  const identity = evidenceIdentity(evidence);
  const values = [
    ["Collection", identity.collection],
    ["Source", identity.path],
    ["Revision", identity.revision],
    ["Chunk", identity.id],
    ["Page", evidence.page ?? ""],
    ["Offsets", Number.isInteger(evidence.start) && Number.isInteger(evidence.end)
      ? `${evidence.start}–${evidence.end}` : ""],
  ];
  const fields = element("dl", undefined, "diagnostic-metrics diagnostic-citation-metadata");
  for (const [label, value] of values) {
    if (value === "") continue;
    const item = element("div");
    item.append(element("dt", label), element("dd", value));
    fields.append(item);
  }
  host.replaceChildren(fields);
  if (typeof evidence.text === "string" && evidence.text.length) {
    const detail = element("details", undefined, "diagnostic-excerpt");
    detail.append(element("summary", "Read indexed passage"), element("pre", evidence.text));
    host.append(detail);
  }
}

/** Mount in the shared console's Activity and Sources views, using its same-origin request. */
export function mountDiagnostics(request, {
  onTrace = () => {}, onCitation = () => {}, openSources = () => {}, onAccessChanged = () => {},
} = {}) {
  const activity = panel(document.getElementById("activity-diagnostics"), "Evidence trace",
    "Totals cover your granted collections. Collection selection filters paths and passages.");
  const sources = panel(document.getElementById("citation-diagnostics"), "Check a citation",
    "Reopen an indexed citation under your current session.");
  activity.status.id = "trace-status";
  sources.status.id = "citation-status";

  const question = input("trace-question", { name: "question", required: true, maxLength: 4000 });
  const mode = Object.assign(element("select"), { id: "trace-mode", name: "mode" });
  for (const [value, label] of Object.entries(modes)) mode.append(Object.assign(element("option", label), { value }));
  const endpoint = input("trace-answer-endpoint", { name: "answer-endpoint", maxLength: 64 });
  const limit = input("trace-limit", { name: "limit", type: "number", min: 1, max: 20, defaultValue: "5", required: true });
  const traceSubmit = Object.assign(element("button", "Run diagnostic"), { type: "submit" });
  activity.form.append(field("Question", question), field("Retrieval mode", mode),
    field("Result limit", limit), traceSubmit,
    element("p", "Vector and hybrid may use the configured embedding provider.", "metadata"));
  const modelOptions = element("details", undefined, "diagnostic-model-options");
  modelOptions.append(element("summary", "Configured answer endpoint"),
    field("Endpoint name", endpoint),
    element("p", "A named answer endpoint can receive the question and selected passages.", "metadata"));
  activity.form.append(modelOptions);
  const chunk = input("citation-chunk", { name: "chunk", maxLength: 64, pattern: "[0-9a-fA-F]{64}", required: true });
  const citationSubmit = Object.assign(element("button", "Check citation"), { type: "submit" });
  sources.form.append(field("Chunk ID", chunk), citationSubmit);

  let collection = "";
  let trace;
  let traceVersion = 0;
  let citationVersion = 0;

  function clear() {
    traceVersion += 1;
    citationVersion += 1;
    trace = undefined;
    activity.form.reset();
    sources.form.reset();
    for (const view of [activity, sources]) {
      view.result.replaceChildren();
      message(view, "");
    }
    traceSubmit.disabled = false;
    citationSubmit.disabled = false;
  }

  function changedAccess(error, view) {
    if (error.status !== 401 && error.code !== "ACCESS_CHANGED") return false;
    clear();
    onAccessChanged();
    message(view, "Session or collection access changed. Sign in and run the check again.", "warn");
    return true;
  }

  async function checkCitation(id, expected) {
    if (!citationId.test(id)) {
      message(sources, "Enter a 64-character chunk ID from an indexed citation.", "warn");
      return;
    }
    const version = ++citationVersion;
    citationSubmit.disabled = true;
    chunk.value = id;
    sources.result.replaceChildren();
    message(sources, "Checking the current citation…");
    try {
      const { evidence } = await request("/v1/evidence/" + id);
      if (version !== citationVersion) return;
      const identity = evidenceIdentity(evidence);
      if (!identity || identity.id !== id) throw new Error("Invalid evidence identity.");
      if ((expected?.revision && expected.revision !== identity.revision) ||
          (expected?.collection && expected.collection !== identity.collection)) {
        message(sources, "The citation resolves to a different collection or revision. Rerun the trace.", "warn");
        onCitation({ title: "Citation identity changed", detail: "The current response did not match the trace.", tone: "warn" });
        return;
      }
      showCitation(sources.result, evidence);
      message(sources, "Citation reopened.", "good");
      onCitation({ title: "Citation reopened", detail: "Current session · indexed citation", tone: "good" });
    } catch (error) {
      if (version !== citationVersion || changedAccess(error, sources)) return;
      if (error.status === 404 || error.status === 403) {
        message(sources, "Citation unavailable in the current index or to this session.", "warn");
        onCitation({ title: "Citation unavailable", detail: "Current index or session access", tone: "warn" });
      } else {
        message(sources, "Citation check failed. Retry once the service is ready.", "warn");
        onCitation({ title: "Citation check failed", detail: "No access result confirmed", tone: "warn" });
      }
    } finally {
      if (version === citationVersion) citationSubmit.disabled = false;
    }
  }

  function reopen(identity) {
    openSources();
    checkCitation(identity.id, identity);
  }

  async function runTrace(event) {
    event.preventDefault();
    if (traceSubmit.disabled) return;
    const text = question.value.trim();
    if (!text) {
      message(activity, "Enter a search question.", "warn");
      return;
    }
    const name = endpoint.value.trim();
    const requestBody = { question: text, mode: mode.value, limit: Number(limit.value) };
    if (name) requestBody.answer_endpoint = name;
    const version = ++traceVersion;
    question.value = "";
    traceSubmit.disabled = true;
    trace = undefined;
    activity.result.replaceChildren();
    message(activity, "Reading evidence…");
    try {
      const response = await request("/v1/query", requestBody);
      if (version !== traceVersion) return;
      if (response?.schema_version !== 1 || !Array.isArray(response.evidence)) throw new Error("Invalid query response.");
      trace = traceSnapshot(response);
      const summary = traceSummary(trace);
      showTrace(activity.result, trace, collection, reopen);
      message(activity, trace.outcome + ".", summary.tone);
      onTrace(summary);
    } catch (error) {
      if (version !== traceVersion || changedAccess(error, activity)) return;
      const reason = {
        EMBEDDING_UNAVAILABLE: "This deployment has no embedding provider for that retrieval mode.",
        MODEL_ENDPOINT: "Choose an answer endpoint configured for this deployment.",
        MODEL_DESTINATION_DENIED: "Current policy does not allow processing at the selected endpoint.",
      };
      message(activity, known(reason, error.code, "Evidence trace could not be loaded. Retry once the service is ready."), "warn");
      onTrace({ title: "Evidence trace failed", detail: "No diagnostic result confirmed", tone: "warn" });
    } finally {
      if (version === traceVersion) traceSubmit.disabled = false;
    }
  }

  activity.form.addEventListener("submit", runTrace);
  sources.form.addEventListener("submit", event => {
    event.preventDefault();
    checkCitation(chunk.value.trim().toLowerCase());
  });
  return {
    clear,
    setCollection(name) {
      const next = typeof name === "string" ? name : "";
      if (collection === next) return;
      collection = next;
      if (trace) showTrace(activity.result, trace, collection, reopen);
    },
  };
}
