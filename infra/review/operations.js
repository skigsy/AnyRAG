const names = {
  overview: ["Overview", "Index health and evidence checks"],
  search: ["Search", "Find and reopen passages in collections you can read"],
  coverage: ["Coverage", "Observe extraction and indexing by collection"],
  sources: ["Sources", "Reopen a current citation with your signed-in access"],
  activity: ["Activity", "Follow a request from retrieval to evidence"],
  graph: ["Graph review", "Inspect directed links and the revisions that support them"],
};

const laneCounts = [
  ["Ready", "ready", "gold"], ["Leased", "leased", "cool"],
  ["Deferred", "deferred", "warm"], ["Quarantined", "quarantined", "alert"],
];

const noTrace = {
  title: "No trace run", detail: "Run a trace to inspect retrieved and delivered evidence.", tone: "neutral",
};

const noCitation = {
  title: "No check run", detail: "Check whether a citation reopens with your current access.", tone: "neutral",
};

const scopeName = /^[a-z][a-z0-9_-]{0,63}$/;
const integer = new Intl.NumberFormat();

function element(tag, value, style) {
  const item = document.createElement(tag);
  if (value !== undefined) item.textContent = value;
  if (style) item.className = style;
  return item;
}

function metric(value) {
  return Number.isSafeInteger(value) && value >= 0 ? integer.format(value) : "—";
}

function utc(seconds) {
  const date = new Date(seconds * 1000);
  return Number.isSafeInteger(seconds) && seconds > 0 && Number.isFinite(date.getTime())
    ? date.toISOString().slice(11, 19) + " UTC" : "—";
}

function approximate(value, more) {
  return metric(value) + (more && Number.isSafeInteger(value) ? "+" : "");
}

function chip(label, tone) {
  const item = element("span", label, "ops-chip");
  item.dataset.tone = tone;
  return item;
}

function ledger(rows) {
  const list = element("dl", undefined, "ops-facts");
  for (const [name, value] of rows) {
    const row = element("div");
    row.append(element("dt", name), element("dd", value));
    list.append(row);
  }
  return list;
}

function meters(rows) {
  const list = element("dl", undefined, "ops-meters");
  const max = Math.max(1, ...rows.map(([, value]) => value ?? 0));
  for (const [name, value, tone, more] of rows) {
    const line = element("div");
    line.dataset.ink = tone;
    line.append(element("dt", name), element("dd", approximate(value, more)));
    if (Number.isSafeInteger(value) && value >= 0) {
      const bar = element("meter");
      bar.min = 0;
      bar.max = max;
      bar.value = value;
      bar.setAttribute("aria-label", name);
      line.append(bar);
    }
    list.append(line);
  }
  return list;
}

function laneState(lane) {
  if (!lane) return ["Off", "neutral"];
  if (lane.publication_current === false) return ["Reconciling", "warn"];
  if (lane.pending > 0) return [metric(lane.pending) + " pending", "warn"];
  if (lane.pending === 0) return ["No pending work", "good"];
  return ["Pending unknown", "neutral"];
}

function reviewState(review) {
  if (!review) return ["Review unavailable", "neutral"];
  if (review.status === "reconciling") return ["Review reconciling", "warn"];
  if (review.scoped_rechecks > 0) {
    const one = review.scoped_rechecks === 1 && !review.scoped_more;
    return [approximate(review.scoped_rechecks, review.scoped_more) + (one ? " needs re-review" : " need re-review"), "warn"];
  }
  if (review.pending_proposals > 0) {
    const one = review.pending_proposals === 1 && !review.pending_more;
    return [approximate(review.pending_proposals, review.pending_more) + (one ? " pending proposal" : " pending proposals"), "warn"];
  }
  if (review.pending_more || review.scoped_more) return ["More reviews available", "neutral"];
  return ["No review work in the current sample", "good"];
}

