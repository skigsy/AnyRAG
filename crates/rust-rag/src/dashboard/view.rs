use std::fmt::Write as _;

use super::{Coverage, CoverageState, ReadState, ReviewQueue, Snapshot, history};

pub(super) const STYLE: &str = include_str!("view.css");
pub(super) const SCRIPT: &str = include_str!("view.js");

pub(super) fn render(snapshot: &Snapshot) -> String {
    let assets = format!(
        "<meta http-equiv=\"Content-Security-Policy\" content=\"default-src 'none'; style-src 'unsafe-inline'; base-uri 'none'; form-action 'none'\"><style>{STYLE}</style>"
    );
    let stamp = clock(snapshot.observed_at_unix_seconds as i64);
    page(
        &assets,
        "Offline snapshot",
        "Snapshot",
        &stamp,
        "",
        &cards(snapshot, None),
        "",
        "",
    )
}

pub(super) fn live_shell(show_review_queue: bool) -> String {
    let review_link = if show_review_queue {
        r##"<a href="#graph-review">Review queue</a>"##
    } else {
        ""
    };
    page(
        r#"<link rel="stylesheet" href="/dashboard.css"><script defer src="/dashboard.js"></script>"#,
        "Pairing required",
        "Last successful refresh",
        "—",
        r#"<section class="panel pair" id="pairing" aria-labelledby="pair-title">
  <div class="panel-head"><h2 id="pair-title">Connect this browser</h2><span class="label">Local session</span></div>
  <div class="panel-body">
    <p>Enter the access code shown in the terminal that started the dashboard.</p>
    <form id="pair-form" method="post" autocomplete="off">
      <label for="access-code">Access code</label>
      <input id="access-code" type="password" maxlength="64" required autocomplete="off" spellcheck="false" autocapitalize="off">
      <button type="submit">Connect</button>
    </form>
  </div>
</section>"#,
        "",
        r#"<section class="panel" id="indexed-sources" hidden>
  <div class="panel-head"><h2>Indexed sources</h2></div>
  <div class="panel-body">
    <button type="button" id="load-sources">Browse indexed sources</button>
    <form id="open-indexed-source" method="post">
      <label for="indexed-source-id">Or open a source ID from a query</label>
      <input id="indexed-source-id" type="text" maxlength="64" pattern="[a-fA-F0-9]{64}" required spellcheck="false" autocomplete="off">
      <button type="submit">Open source</button>
    </form>
    <div id="source-results" aria-live="polite"></div>
    <details><summary>Review skipped and failed files</summary>
      <p>Choose the JSON output from <code>ingest --review --json</code>. The browser reads the report locally.</p>
      <input type="file" id="ingest-review-file" accept=".json,application/json" aria-label="Ingest review JSON">
      <div id="ingest-review-results" aria-live="polite"></div>
    </details>
  </div>
</section>"#,
        review_link,
    )
}

fn page(
    assets: &str,
    status: &str,
    time_label: &str,
    time: &str,
    pair: &str,
    content: &str,
    sources: &str,
    review_link: &str,
) -> String {
    format!(
        r##"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <meta name="referrer" content="no-referrer">
  <title>Index overview | Any RAG</title>
  {assets}
</head>
<body>
<header class="topbar">
  <div class="brand"><span class="wordmark">Any <span>RAG</span></span><span class="label product">Operator console</span></div>
  <span class="connection" id="connection-state" role="status">{status}</span>
</header>
<div class="frame">
  <aside class="rail">
    <p class="label">Workspace</p>
    <nav aria-label="Dashboard sections">
      <a href="#overview">Overview</a><a href="#pipeline">Pipeline</a><a href="#recent-activity">Activity</a>{review_link}
    </nav>
    <p class="rail-note">Local personal index<br>Read-only view</p>
  </aside>
  <main id="overview">
    <div class="title-row">
      <div><span class="label">Personal index</span><h1>Index overview</h1></div>
      <p class="refresh-meta">{time_label}<br><time id="last-refresh">{time}</time></p>
    </div>
    <p id="connection-notice" class="notice" role="status" hidden></p>
    {pair}
    <div id="dashboard-content">{content}</div>
    {sources}
  </main>
</div>
</body>
</html>
"##
    )
}

