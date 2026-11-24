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
        r##"<a href="#graph-review" hidden>Graph review</a>"##
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
    <details id="source-lookup"><summary>Open a source from query output</summary>
      <form id="open-indexed-source" method="post">
        <label for="indexed-source-id">Source ID</label>
        <input id="indexed-source-id" type="text" maxlength="64" pattern="[a-fA-F0-9]{64}" required spellcheck="false" autocomplete="off">
        <button type="submit">Open source</button>
      </form>
    </details>
    <p id="source-status" role="status"></p>
    <div id="source-results"></div>
  </div>
</section>
<section class="panel" id="ingest-report" hidden aria-labelledby="ingest-report-title">
  <header class="panel-head"><h2 id="ingest-report-title">Ingest report</h2></header>
  <div class="panel-body">
    <details id="ingest-review"><summary>Review skipped, failed or deferred files</summary>
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
    let (view, title) = if pair.is_empty() {
        ("", "Index overview")
    } else {
        (r#" data-view="overview""#, "Overview")
    };
    format!(
        r##"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <meta name="referrer" content="no-referrer">
  <title>Index overview | AnyRAG</title>
  {assets}
</head>
<body>
<a href="#workspace-title" class="skip-link">Skip to dashboard content</a>
<div class="frame">
  <aside class="rail">
    <div class="rail-inner">
      <div class="brand"><span class="wordmark">Any<span>RAG</span></span><span class="label product">Operator console</span></div>
      <nav aria-label="Dashboard sections">
        <a href="#overview" aria-current="location">Overview</a><a href="#pipeline">Coverage</a><a href="#indexed-sources" hidden>Sources</a><a href="#recent-activity">Activity</a>{review_link}
      </nav>
      <p class="index-scope"><strong>Personal index</strong><span>Read-only</span></p>
    </div>
  </aside>
  <main id="overview" aria-labelledby="workspace-title"{view}>
    <header class="title-row">
      <h1 id="workspace-title" tabindex="-1">{title}</h1>
      <div class="live-meta">
        <span class="connection" id="connection-state">{status}</span>
        <p class="refresh-meta"><span>{time_label}</span><time id="last-refresh">{time}</time><span id="retry-age"></span></p>
      </div>
    </header>
    <p id="connection-notice" class="notice" hidden></p>
    <p id="dashboard-announcement" class="sr-only" role="status" aria-atomic="true"></p>
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
    let sync_at = snapshot
        .history
        .last_complete_reconciliation_unix_seconds
        .unwrap_or(0);
    let latest_ingest =
        snapshot
            .history
            .latest_ingest
            .as_ref()
            .map_or_else(String::new, |ingest| {
                format!(
                    "{}-{}-{}",
                    ingest.started_unix_seconds,
                    ingest.finished_unix_seconds.unwrap_or(0),
                    enum_key(&ingest.status)
                )
            });
    let _ = write!(
        html,
        r#"<div id="snapshot-observation" data-observed-at="{observed}" data-sync-at="{sync_at}" data-ingest="{latest_ingest}">"#
    );
    if let Some(index) = &snapshot.index {
        let sources = index.sources;
        let chunks = index.chunks;
        let (keyword_status, keyword_tone) = if chunks == 0 {
            ("Idle", "neutral")
        } else {
            ("Indexed", "good")
        };
        let attention = attention(snapshot, review);
        let empty = chunks == 0 && attention.first().is_none_or(|item| item.tone == "neutral");
        let (headline, tone) = if empty {
            ("Index is empty", "neutral")
        } else {
            attention
                .first()
                .map_or(("Search is ready", "good"), |item| (item.title, item.tone))
        };
        let icon = match tone {
            "good" => "✓",
            "warn" | "error" => "!",
            _ => "·",
        };
        let sync = snapshot
            .history
            .last_complete_reconciliation_unix_seconds
            .map_or_else(|| "None recorded".into(), timestamp);
        let sync_detail = if snapshot
            .history
            .last_complete_reconciliation_unix_seconds
            .is_some()
        {
            "via <code>--sync</code>"
        } else {
            "A regular ingest can complete without syncing"
        };
        let more = match attention.len().saturating_sub(usize::from(!empty)) {
            0 => String::new(),
            count => format!(r##"<a class="verdict-more" href="#next-steps">+{count} more</a>"##),
        };
        let graph_total = index.graph.as_ref().map_or_else(
            || r#"<dd class="metric-disabled">Off</dd>"#.to_owned(),
            |graph| {
                format!(
                    r#"<dd class="number">{}<span> / {}</span></dd>"#,
                    graph.completed, graph.eligible
                )
            },
        );
        let _ = write!(
            html,
            r#"
<section class="index-status" id="index-status" aria-labelledby="index-health">
<dl class="metrics">
  <div class="metric"><dt class="label">Sources</dt><dd class="number">{sources}</dd></div>
  <div class="metric"><dt class="label">Passages</dt><dd class="number">{chunks}</dd></div>
  <div class="metric"><dt class="label">Graph coverage</dt>{graph_total}</div>
</dl>
<div class="status-strip">
  <div class="verdict" data-tone="{tone}">
    <span class="verdict-icon" aria-hidden="true">{icon}</span>
    <div><p class="verdict-label" id="verdict-label">Index status</p><h2 id="index-health">{headline}</h2>{more}</div>
  </div>
  <dl class="sync"><dt class="label">Last full sync</dt><dd>{sync}</dd><dd class="muted">{sync_detail}</dd></dl>
</div>
</section>
"#,
        );
        overview_charts(&mut html, &snapshot.history);
        html.push_str(r#"<div class="work" id="index-work">"#);
        next(&mut html, &attention);
        let _ = write!(
            html,
            r#"
  <section class="panel" id="pipeline">
    <header class="panel-head"><h2>Retrieval coverage</h2></header>
    <div class="coverage-map" aria-label="How the index is built">
      <div><h3>Sources</h3><p>{sources} indexed</p></div>
      <div><h3>Passages</h3><p>{chunks} extracted</p></div>
      <div><h3>Retrieval</h3><p>Keyword, vector, and graph coverage below</p></div>
    </div>
    <div class="pipeline">
      <div class="ledger-head" aria-hidden="true"><span>Search mode</span><span>Coverage</span><span>Passages</span></div>
      <div class="lane" id="keyword-coverage">
        <h3>Keyword search</h3><span class="dot" data-tone="{keyword_tone}">{keyword_status}</span>
        <span class="count">{chunks} / {chunks}</span>
        {}
      </div>
"#,
            if chunks > 0 {
                format!(
                    r#"<meter min="0" max="{chunks}" value="{chunks}" aria-label="Keyword coverage"></meter>"#
                )
            } else {
                String::new()
            }
        );
        if let Some(vector) = &index.vector {
            lane(&mut html, "Vector search", "vector-coverage", vector);
        } else {
            unconfigured(&mut html, "Vector search", "vector-coverage");
        }
        if let Some(graph) = &index.graph {
            lane(&mut html, "Graph extraction", "graph-coverage", graph);
        } else {
            unconfigured(&mut html, "Graph extraction", "graph-coverage");
        }
        html.push_str("</div></section>");
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
                "Use an AnyRAG build that supports this index format.",
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
    html.push_str("</div>");
    html
}

pub(super) struct SourcePassage {
    pub id: String,
    pub start: i64,
    pub end: i64,
    pub page: Option<i64>,
    pub anchor: Option<String>,
    pub text: String,
    pub characters: i64,
}

pub(super) fn sources(entries: Vec<(String, String)>, page: u32) -> String {
    if entries.is_empty() {
        return if page == 1 {
            "<p>There are no indexed sources.</p>".into()
        } else {
            r#"<p>This page is empty. The index may have changed.</p><button type="button" data-source-page="1">First page</button>"#.into()
        };
    }
    let mut html = format!(
        r#"<div class="source-title"><h3 id="source-result-heading" tabindex="-1">Sources · Page {page}</h3><nav class="source-pages" aria-label="Indexed source pages">"#
    );
    if page > 1 {
        let _ = write!(
            html,
            r#"<button type="button" data-source-page="{}">Previous page</button>"#,
            page - 1
        );
    }
    if entries.len() > 50 && page < super::MAX_SOURCE_PAGE {
        let _ = write!(
            html,
            r#"<button type="button" data-source-page="{}">Next page</button>"#,
            page + 1
        );
    }
    html.push_str("</nav></div><ul>");
    for (id, path) in entries.iter().take(50) {
        let _ = write!(
            html,
            r#"<li><button type="button" data-source-id="{}" title="{}">{}</button></li>"#,
            escape_text(id),
            escape_text(path),
            escape_text(path.rsplit(['/', '\\']).next().unwrap_or(path))
        );
    }
    html.push_str("</ul>");
    html
}

pub(super) fn missing_source() -> String {
    "<p>This source is no longer in the index.</p>".into()
}

pub(super) fn source_passages(
    id: &str,
    path: &str,
    revision: &str,
    passages: Vec<SourcePassage>,
    page: u32,
) -> String {
    let mut html = format!(
        r#"<div class="source-title"><h3 id="source-result-heading" class="source-heading" tabindex="-1">{}</h3><div class="source-pages"><button type="button" id="back-to-sources">Back to sources</button><button type="button" data-source-id="{}">Reload source</button></div></div>
<p class="source-meta">Revision <code>{}</code></p>
<details class="source-meta"><summary>Path and source ID</summary><p><code dir="auto">{}</code></p><p><code>{}</code></p></details>"#,
        escape_text(path.rsplit(['/', '\\']).next().unwrap_or(path)),
        escape_text(id),
        escape_text(revision),
        escape_text(path),
        escape_text(id)
    );
    if passages.is_empty() {
        html.push_str("<p>No passages on this page.</p>");
    }
    for passage in passages.iter().take(30) {
        let location = match (&passage.anchor, passage.page) {
            (Some(anchor), _) => format!("{} · ", escape_text(anchor)),
            (_, Some(page)) => format!("Page {page} · "),
            _ => String::new(),
        };
        let _ = write!(
            html,
            "<details><summary>{location}Offsets {}–{}</summary><pre>{}</pre>{}<p class=\"source-meta\">Chunk ID <code>{}</code></p></details>",
            passage.start,
            passage.end,
            escape_text(&passage.text),
            if passage.characters > 1200 {
                format!(
                    "<p class=\"foot\">Truncated at 1,200 of {} characters. Use the citation command for the full passage.</p>",
                    passage.characters
                )
            } else {
                String::new()
            },
            escape_text(&passage.id)
        );
    }
    if page > 1 || passages.len() > 30 {
        let _ = write!(
            html,
            r#"<nav class="source-pages" aria-label="Passage pages"><span>Passage page {page}</span>"#
        );
        for (available, number, label) in [
            (page > 1, page.saturating_sub(1), "Previous passages"),
            (
                passages.len() > 30 && page < super::MAX_SOURCE_PAGE,
                page + 1,
                "Next passages",
            ),
        ] {
            if available {
                let _ = write!(
                    html,
                    r#"<button type="button" data-source-id="{}" data-passage-page="{number}">{label}</button>"#,
                    escape_text(id)
                );
            }
        }
        html.push_str("</nav>");
    }
    html.push_str("<p class=\"foot\">Excerpt previews show up to 1,200 characters. The citation command returns the complete indexed passage.</p>");
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
    if queue.as_ref().is_ok_and(|queue| queue.proposals.is_empty()) {
        html.push_str(r#"<section class="review review-clear" id="graph-review"><h2 tabindex="-1">Graph review</h2><p>No proposals awaiting review</p></section>"#);
        return;
    }
    html.push_str(
        r#"<section class="panel review" id="graph-review" aria-labelledby="review-title">
<header class="panel-head"><h2 id="review-title" tabindex="-1">Graph review queue</h2></header>
<div class="panel-body">"#,
    );
    match queue {
        Err(_) => {
            html.push_str("<p>The review queue could not be read. The dashboard will retry.</p>")
        }
        Ok(queue) => {
            html.push_str(
                r#"<p>Check the quoted evidence and its direction before approving an edge.</p>
<p class="muted">Before you run a review command, replace <strong><code>&lt;state-dir&gt;</code></strong> with the private index folder used to launch this dashboard.</p>"#,
            );
            let mut selectors = String::new();
            let mut details = String::new();
            for (position, proposal) in queue.proposals.iter().enumerate() {
                let subject = escape_text(&proposal.subject);
                let predicate = escape_text(&proposal.predicate);
                let object = escape_text(&proposal.object);
                let quote = escape_text(&proposal.quote);
                let file = escape_text(
                    proposal
                        .path
                        .rsplit(['/', '\\'])
                        .next()
                        .unwrap_or(&proposal.path),
                );
                let path = escape_text(&proposal.path);
                // Use only a fixed-format ID in command snippets and disclosure keys.
                let id = &proposal.proposal_id;
                let valid = id.strip_prefix("proposal_").is_some_and(|digest| {
                    digest.len() == 32
                        && digest
                            .bytes()
                            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
                });
                let disclosure = if valid {
                    format!("proposal-path-{id}")
                } else {
                    format!("proposal-path-unavailable-{position}")
                };
                let item = format!("detail-{disclosure}");
                let _ = write!(
                    selectors,
                    r#"<button type="button" aria-pressed="false" aria-controls="{item}" data-review-item="{item}" data-focus-key="select-{disclosure}"><bdi>{subject}</bdi><span>{predicate}</span><bdi>{object}</bdi></button>"#
                );
                let _ = write!(
                    details,
                    r#"<li id="{item}">
<h3 class="review-direction"><bdi>{subject}</bdi> → <code>{predicate}</code> → <bdi>{object}</bdi></h3>
<blockquote dir="auto">{quote}</blockquote>
<p class="review-source"><span class="label">Source</span> <code dir="auto">{file}</code>
<span class="muted">Characters {}–{}</span></p>
<details class="review-source" id="{disclosure}"><summary data-focus-key="{disclosure}">Full source path</summary><code dir="auto">{path}</code></details>"#,
                    proposal.start, proposal.end
                );
                if let Some(source_id) = &proposal.source_id {
                    let _ = write!(
                        details,
                        r#"<button type="button" class="review-open-source" data-review-source="{}" data-focus-key="open-{disclosure}">Inspect source</button>"#,
                        escape_text(source_id)
                    );
                }
                if valid {
                    let _ = write!(
                        details,
                        r#"<dl class="review-commands">
<dt>Approve</dt><dd><code>rust-rag --state-dir "&lt;state-dir&gt;" graph-review approve {id}</code></dd>
<dt>Dismiss</dt><dd><code>rust-rag --state-dir "&lt;state-dir&gt;" graph-review dismiss {id}</code></dd>
</dl>"#
                    );
                } else {
                    details.push_str(
                        r#"<p class="muted">This proposal has an invalid identifier.</p>"#,
                    );
                }
                details.push_str("</li>");
            }
            let _ = write!(
                html,
                r#"<div class="review-desk"><div class="review-selector" role="group" aria-label="Proposed links">{selectors}</div><ol class="review-list">{details}</ol></div>"#
            );
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
  <h3>{name}</h3><span class="dot" data-tone="{tone}">{label}</span><span class="count">{completed} / {eligible}</span>
"#
    );
    if *eligible > 0 {
        let _ = write!(
            html,
            r#"<meter min="0" max="{eligible}" value="{completed}" aria-label="{name} coverage"></meter>"#
        );
    }
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

fn unconfigured(html: &mut String, name: &str, id: &str) {
    let _ = write!(
        html,
        r#"<div class="lane" id="{id}" data-state="unconfigured"><h3>{name}</h3><span class="dot">Not configured</span><span class="count">—</span></div>"#
    );
}

struct Attention {
    title: &'static str,
    tone: &'static str,
    detail: String,
    target: &'static str,
    action: &'static str,
}

fn attention(snapshot: &Snapshot, review: Option<&anyhow::Result<ReviewQueue>>) -> Vec<Attention> {
    use history::IngestStatus;

    let mut items = Vec::new();
    let history = &snapshot.history;
    if history.collection_enabled == Some(true) {
        if let Some(ingest) = &history.latest_ingest {
            let needs_review = !matches!(ingest.status, IngestStatus::Completed)
                || ingest.sync_blocked == Some(true)
                || ingest.inventory_complete == Some(false)
                || ingest.counts.as_ref().is_some_and(|counts| {
                    counts.failed_sources > 0
                        || counts.deferred_sources > 0
                        || counts.skipped_pdfs > 0
                        || counts.skipped_docx > 0
                        || counts.skipped_xlsx > 0
                });
            if needs_review {
                let detail = ingest.counts.as_ref().map_or_else(
                    || "Completion or file counts could not be confirmed.".to_owned(),
                    |counts| {
                        if counts.retained_previous_sources > 0 {
                            return format!(
                                "{} failed · {}",
                                counts.failed_sources,
                                retained_revisions(counts.retained_previous_sources)
                            );
                        }
                        format!(
                            "{} failed · {} deferred · {} skipped by file type",
                            counts.failed_sources,
                            counts.deferred_sources,
                            counts
                                .skipped_pdfs
                                .saturating_add(counts.skipped_docx)
                                .saturating_add(counts.skipped_xlsx)
                        )
                    },
                );
                items.push(Attention {
                    title: "Ingest needs review",
                    tone: "warn",
                    detail,
                    target: "latest-ingest",
                    action: "Inspect ingest",
                });
            }
        }
    }
    if let Some(index) = &snapshot.index {
        for (lane, title, target, command) in [
            (
                &index.graph,
                "Graph extraction pending",
                "graph-coverage",
                "extract-graph",
            ),
            (
                &index.vector,
                "Vector indexing pending",
                "vector-coverage",
                "embed",
            ),
        ] {
            if let Some(lane) = lane.as_ref().filter(|lane| lane.pending > 0) {
                let action = if snapshot.build.model_endpoint || snapshot.build.native_embeddings {
                    lane_action(lane, command)
                } else {
                    "Use a build with model support to resume.".into()
                };
                items.push(Attention {
                    title,
                    tone: "warn",
                    detail: format!("{} passages pending. {action}", lane.pending),
                    target,
                    action: "See coverage",
                });
            }
        }
    }
    match review {
        Some(Err(_)) => items.push(Attention {
            title: "Graph review unavailable",
            tone: "warn",
            detail: "The dashboard will retry the review queue.".into(),
            target: "graph-review",
            action: "See review queue",
        }),
        Some(Ok(queue)) if !queue.proposals.is_empty() => items.push(Attention {
            title: "Graph links awaiting review",
            tone: "warn",
            detail: if queue.has_more {
                format!("{}+ proposals.", queue.proposals.len())
            } else {
                format!("{} proposals.", queue.proposals.len())
            },
            target: "graph-review",
            action: "Open proposals",
        }),
        _ => {}
    }
    if history.collection_enabled != Some(true) || history.state == "unavailable" {
        let (title, detail) = if history.state == "disabled" {
            (
                "Activity recording is off",
                "The CLI records aggregate activity only when you opt in.",
            )
        } else {
            (
                "Activity history is unavailable",
                "Recent ingest and query observations could not be confirmed.",
            )
        };
        items.push(Attention {
            title,
            tone: "neutral",
            detail: detail.into(),
            target: "recent-activity",
            action: "About history",
        });
    } else if let Some(queries) = &history.recent_queries {
        for (title, count, label, tone) in [
            (
                "Recent query errors",
                queries.errors,
                "queries failed",
                "error",
            ),
            (
                "Reranker used its fallback",
                queries.rerank_fallbacks.unwrap_or(0),
                "rerank attempts fell back",
                "warn",
            ),
        ] {
            if count > 0 {
                items.push(Attention {
                    title,
                    tone,
                    detail: format!("{count} {label} in the recorded window."),
                    target: "recorded-queries",
                    action: "Inspect queries",
                });
            }
        }
    }
    items
}

fn next(html: &mut String, items: &[Attention]) {
    let _ = write!(
        html,
        r#"<section class="panel" id="next-steps"><header class="panel-head"><h2>Needs attention</h2><span class="attention-count">{}</span></header>"#,
        items.len()
    );
    if items.is_empty() {
        html.push_str(r#"<p class="panel-body muted">No issues in the current coverage and recorded activity.</p></section>"#);
        return;
    }
    html.push_str(r#"<ul class="attention-list">"#);
    for item in items {
        let _ = write!(
            html,
            r##"<li><h3><span class="dot" data-tone="{}"></span>{}</h3><p>{}</p><div class="attention-actions"><a href="#{}" data-focus-key="{}">{}</a>"##,
            item.tone, item.title, item.detail, item.target, item.title, item.action
        );
        if item.target == "latest-ingest" {
            html.push_str(r##"<a data-live-action href="#ingest-review" data-focus-key="inspect-ingest-files">Review affected files</a>"##);
        }
        html.push_str("</div></li>");
    }
    html.push_str("</ul></section>");
}

fn lane_action(lane: &Coverage, command: &str) -> String {
    match &lane.scheduling {
        Some(scheduling) if scheduling.provider_paused_until_unix.is_some() => {
            "The provider is paused. Check the model endpoint.".into()
        }
        Some(scheduling) if scheduling.ready == 0 && scheduling.quarantined > 0 => {
            "Check the model output before retrying quarantined work.".into()
        }
        Some(scheduling) if scheduling.ready == 0 => "Work is deferred or running.".into(),
        _ => format!("Run <code>{command}</code> to continue."),
    }
}

fn clock(seconds: i64) -> String {
    time::OffsetDateTime::from_unix_timestamp(seconds).map_or_else(
        |_| "—".to_owned(),
        |t| {
            format!(
                "{:04}-{:02}-{:02} {:02}:{:02}:{:02} UTC",
                t.year(),
                t.month() as u8,
                t.day(),
                t.hour(),
                t.minute(),
                t.second()
            )
        },
    )
}

fn timestamp(seconds: i64) -> String {
    let exact = clock(seconds);
    format!(r#"<time title="{exact}">{exact}</time>"#)
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
        "available" => ("Recording on", ""),
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
  <header class="panel-head"><h2>Recent activity</h2><span class="section-meta">{label}</span></header>
"#
    );
    if !note.is_empty() {
        let _ = write!(html, r#"<p class="activity-note">{note}</p>"#);
    }
    if history.recent_queries.is_none() && history.latest_ingest.is_none() {
        html.push_str("</section>");
        return;
    }
    html.push_str("<div class=\"activity-body\">");
    queries(html, history.recent_queries.as_ref());
    ingest(html, history.latest_ingest.as_ref());
    html.push_str(r#"</div><details class="activity-policy" id="activity-policy"><summary data-focus-key="activity-policy">History retention</summary><p>Up to seven days or 2,000 CLI observations.</p></details></section>"#);
}

fn overview_charts(html: &mut String, history: &history::Summary) {
    let window = if history.collection_enabled == Some(false) {
        "Retained history"
    } else {
        "Recorded CLI activity"
    };
    let _ = write!(
        html,
        r#"<div class="overview-charts" id="overview-analytics" aria-label="Operational charts">
<section class="panel chart chart-latency">
  <header class="panel-head"><h2>Query speed</h2><span class="section-meta">{window}</span></header>"#,
    );
    if let Some(queries) = &history.recent_queries {
        let samples = &queries.recent_durations_micros;
        let sequence = samples
            .iter()
            .map(i64::to_string)
            .collect::<Vec<_>>()
            .join(",");
        let median = milliseconds(queries.median_duration_micros);
        let p95 = milliseconds(queries.p95_duration_micros);
        let max = samples.iter().copied().max().unwrap_or(0);
        let max_label = milliseconds(max);
        let latest = milliseconds(queries.latest.duration_micros);
        let points = samples.len();
        let _ = write!(
            html,
            r##"<dl class="chart-kpis">
  <div><dt>Median</dt><dd>{median}</dd></div><div><dt>p95</dt><dd>{p95}</dd></div><div><dt>Latest</dt><dd>{latest}</dd></div>
</dl>
<div class="chart-plot" aria-hidden="true">
  <div class="plot-labels"><span>{max_label}</span><span>0 ms</span></div>
  <canvas width="880" height="260" data-timing-samples="{sequence}"></canvas>
</div>
<footer class="chart-foot"><span>Last {points} queries · engine time</span><a data-chart-link data-focus-key="chart-query-detail" href="#query-details">Timing details</a></footer>"##,
        );
    } else {
        html.push_str(r#"<p class="chart-empty">No recorded query timings.</p>"#);
    }
    html.push_str(r#"</section><section class="panel chart chart-outcomes"><header class="panel-head"><h2>Query outcomes</h2></header>"#);
    if let Some(queries) = &history.recent_queries {
        let count = queries.count;
        let _ = write!(
            html,
            r#"<div class="outcome-chart"><div class="outcome-ring">
  <canvas width="240" height="240" data-outcome-counts="{},{},{},{}" aria-hidden="true"></canvas>
  <span><strong>{count}</strong><small>queries</small></span>
</div><dl class="chart-legend">"#,
            queries.evidence, queries.weak_match, queries.abstained, queries.errors,
        );
        for (label, count, class) in query_categories(queries) {
            let _ = write!(
                html,
                r#"<div class="{class}"><dt>{label}</dt><dd>{count}</dd></div>"#,
            );
        }
        html.push_str(r##"</dl></div><footer class="chart-foot"><a data-chart-link data-focus-key="chart-outcome-detail" href="#recorded-queries">Query activity</a></footer>"##);
    } else {
        html.push_str(r#"<p class="chart-empty">No recorded query outcomes.</p>"#);
    }
    html.push_str(r#"</section><section class="panel chart chart-ingest"><header class="panel-head"><h2>Latest ingest</h2></header>"#);
    if let Some(ingest) = &history.latest_ingest {
        if let Some(counts) = &ingest.counts {
            let changes = [
                ("Updated", counts.updated_sources, "evidence"),
                ("Removed", counts.removed_sources, "abstained"),
                ("Failed", counts.failed_sources, "error"),
                (
                    "Prior revisions kept",
                    counts.retained_previous_sources,
                    "weak",
                ),
            ];
            let scale = changes
                .iter()
                .map(|(_, count, _)| *count)
                .max()
                .unwrap_or(0)
                .max(1);
            html.push_str(r#"<dl class="change-bars">"#);
            for (label, count, class) in changes {
                let _ = write!(
                    html,
                    r#"<div class="{class}"><dt>{label}</dt><dd>{count}</dd><meter min="0" max="{scale}" value="{count}" aria-hidden="true"></meter></div>"#,
                );
            }
            html.push_str("</dl>");
        } else {
            html.push_str(
                r#"<p class="chart-empty">The latest ingest has no recorded file counts.</p>"#,
            );
        }
        html.push_str(r##"<footer class="chart-foot"><a data-chart-link data-focus-key="chart-ingest-detail" href="#ingest-details">Ingest details</a></footer>"##);
    } else {
        html.push_str(r#"<p class="chart-empty">No recorded ingests.</p>"#);
    }
    html.push_str("</section></div>");
}

fn query_categories(queries: &history::QuerySummary) -> [(&'static str, i64, &'static str); 4] {
    [
        ("Evidence", queries.evidence, "evidence"),
        ("Weak match", queries.weak_match, "weak"),
        ("Abstained", queries.abstained, "abstained"),
        ("Error", queries.errors, "error"),
    ]
}

fn queries(html: &mut String, queries: Option<&history::QuerySummary>) {
    html.push_str(
        "<section class=\"activity-block\" id=\"recorded-queries\"><h3>Query outcomes</h3>",
    );
    if let Some(queries) = queries {
        let count = queries.count;
        let median = milliseconds(queries.median_duration_micros);
        let categories = query_categories(queries);
        let _ = write!(
            html,
            r#"<p class="activity-lede"><strong>{count}</strong> recorded</p><div class="outcome-cluster"><div class="outcome-strip" aria-hidden="true">"#
        );
        // Equal-width cells approximate proportions. The adjacent legend gives exact counts.
        const CELLS: i64 = 48;
        let mut end = 0;
        let mut drawn = 0;
        for (_, category_count, class) in categories {
            end += category_count;
            let next = (end * CELLS + count / 2) / count.max(1);
            for _ in drawn..next {
                let _ = write!(html, r#"<span class="{class}"></span>"#);
            }
            drawn = next;
        }
        html.push_str("</div><div class=\"outcomes\">");
        for (label, count, class) in categories {
            let _ = write!(
                html,
                "<div class=\"{class}\"><span>{label}</span><strong class=\"count\">{count}</strong></div>"
            );
        }
        let first = clock(queries.first_observed_unix_seconds);
        let last = clock(queries.last_observed_unix_seconds);
        let window = if first == last {
            first
        } else {
            format!("{first} to {last}")
        };
        html.push_str(r#"</div></div><details id="query-details" class="activity-details"><summary data-focus-key="query-details">Latest query and timings</summary>"#);
        let _ = write!(
            html,
            r#"<p class="muted">Recorded window · {window}</p><dl class="kv"><dt>Median engine time</dt><dd>{median}</dd>"#
        );
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
        html.push_str(r##"</dl><p class="foot">Engine time excludes opening the index, recording history and writing CLI output.</p><a data-live-action class="inline-action" href="#source-lookup" data-focus-key="review-query-source">Open a source from query output</a></details>"##);
    } else {
        html.push_str("<p class=\"muted\">No recorded queries in this window.</p>");
    }
    html.push_str("</section>");
}

fn ingest(html: &mut String, observation: Option<&history::IngestObservation>) {
    html.push_str("<section class=\"activity-block\" id=\"latest-ingest\"><h3>Latest ingest</h3>");
    if let Some(ingest) = observation {
        let status = match enum_key(&ingest.status).as_str() {
            "completed" => "Completed",
            "partial" => "Partial",
            "error" => "Error",
            "completion_unobserved" => "Completion unobserved",
            _ => "Unavailable",
        };
        let started = timestamp(ingest.started_unix_seconds);
        let operation = if ingest.sync_requested {
            "Sync ingest"
        } else {
            "Ingest"
        };
        let _ = write!(
            html,
            r#"<div class="ingest-state"><p class="activity-lede"><strong>{operation} · {status}</strong></p><p class="ingest-time">Started {started}</p></div>"#,
        );
        if let Some(counts) = &ingest.counts {
            let _ = write!(
                html,
                r#"<dl class="ingest-totals"><div><dt>Updated</dt><dd>{}</dd></div><div><dt>Removed</dt><dd>{}</dd></div><div><dt>Failed</dt><dd>{}</dd></div></dl>"#,
                counts.updated_sources, counts.removed_sources, counts.failed_sources
            );
        }
        if ingest.sync_requested && ingest.complete_reconciliation == Some(false) {
            let retained = ingest
                .counts
                .as_ref()
                .map_or(0, |counts| counts.retained_previous_sources);
            let _ = write!(
                html,
                "<p class=\"ingest-exception\">Full sync incomplete{}.</p>",
                if retained > 0 {
                    format!(" · {}", retained_revisions(retained))
                } else {
                    String::new()
                }
            );
        }
        html.push_str(r#"<details id="ingest-details" class="activity-details"><summary data-focus-key="ingest-details">Ingest and sync details</summary><dl class="kv">"#);
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
                ("Unchanged", counts.unchanged_sources),
                ("Deferred", counts.deferred_sources),
                ("Retained prior revisions", counts.retained_previous_sources),
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
        html.push_str("</dl></details>");
    } else {
        html.push_str("<p class=\"muted\">No recorded ingest in this window.</p>");
    }
    html.push_str("</section>");
}

fn retained_revisions(count: i64) -> String {
    let noun = if count == 1 { "revision" } else { "revisions" };
    format!("{count} prior {noun} kept")
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
                chunk_id: "test-chunk".into(),
                source_id: None,
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

    #[test]
    fn recorded_reranker_fallback_has_an_attention_item() {
        let mut snapshot = snapshot();
        snapshot.history.state = "available";
        snapshot.history.collection_enabled = Some(true);
        snapshot.history.recent_queries = Some(history::QuerySummary {
            count: 1,
            first_observed_unix_seconds: 1,
            last_observed_unix_seconds: 1,
            median_duration_micros: 11,
            p95_duration_micros: 11,
            recent_durations_micros: vec![11],
            evidence: 1,
            weak_match: 0,
            abstained: 0,
            errors: 0,
            rerank_attempted: 1,
            rerank_fallbacks: Some(1),
            latest: serde_json::from_value(serde_json::json!({
                "observed_at_unix_seconds": 1, "mode": "lexical", "status": "evidence",
                "graph_requested": false, "reranking": "fallback", "duration_micros": 11,
            }))
            .unwrap(),
        });
        let items = attention(&snapshot, None);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].title, "Reranker used its fallback");
        assert_eq!(items[0].target, "recorded-queries");
        snapshot.history.collection_enabled = Some(false);
        snapshot.history.state = "disabled";
        assert_eq!(
            attention(&snapshot, None)[0].title,
            "Activity recording is off"
        );
    }

    #[test]
    fn source_preview_reports_truncation_and_can_reopen_previous_passages() {
        let html = source_passages(
            "test-source",
            "synthetic.md",
            "test-revision",
            vec![SourcePassage {
                id: "test-chunk".into(),
                start: 1200,
                end: 3000,
                page: None,
                anchor: None,
                text: "x".repeat(1200),
                characters: 1800,
            }],
            2,
        );
        assert!(html.contains("Truncated at 1,200 of 1800 characters"));
        assert!(html.contains("Offsets 1200–3000"));
        assert!(html.contains("test-chunk"));
        assert!(html.contains(r#"data-passage-page="1">Previous passages"#));
    }
}