function syncState(snapshot) {
  if (snapshot?.restore_pending) return ["Restore reconciliation pending", "warn"];
  if (snapshot?.maintenance) return ["Maintenance window", "warn"];
  if (snapshot?.sync?.status === "running") return ["Sync running", "neutral"];
  if (snapshot?.sync?.status === "complete") return ["Sync complete", "good"];
  if (snapshot?.sync?.status === "partial") return ["Sync partial", "warn"];
  if (snapshot?.sync?.status === "incomplete") return ["Sync incomplete", "warn"];
  if (["failed", "error"].includes(snapshot?.sync?.status)) return ["Sync failed", "warn"];
  return [snapshot?.maintenance === false ? "No sync observed" : "Admin sync details unavailable", "neutral"];
}

function lanePanel(title, lane) {
  const [label, tone] = laneState(lane);
  const panel = element("section", undefined, "panel ops-lane");
  const header = element("header", undefined, "ops-panel-head");
  header.append(element("h2", title), chip(label, tone));
  panel.append(header);
  if (lane) {
    panel.append(meters(laneCounts.map(([label, field, ink]) => [label, lane[field], ink])));
    const schedule = [];
    if (lane.next_retry_unix) schedule.push(["Next retry", utc(lane.next_retry_unix)]);
    if (lane.provider_paused_until_unix) schedule.push(["Provider pause until", utc(lane.provider_paused_until_unix)]);
    if (schedule.length) panel.append(ledger(schedule));
  } else {
    panel.append(element("p", "This enrichment lane is off for the collection.", "metadata"));
  }
  return panel;
}

const queueSeries = [
  ["ready", "Ready", "--queue-ready"],
  ["leased", "Leased", "--queue-leased"],
  ["deferred", "Deferred", "--queue-deferred"],
  ["quarantined", "Quarantined", "--queue-quarantined"],
  ["other", "Other pending", "--queue-other"],
];

function plottedCount(value) {
  return Number.isSafeInteger(value) && value >= 0 ? value : null;
}

function chartBar(label, value, max, kind) {
  const row = element("div", undefined, "ops-volume-line");
  row.dataset.series = kind;
  const count = element("span", metric(value));
  const meter = element("meter");
  meter.min = 0;
  meter.max = max;
  meter.value = value;
  meter.setAttribute("aria-label", label);
  row.append(meter, count);
  return row;
}

function corpusChart(collections, selected, choose) {
  const root = element("div", undefined, "ops-corpus-chart");
  const ranked = collections.filter(item =>
    plottedCount(item.sources) !== null && plottedCount(item.passages) !== null
  ).sort((left, right) => right.passages - left.passages || left.name.localeCompare(right.name));
  if (!ranked.length) {
    root.append(element("p", "No collection inventory available.", "metadata"));
    return root;
  }

  const plotted = ranked.slice(0, 6);
  const selectedRow = ranked.find(item => item.name === selected);
  if (selectedRow && !plotted.includes(selectedRow)) plotted[plotted.length - 1] = selectedRow;
  const scale = Math.max(1, ...plotted.flatMap(item => [item.sources, item.passages]));
  const legend = element("div", undefined, "ops-series-legend");
  for (const [label, color] of [["Indexed sources", "sources"], ["Searchable passages", "passages"]]) {
    const key = element("span", label);
    key.dataset.series = color;
    legend.append(key);
  }
  root.append(legend);
  const axis = element("div", undefined, "ops-volume-axis");
  axis.setAttribute("aria-hidden", "true");
  axis.append(element("span", "0"));
  if (scale > 1) axis.append(element("span", integer.format(scale / 2)));
  axis.append(element("span", metric(scale)));
  root.append(axis);
  for (const item of plotted) {
    const row = element("div", undefined, "ops-volume-row");
    const label = element("button", item.name, "ops-volume-pick");
    label.type = "button";
    label.dataset.collection = item.name;
    label.setAttribute("aria-pressed", String(item.name === selected));
    label.addEventListener("click", () => choose(item.name, true, true));
    const lines = element("div", undefined, "ops-volume-lines");
    lines.append(chartBar(item.name + " indexed sources", item.sources, scale, "sources"));
    lines.append(chartBar(item.name + " searchable passages", item.passages, scale, "passages"));
    row.append(label, lines);
    root.append(row);
  }
  if (plotted.length < ranked.length) {
    root.append(element("p", plotted.length + " of " + ranked.length + " collections · largest and selected", "ops-chart-meta"));
  }
  return root;
}