pub(super) fn cards(snapshot: &Snapshot, review: Option<&anyhow::Result<ReviewQueue>>) -> String {
    let mut html = String::new();
    let observed = snapshot.observed_at_unix_seconds;
    let _ = write!(
        html,
        r#"<div id="snapshot-observation" data-observed-at="{observed}">"#
    );
    if let Some(index) = &snapshot.index {
        let sources = index.sources;
        let chunks = index.chunks;
        let (keyword_status, keyword_tone) = if chunks == 0 {
            ("Idle", "neutral")
        } else {
            ("Indexed", "good")
        };
        let sync = snapshot
            .history
            .last_complete_reconciliation_unix_seconds
            .map(short_clock)
            .unwrap_or_else(|| "—".to_owned());
        let sync_detail = if sync == "—" {
            "No completed sync recorded"
        } else {
            "Latest complete sync · UTC"
        };
        let _ = write!(
            html,
            r#"
<dl class="metrics">
  <div class="metric"><dt class="label">Active sources</dt><dd class="number">{sources}</dd><dd class="muted">In the committed index</dd></div>
  <div class="metric"><dt class="label">Active chunks</dt><dd class="number">{chunks}</dd><dd class="muted">Passages eligible for search</dd></div>
  <div class="metric"><dt class="label">Reconciled</dt><dd class="number">{sync}</dd><dd class="muted">{sync_detail}</dd></div>
</dl>
<div class="work">
  <section class="panel" id="pipeline">
    <header class="panel-head"><h2>Pipeline coverage</h2><span class="label">Current index</span></header>
    <div class="pipeline">
      <div class="lane"><h3>Keyword</h3><span class="dot" data-tone="{keyword_tone}">{keyword_status}</span><span class="count">{chunks}</span></div>
"#
        );
        if let Some(vector) = &index.vector {
            lane(&mut html, "Vector", "vector-coverage", vector);
        }
        if let Some(graph) = &index.graph {
            lane(&mut html, "Graph", "graph-coverage", graph);
        }
        html.push_str("</div></section>");
        next(&mut html, snapshot);
        html.push_str("</div>");
    } else {
        let (heading, detail) = match snapshot.state {
            ReadState::Available => ("Index unavailable", "The reader returned no index counts."),
            ReadState::Missing => (
                "Index missing",
                "Select an existing private index with <code>--state-dir</code> or run <code>init</code>.",
            ),
            ReadState::Unavailable => (
                "Index unavailable",
                "Retry after active writers finish. Check that the index folder is private and accessible.",
            ),
            ReadState::UnsupportedJournal => (
                "Unsupported journal mode",
                "Use a personal index in SQLite DELETE journal mode.",
            ),
            ReadState::UnsupportedSchema => (
                "Unsupported index format",
                "Use an Any RAG build that supports this index format.",
            ),
        };
        let _ = write!(
            html,
            r#"<section class="panel" id="pipeline"><header class="panel-head"><h2>{heading}</h2></header><div class="panel-body"><p>{detail}</p></div></section>"#
        );
    }
    activity(&mut html, &snapshot.history);
    if let Some(review) = review {
        graph_review(&mut html, review);
    }
    html.push_str(r#"<p class="foot">Coverage describes committed index data. Use ingest review to check source files.</p></div>"#);
    html
}

pub(super) struct SourcePassage {
    pub start: i64,
    pub end: i64,
    pub page: Option<i64>,
    pub anchor: Option<String>,
    pub text: String,
}

pub(super) fn sources(entries: Vec<(String, String)>) -> String {
    if entries.is_empty() {
        return "<p>There are no indexed sources.</p>".into();
    }
    let mut html = String::from("<ul>");
    for (id, path) in entries.iter().take(50) {
        let _ = write!(
            html,
            r#"<li><button type="button" data-source-id="{}">{}</button></li>"#,
            escape_text(id),
            escape_text(path)
        );
    }
    html.push_str("</ul>");
    if entries.len() > 50 {
        html.push_str("<p>Showing the first 50 indexed sources.</p>");
    }
    html
}

pub(super) fn missing_source() -> String {
    "<p>This source is no longer in the index.</p>".into()
}

pub(super) fn source_passages(path: &str, passages: Vec<SourcePassage>) -> String {
    let mut html = format!("<h3>{}</h3>", escape_text(path));
    if passages.is_empty() {
        html.push_str("<p>This source has no indexed passages.</p>");
    }
    for passage in passages.iter().take(30) {
        let location = match (&passage.anchor, passage.page) {
            (Some(anchor), _) => format!("{} · ", escape_text(anchor)),
            (_, Some(page)) => format!("Page {page} · "),
            _ => String::new(),
        };
        let _ = write!(
            html,
            "<details><summary>{location}Offsets {}–{}</summary><pre>{}</pre></details>",
            passage.start,
            passage.end,
            escape_text(&passage.text)
        );
    }
    if passages.len() > 30 {
        html.push_str("<p>Showing the first 30 passages.</p>");
    }
    html.push_str("<p>Passage previews show up to 1,200 characters.</p>");
    html
}

fn escape_text(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&#39;"),
            character => escaped.push(character),
        }
    }
    escaped
}