function drawQueue(canvas, lane, total) {
  const ctx = canvas.getContext("2d");
  if (!ctx) return;
  const style = getComputedStyle(document.documentElement);
  const ink = token => style.getPropertyValue(token).trim();
  const center = canvas.width / 2;
  const radius = center - 24;
  const turn = Math.PI * 2;
  ctx.lineWidth = 18;
  ctx.strokeStyle = ink("--queue-track");
  ctx.beginPath();
  ctx.arc(center, center, radius, 0, turn);
  ctx.stroke();

  if (total === null || total === 0) return;
  const known = queueSeries.slice(0, -1).map(([key]) => plottedCount(lane[key]));
  if (known.includes(null)) return;
  const categorized = known.reduce((sum, count) => sum + count, 0);
  if (categorized > total) return;
  const counts = [...known, total - categorized];
  let angle = -Math.PI / 2;
  for (const [index, count] of counts.entries()) {
    if (count === 0) continue;
    const end = angle + turn * count / total;
    const gap = count === total ? 0 : Math.min(.035, (end - angle) / 5);
    ctx.beginPath();
    ctx.strokeStyle = ink(queueSeries[index][2]);
    ctx.arc(center, center, radius, angle + gap, end - gap);
    ctx.stroke();
    angle = end;
  }
}

function queueRing(label, lane) {
  const card = element("section", undefined, "ops-ring-tile");
  const title = element("h3", label);
  const total = plottedCount(lane?.pending);
  const plot = element("div", undefined, "ops-ring");
  const canvas = element("canvas", undefined, "ops-ring-canvas");
  canvas.width = 240;
  canvas.height = 240;
  canvas.setAttribute("aria-hidden", "true");
  const value = element("div", undefined, "ops-ring-values");
  value.append(element("strong", metric(total)), element("span", total !== null ? "pending" : lane === null ? "off" : "unknown"));
  plot.append(canvas, value);
  card.append(title, plot);
  if (total > 0) {
    const breakdown = queueSeries.slice(0, -1)
      .filter(([key]) => plottedCount(lane[key]) !== null)
      .map(([key, name]) => metric(lane[key]) + " " + name.toLowerCase());
    const residual = total - queueSeries.slice(0, -1).reduce((sum, [key]) => sum + (plottedCount(lane[key]) ?? 0), 0);
    if (residual > 0) breakdown.push(metric(residual) + " other pending");
    card.append(element("p", breakdown.join(" · "), "ops-ring-detail"));
  }
  drawQueue(canvas, lane, total);
  return card;
}

export function routeDiagram(steps) {
  if (!Array.isArray(steps) || !steps.length) return null;
  const edges = steps.map(step => step?.assertion ?? step);
  if (edges.some(edge => !edge || [edge.subject, edge.predicate, edge.object]
    .some(value => typeof value !== "string" || !value.trim()))) return null;
  const diagram = element("div", undefined, "ops-route-diagram");
  diagram.setAttribute("role", "group");
  diagram.setAttribute("aria-label", "Directed relationships");
  let previous;
  let strip;
  for (const {subject, predicate, object} of edges) {
    if (previous !== subject) {
      strip = element("div", undefined, "ops-route-strip");
      strip.append(element("strong", subject, "ops-route-node"));
      diagram.append(strip);
    }
    const connector = element("div", undefined, "ops-route-edge");
    connector.append(element("span", predicate.replaceAll("_", " ")));
    strip.append(connector, element("strong", object, "ops-route-node"));
    previous = object;
  }
  return diagram;
}

export function mountOperations({ request, openReview, clearReviewedEvidence, diagnostics }) {
  const byId = id => document.getElementById(id);
  const selector = byId("ops-collection");
  const selectorLabel = byId("ops-collection-label");
  const searchButton = byId("ops-start-search");
  const connection = byId("ops-connection");
  const watch = byId("ops-watchlist");
  const refreshButton = byId("ops-refresh");
  const panels = [...document.querySelectorAll("[data-panel]")];
  const nav = [...document.querySelectorAll(".ops-nav [data-view]")];
  let snapshot;
  let selected = "";
  let active = "freshness";
  let results = { trace: noTrace, access: noCitation };
  let timer;
  let inFlight = false;
  let autoPoll = true;
  let generation = 0;
  let connectionText;
  let connectionTone;

  function setConnection(text, tone) {
    if (connectionText !== text) connection.textContent = text;
    if (connectionTone !== tone) connection.dataset.tone = tone;
    connectionText = text;
    connectionTone = tone;
  }

  function showView(key, focus = true) {
    const [title, summary] = names[key];
    for (const panel of panels) panel.hidden = panel.dataset.panel !== key;
    for (const button of nav) {
      if (button.dataset.view === key) button.setAttribute("aria-current", "page");
      else button.removeAttribute("aria-current");
    }
    selector.hidden = key === "search";
    selectorLabel.hidden = selector.hidden;
    searchButton.hidden = key !== "overview";
    byId("ops-view-title").textContent = title;
    byId("ops-view-summary").textContent = summary;
    if (focus) byId("ops-view-title").focus();
  }

  function collection() {
    return snapshot?.collections.find(item => item.name === selected);
  }

  const records = [
    ["freshness", "Collection freshness", "Open coverage", () => showView("coverage")],
    ["trace", "Query evidence", "Open evidence trace", () => showView("activity")],
    ["review", "Graph review", "Open Graph review", () => {
      showView("graph");
      const item = collection();
      if (item?.review?.status === "current") openReview(item.name, item.review.scoped_rechecks > 0);
    }],
    ["access", "Citation access", "Open citation checks", () => showView("sources")],
  ].map(([key, label, action, open]) => {
    const button = element("button", undefined, "ops-watch-item");
    button.type = "button";
    button.dataset.watch = key;
    const title = element("span", label, "ops-watch-title");
    const state = element("strong", undefined, "ops-watch-value");
    const detail = element("small");
    button.append(title, state, detail);
    button.addEventListener("click", () => {
      active = key;
      renderWatch();
    });
    watch.append(button);
    return { key, label, action, open, button, state, detail };
  });

  function freshness(item) {
    if (!item) return { title: "No collection selected", detail: "Collection status appears when available to your session.", tone: "neutral" };
    const [graph, graphTone] = laneState(item.graph);
    const [vector, vectorTone] = laneState(item.vector);
    const inventoryPending = snapshot?.sync?.inventory_reconciled === false;
    const busy = graphTone === "warn" || vectorTone === "warn" || inventoryPending;
    const inventory = inventoryPending ? " · Shared sync incomplete" : "";
    return { title: busy ? "Work pending" : "Observed", detail: item.name + " · Graph extraction " + graph + " · Vector " + vector + inventory, tone: busy ? "warn" : "good" };
  }

  function renderWatch() {
    const item = collection();
    const [review, reviewTone] = reviewState(item?.review);
    const signals = {
      freshness: freshness(item),
      trace: results.trace,
      review: { title: review, detail: item?.review ? "Current evidence and review scope." : "Open the signed review workspace to check your grant.", tone: reviewTone },
      access: results.access,
    };
    let alerts = 0;
    for (const record of records) {
      const signal = signals[record.key];
      if (signal.tone === "warn") alerts++;
      record.button.setAttribute("aria-pressed", String(record.key === active));
      record.button.dataset.tone = signal.tone;
      record.state.textContent = signal.title;
      record.detail.textContent = signal.detail;
    }
    byId("ops-watch-count").textContent = alerts > 0 ? alerts + (alerts === 1 ? " needs attention" : " need attention") : "";
    const chosen = records.find(record => record.key === active);
    const signal = signals[active];
    const header = element("header", undefined, "ops-panel-head");
    const heading = element("h2", chosen.label);
    heading.id = "ops-highlight-title";
    header.append(heading, chip(signal.title, signal.tone));
    const body = element("div", undefined, "ops-highlight-body");
    body.append(element("p", signal.detail, "ops-highlight-detail"));
    if (active === "freshness" && item) {
      body.append(ledger([
        ["Collection", item.name],
        ["Graph pending", metric(item.graph?.pending)],
        ["Vector pending", metric(item.vector?.pending)],
      ]));
    }
    if (active === "review" && item?.review?.status === "current") {
      body.append(ledger([
        ["Pending proposals", approximate(item.review.pending_proposals, item.review.pending_more)],
        ["Scoped rechecks", approximate(item.review.scoped_rechecks, item.review.scoped_more)],
      ]));
    }
    const action = element("button", chosen.action, "ops-secondary");
    action.type = "button";
    action.addEventListener("click", chosen.open);
    const footer = element("footer", undefined, "ops-highlight-foot");
    footer.append(action);
    const highlight = byId("ops-highlight");
    const actionHadFocus = highlight.contains(document.activeElement);
    highlight.replaceChildren(header, body, footer);
    if (actionHadFocus) action.focus({ preventScroll: true });
  }

  function renderCollections(item) {
    const rows = snapshot?.collections ?? [];
    const list = byId("ops-collections");
    const coverage = byId("ops-coverage-detail");
    if (!rows.length) {
      list.replaceChildren(element("p", "No collection status is available to this session.", "metadata"));
      coverage.replaceChildren();
      return;
    }
    const table = element("table", undefined, "ops-table");
    table.setAttribute("aria-label", "Observed coverage by authorized collection");
    const head = table.createTHead().insertRow();
    for (const label of ["Collection", "Passages", "Keyword", "Vector jobs", "Graph jobs"]) {
      const th = element("th", label);
      th.scope = "col";
      head.append(th);
    }
    const body = table.createTBody();
    for (const row of rows) {
      const line = body.insertRow();
      const action = element("button", row.name, "ops-table-link");
      action.type = "button";
      action.addEventListener("click", () => choose(row.name));
      line.dataset.selected = String(row.name === selected);
      line.insertCell().append(action);
      line.insertCell().textContent = metric(row.passages);
      line.insertCell().append(chip(row.passages > 0 ? "Indexed" : row.passages === 0 ? "Empty" : "Unknown", "neutral"));
      line.insertCell().append(chip(...laneState(row.vector)));
      line.insertCell().append(chip(...laneState(row.graph)));
    }
    list.replaceChildren(table);
    coverage.replaceChildren(lanePanel("Graph extraction", item.graph), lanePanel("Vector indexing", item.vector));
  }

  function render() {
    const item = collection();
    const values = byId("ops-kpis").querySelectorAll("dd");
    [item?.sources, item?.passages, item?.graph?.pending].forEach((v, i) => { values[i].textContent = metric(v); });
    byId("ops-work-scope").textContent = item?.name ?? "";
    const volume = byId("ops-volume");
    volume.replaceChildren(corpusChart(snapshot?.collections ?? [], selected, choose));
    const rings = element("div", undefined, "ops-rings");
    rings.append(queueRing("Graph extraction", item?.graph), queueRing("Vector indexing", item?.vector));
    const legend = element("div", undefined, "ops-queue-legend");
    for (const [key, title] of queueSeries) {
      const label = element("span", title);
      label.dataset.queue = key;
      legend.append(label);
    }
    byId("ops-enrichment").replaceChildren(rings, legend);
    const review = item?.review;
    const [reviewLabel] = reviewState(review);
    byId("ops-review-summary").replaceChildren(review?.status === "current"
      ? meters([
        ["Pending proposals", review.pending_proposals, "gold", review.pending_more],
        ["Scoped rechecks", review.scoped_rechecks, "alert", review.scoped_more],
      ]) : element("p", reviewLabel, "metadata"));
    const [state, tone] = syncState(snapshot);
    const summary = [chip(state, tone)];
    if (typeof snapshot?.sync?.inventory_reconciled === "boolean") summary.push(ledger([
      ["Source inventory", snapshot.sync.inventory_reconciled ? "Reconciled" : "Reconciliation pending"],
    ]));
    byId("ops-sync").replaceChildren(...summary);
    renderWatch();
    renderCollections(item);
  }

  function clearResults() {
    diagnostics.clear();
    clearReviewedEvidence();
    results = { trace: noTrace, access: noCitation };
  }

  function choose(name, clear = true, focusChart = false) {
    const changed = name !== selected;
    if (changed && clear) clearResults();
    selected = name;
    selector.value = name;
    if (changed) diagnostics.setCollection(name);
    render();
    if (focusChart) {
      const button = [...byId("ops-volume").querySelectorAll("[data-collection]")]
        .find(candidate => candidate.dataset.collection === name);
      button?.focus({ preventScroll: true });
    }
  }

  function updateOptions(rows) {
    const keys = rows.map(row => row.name);
    if (keys.join(",") !== [...selector.options].map(option => option.value).join(",")) {
      selector.replaceChildren(...rows.map(({ name }) => new Option(name, name)));
    }
    if (!keys.length) selector.replaceChildren(new Option("No available collections", ""));
    selector.disabled = !keys.length;
    diagnostics.setSearchScope(rows);
  }

  function forget() {
    generation++;
    snapshot = undefined;
    clearResults();
    updateOptions([]);
    choose("", false);
    setConnection("Session access changed · refresh to check current status", "warn");
  }

  async function refresh() {
    if (inFlight || document.hidden) return;
    clearTimeout(timer);
    inFlight = true;
    refreshButton.disabled = true;
    const requestGeneration = generation;
    try {
      const next = await request("/v1/operator/status");
      if (requestGeneration !== generation) return;
      if (next.schema_version !== 1 || !Array.isArray(next.collections) ||
          next.collections.some(row => !scopeName.test(row?.name ?? ""))) {
        throw new Error("Unexpected operator status response.");
      }
      const previous = collection();
      const nextCollection = next.collections.find(row => row.name === selected);
      const previousScopes = snapshot?.collections.map(row => row.name).sort().join(",");
      const nextScopes = next.collections.map(row => row.name).sort().join(",");
      if (snapshot && (snapshot.policy_version !== next.policy_version || previousScopes !== nextScopes ||
          Boolean(previous?.review) !== Boolean(nextCollection?.review))) {
        clearResults();
      }
      const signature = ({ observed_at_unix, ...state }) => JSON.stringify(state);
      const sameData = snapshot && signature(snapshot) === signature(next);
      snapshot = next;
      autoPoll = true;
      updateOptions(next.collections);
      if (!sameData) choose(nextCollection?.name ?? next.collections[0]?.name ?? "", false);
      setConnection(next.maintenance ? "Maintenance · evidence reads paused" :
        "Connected · observed " + utc(next.observed_at_unix), next.maintenance ? "warn" : "good");
    } catch (error) {
      if (requestGeneration !== generation) return;
      if (error.status === 401) {
        forget();
        setConnection("Operator status access is unavailable.", "warn");
      } else if (error.code === "MAINTENANCE") {
        forget();
        setConnection("Maintenance · evidence reads paused", "warn");
      } else if (error.status === 403 || error.status === 404) {
        forget();
        autoPoll = false;
        setConnection("Operator status is unavailable at this gateway.", "neutral");
      } else {
        setConnection(snapshot ? "Status refresh failed · showing last observation" :
          "Shared status unavailable · retry or check the gateway", "warn");
      }
    } finally {
      inFlight = false;
      refreshButton.disabled = false;
      if (autoPoll && !document.hidden) timer = setTimeout(refresh, 30000);
    }
  }

  for (const button of nav) button.addEventListener("click", () => showView(button.dataset.view));
  searchButton.addEventListener("click", () => showView("search"));
  selector.addEventListener("change", () => choose(selector.value));
  refreshButton.addEventListener("click", refresh);
  document.addEventListener("visibilitychange", () => {
    if (document.hidden) clearTimeout(timer);
    else if (autoPoll) refresh();
  });
  render();
  refresh();

  return {
    forget, refresh, showView,
    record(kind, result) {
      results[kind] = result;
      renderWatch();
    },
  };
}