fn graph_review(html: &mut String, queue: &anyhow::Result<ReviewQueue>) {
    html.push_str(
        r#"<section class="panel review" id="graph-review" aria-labelledby="review-title">
<header class="panel-head"><h2 id="review-title">Graph review queue</h2><span class="label">Read-only review</span></header>
<div class="panel-body">"#,
    );
    match queue {
        Err(_) => {
            html.push_str("<p>The review queue could not be read. The dashboard will retry.</p>")
        }
        Ok(queue) if queue.proposals.is_empty() => {
            html.push_str("<p>No graph proposals are waiting for review.</p>");
        }
        Ok(queue) => {
            html.push_str(
                r#"<p>Check the quoted evidence and its direction before approving an edge.</p>
<p class="muted">Run one command from a proposal in your terminal. Replace <code>&lt;state-dir&gt;</code> with the same private index folder used to launch this dashboard.</p>
<ol class="review-list">"#,
            );
            for proposal in &queue.proposals {
                let subject = escape_text(&proposal.subject);
                let predicate = escape_text(&proposal.predicate);
                let object = escape_text(&proposal.object);
                let quote = escape_text(&proposal.quote);
                let path = escape_text(&proposal.path);
                let _ = write!(
                    html,
                    r#"<li>
<h3 class="review-direction"><bdi>{subject}</bdi> → <code>{predicate}</code> → <bdi>{object}</bdi></h3>
<blockquote dir="auto">{quote}</blockquote>
<p class="review-source"><span class="label">Source</span> <code dir="auto">{path}</code>
<span class="muted">Characters {}–{}</span></p>"#,
                    proposal.start, proposal.end
                );
                // Command snippets only interpolate the fixed-format ID, never document text or paths.
                let id = &proposal.proposal_id;
                if id.strip_prefix("proposal_").is_some_and(|digest| {
                    digest.len() == 32
                        && digest
                            .bytes()
                            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
                }) {
                    let _ = write!(
                        html,
                        r#"<dl class="review-commands">
<dt>Approve</dt><dd><code>rust-rag --state-dir "&lt;state-dir&gt;" graph-review approve {id}</code></dd>
<dt>Dismiss</dt><dd><code>rust-rag --state-dir "&lt;state-dir&gt;" graph-review dismiss {id}</code></dd>
</dl>"#
                    );
                } else {
                    html.push_str(
                        r#"<p class="muted">This proposal has an invalid identifier.</p>"#,
                    );
                }
                html.push_str("</li>");
            }
            html.push_str("</ol>");
            if queue.has_more {
                html.push_str(
                    r#"<p class="muted">More proposals are pending. The next items appear as you review this list.</p>"#,
                );
            }
        }
    }
    html.push_str("</div></section>");
}

fn lane(html: &mut String, name: &str, id: &str, lane: &Coverage) {
    let (state, label, tone) = match lane.state {
        CoverageState::Idle => ("idle", "Idle", "neutral"),
        CoverageState::Current => ("current", "Current", "good"),
        CoverageState::Behind => ("behind", "Behind", "warn"),
    };
    let Coverage {
        completed,
        eligible,
        pending,
        ..
    } = lane;
    let _ = write!(
        html,
        r#"
<div class="lane" id="{id}" data-state="{state}" data-completed="{completed}" data-eligible="{eligible}">
  <h3>{name}</h3><div><span class="dot" data-tone="{tone}">{label}</span>
"#
    );
    if *eligible > 0 {
        let _ = write!(
            html,
            r#"<meter min="0" max="{eligible}" value="{completed}" aria-label="{name} coverage"></meter>"#
        );
    }
    let _ = write!(
        html,
        r#"</div><span class="count">{completed} / {eligible}</span>"#
    );
    if *pending > 0 {
        let _ = write!(html, "<small>{pending} pending");
        if let Some(scheduling) = &lane.scheduling {
            let _ = write!(
                html,
                " · {} deferred · {} quarantined · {} leased",
                scheduling.deferred, scheduling.quarantined, scheduling.leased
            );
        }
        html.push_str("</small>");
    }
    html.push_str("</div>");
}

fn next(html: &mut String, snapshot: &Snapshot) {
    html.push_str(r#"<section class="panel" id="next-steps"><header class="panel-head"><h2>What needs attention</h2></header><div class="panel-body">"#);
    if let Some(index) = &snapshot.index {
        let graph = index.graph.as_ref().filter(|lane| lane.pending > 0);
        let vector = index.vector.as_ref().filter(|lane| lane.pending > 0);
        if graph.is_none() && vector.is_none() {
            html.push_str("<p>No pending work in the configured enrichment lanes.</p>");
        } else if snapshot.build.model_endpoint {
            for (lane, name, command) in [
                (graph, "Graph", "extract-graph"),
                (vector, "Vector", "embed"),
            ] {
                if let Some(lane) = lane {
                    next_lane(html, lane, name, command);
                }
            }
        } else {
            html.push_str("<p>Use a build with model support to complete pending enrichment.</p>");
        }
    }
    html.push_str(r#"<p class="muted">For source failures, use <code>ingest --review --json</code>. For one query, use <code>query --explain --json</code>.</p></div></section>"#);
}

fn next_lane(html: &mut String, lane: &Coverage, name: &str, command: &str) {
    let action = match &lane.scheduling {
        Some(scheduling) if scheduling.provider_paused_until_unix.is_some() => {
            "The provider is paused. Check the model endpoint before retrying."
        }
        Some(scheduling) if scheduling.ready == 0 && scheduling.quarantined > 0 => {
            "Check the model output before retrying quarantined work."
        }
        Some(scheduling) if scheduling.ready == 0 => {
            "Work is deferred or running. Check again when it becomes ready."
        }
        _ => {
            let _ = write!(
                html,
                "<p>{name} work is pending. Run <code>{command}</code> with the configured model.</p>"
            );
            return;
        }
    };
    let _ = write!(html, "<p>{name} coverage is behind. {action}</p>");
}

fn clock(seconds: i64) -> String {
    time::OffsetDateTime::from_unix_timestamp(seconds).map_or_else(
        |_| "—".to_owned(),
        |t| {
            format!(
                "{:02}-{:02} {:02}:{:02}:{:02} UTC",
                t.month() as u8,
                t.day(),
                t.hour(),
                t.minute(),
                t.second()
            )
        },
    )
}

fn short_clock(seconds: i64) -> String {
    time::OffsetDateTime::from_unix_timestamp(seconds).map_or_else(
        |_| "—".to_owned(),
        |t| {
            format!(
                "{:02}-{:02} {:02}:{:02}",
                t.month() as u8,
                t.day(),
                t.hour(),
                t.minute()
            )
        },
    )
}

fn milliseconds(micros: i64) -> String {
    format!("{:.2} ms", micros as f64 / 1000.0)
}

fn activity(html: &mut String, history: &history::Summary) {
    let (label, note) = match history.state {
        "disabled" if history.latest_ingest.is_some() || history.recent_queries.is_some() => (
            "Recording off",
            "Showing retained observations. Run <code>history enable</code> to resume.",
        ),
        "unavailable" if history.reason == Some("index_replaced_or_rolled_back") => (
            "History unavailable",
            "The index changed. Run <code>history enable</code> to start a fresh history window.",
        ),
        "available" => ("Recording on", "Activity from this index."),
        "idle" => (
            "Recording on",
            "Waiting for the first recorded CLI operation.",
        ),
        "disabled" => (
            "Recording off",
            "History is opt-in. Run <code>history enable</code> for this index.",
        ),
        _ => (
            "History unavailable",
            "The local history store could not be read or matched to this index.",
        ),
    };
    let _ = write!(
        html,
        r#"
<section class="panel activity" id="recent-activity">
  <header class="panel-head"><h2>Recent activity</h2><span class="label">{label}</span></header>
  <div class="panel-body muted">{note} Up to seven days or 2,000 CLI ingest and query observations.</div>
"#
    );
    if history.recent_queries.is_none() && history.latest_ingest.is_none() {
        html.push_str("</section>");
        return;
    }
    html.push_str("<div class=\"activity-body\">");
    queries(html, history.recent_queries.as_ref());
    ingest(html, history.latest_ingest.as_ref());
    html.push_str("</div><p class=\"panel-body foot\">Engine time excludes opening the index, recording history and writing CLI output.</p></section>");
}

fn queries(html: &mut String, queries: Option<&history::QuerySummary>) {
    html.push_str("<section class=\"activity-block\"><h3>Queries</h3>");
    if let Some(queries) = queries {
        let count = queries.count;
        let median = milliseconds(queries.median_duration_micros);
        let first = clock(queries.first_observed_unix_seconds);
        let last = clock(queries.last_observed_unix_seconds);
        let _ = write!(
            html,
            r#"<p><strong>{count}</strong> recorded · <strong>{median}</strong> median engine time</p><p class="muted">{first} to {last}</p><div class="outcomes">"#
        );
        for (label, count) in [
            ("Evidence", queries.evidence),
            ("Weak match", queries.weak_match),
            ("Abstained", queries.abstained),
            ("Error", queries.errors),
        ] {
            let _ = write!(
                html,
                "<div><strong class=\"count\">{count}</strong><span>{label}</span></div>"
            );
        }
        html.push_str("</div><dl class=\"kv\">");
        let latest = &queries.latest;
        let recipe = match enum_key(&latest.mode).as_str() {
            "lexical" => "Keyword",
            "vector" => "Vector",
            "hybrid" => "Hybrid",
            _ => "Unavailable",
        };
        let outcome = match enum_key(&latest.status).as_str() {
            "evidence" => "Evidence",
            "weak_match" => "Weak match",
            "abstained" => "Abstained",
            "error" => "Error",
            _ => "Unavailable",
        };
        let duration = milliseconds(latest.duration_micros);
        let graph = if latest.graph_requested {
            " + graph"
        } else {
            ""
        };
        let _ = write!(
            html,
            "<dt>Latest query</dt><dd>{recipe}{graph} · {outcome}</dd><dt>Latest engine time</dt><dd>{duration}</dd>"
        );
        if let Some(graph) = &latest.graph {
            let seed = match enum_key(&graph.seed).as_str() {
                "unavailable" => "Unavailable",
                "no_seed" => "No seed",
                "seeded" => "Seeded",
                "result_limit" => "Result limit reached",
                _ => "Unobserved",
            };
            let _ = write!(html, "<dt>Graph at last query</dt><dd>{seed}</dd>");
            if graph.index_pending
                || graph.construction_budget_exhausted
                || graph.traversal_budget_exhausted
            {
                let mut reasons = Vec::new();
                if graph.index_pending {
                    reasons.push("Extraction pending");
                }
                if graph.construction_budget_exhausted {
                    reasons.push("Construction limit");
                }
                if graph.traversal_budget_exhausted {
                    reasons.push("Traversal limit");
                }
                let _ = write!(
                    html,
                    "<dt>Graph limits</dt><dd>{}</dd>",
                    reasons.join(" · ")
                );
            }
        }
        if let Some(duration) = latest.retrieval_micros {
            let _ = write!(
                html,
                "<dt>Retrieval</dt><dd>{}</dd>",
                milliseconds(duration)
            );
        }
        if let Some(duration) = latest.rerank_micros {
            let _ = write!(
                html,
                "<dt>Latest rerank</dt><dd>{}</dd>",
                milliseconds(duration)
            );
        }
        if let Some(fallbacks) = queries.rerank_fallbacks {
            let _ = write!(
                html,
                "<dt>Rerank fallbacks</dt><dd>{fallbacks} / {}</dd>",
                queries.rerank_attempted
            );
        }
        html.push_str("</dl>");
    } else {
        html.push_str("<p class=\"muted\">No recorded queries in this window.</p>");
    }
    html.push_str("</section>");
}

fn ingest(html: &mut String, observation: Option<&history::IngestObservation>) {
    html.push_str("<section class=\"activity-block\"><h3>Latest ingest</h3>");
    if let Some(ingest) = observation {
        let status = match enum_key(&ingest.status).as_str() {
            "completed" => "Completed",
            "partial" => "Partial",
            "error" => "Error",
            "completion_unobserved" => "Completion unobserved",
            _ => "Unavailable",
        };
        let started = clock(ingest.started_unix_seconds);
        let _ = write!(
            html,
            r#"<p><strong>{status}</strong> · started {started}</p><dl class="kv">"#
        );
        if let Some(duration) = ingest.duration_micros {
            let _ = write!(
                html,
                "<dt>Engine time</dt><dd>{}</dd>",
                milliseconds(duration)
            );
        }
        if let Some(inventory) = ingest.inventory_complete {
            let label = if inventory { "Complete" } else { "Incomplete" };
            let _ = write!(html, "<dt>Inventory</dt><dd>{label}</dd>");
        }
        if ingest.sync_requested {
            let label = match ingest.complete_reconciliation {
                Some(true) => "Complete",
                Some(false) => "Incomplete",
                None => "Unobserved",
            };
            let _ = write!(html, "<dt>Reconciliation</dt><dd>{label}</dd>");
        }
        if ingest.sync_blocked == Some(true) {
            let _ = write!(html, "<dt>Sync blocked</dt><dd>Review ingest output</dd>");
        }
        if let Some(counts) = &ingest.counts {
            for (label, count) in [
                ("Updated", counts.updated_sources),
                ("Unchanged", counts.unchanged_sources),
                ("Failed", counts.failed_sources),
                ("Deferred", counts.deferred_sources),
                ("Retained prior revisions", counts.retained_previous_sources),
                ("Removed", counts.removed_sources),
                (
                    "Skipped PDF, Word, and Excel files",
                    counts
                        .skipped_pdfs
                        .saturating_add(counts.skipped_docx)
                        .saturating_add(counts.skipped_xlsx),
                ),
            ] {
                let _ = write!(html, "<dt>{label}</dt><dd>{count}</dd>");
            }
        }
        html.push_str("</dl>");
    } else {
        html.push_str("<p class=\"muted\">No recorded ingest in this window.</p>");
    }
    html.push_str("</section>");
}

fn enum_key(value: &impl serde::Serialize) -> String {
    match serde_json::to_value(value) {
        Ok(serde_json::Value::String(key)) => key,
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dashboard::{Build, ReviewProposal};

    fn snapshot() -> Snapshot<'static> {
        Snapshot {
            schema_version: 1,
            observed_at_unix_seconds: 1,
            scope: "one_local_personal_index",
            state: ReadState::Available,
            build: Build {
                model_endpoint: false,
                native_embeddings: false,
            },
            index: None,
            history: history::Summary::unavailable(),
        }
    }

    fn queue(proposal_id: String) -> anyhow::Result<ReviewQueue> {
        Ok(ReviewQueue {
            schema_version: 1,
            proposals: vec![ReviewProposal {
                proposal_id,
                subject: "Switch <A>".into(),
                predicate: "uses".into(),
                object: "Relay & Sensor".into(),
                quote: r#"<img src="bad" onerror="alert(1)">"#.into(),
                path: "private/<note>.md".into(),
                start: 5,
                end: 40,
            }],
            has_more: true,
        })
    }

    #[test]
    fn queue_escapes_private_text_and_stays_out_of_offline_views() {
        let id = format!("proposal_{}", "a".repeat(32));
        let snapshot = snapshot();
        let html = cards(&snapshot, Some(&queue(id.clone())));
        assert!(html.contains(r#"id="graph-review""#));
        assert!(html.contains("<bdi>Switch &lt;A&gt;</bdi> → <code>uses</code>"));
        assert!(html.contains("Relay &amp; Sensor"));
        assert!(html.contains("&lt;img src=&quot;bad&quot; onerror=&quot;alert(1)&quot;&gt;"));
        assert!(html.contains("private/&lt;note&gt;.md"));
        assert!(!html.contains("<img"));
        for action in ["approve", "dismiss"] {
            assert!(html.contains(&format!(
                r#"rust-rag --state-dir "&lt;state-dir&gt;" graph-review {action} {id}"#
            )));
        }
        assert!(html.contains("More proposals are pending"));
        assert!(live_shell(true).contains(r##"href="#graph-review""##));
        assert!(!live_shell(false).contains("graph-review"));
        assert!(!cards(&snapshot, None).contains("graph-review"));
        assert!(!render(&snapshot).contains(r#"id="graph-review""#));
        assert!(
            serde_json::to_value(snapshot)
                .unwrap()
                .get("review")
                .is_none()
        );
    }

    #[test]
    fn review_never_puts_an_invalid_id_or_error_details_in_html() {
        let snapshot = snapshot();
        let html = cards(&snapshot, Some(&queue("proposal_; unsafe".into())));
        assert!(html.contains("invalid identifier"));
        assert!(!html.contains("unsafe"));
        assert!(!html.contains("graph-review approve"));
        let unavailable = cards(
            &snapshot,
            Some(&Err(anyhow::anyhow!("private database detail"))),
        );
        assert!(unavailable.contains("review queue could not be read"));
        assert!(!unavailable.contains("private database detail"));
    }
}
