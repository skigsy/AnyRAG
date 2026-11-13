# Any RAG command reference

The root Cargo workspace builds `rust-rag`. [Install](../../docs/setup.md#install-from-this-repository) · [Status and platform verification](../../status.md) · [Architecture](../../docs/architecture.md)

## Build and try it

The [source review and citation walkthrough](../../docs/setup.md#review-sources-and-explain-results)
shows `ingest --review`, `query --explain`, and `citation <chunk_id> --revision <revision>`
on a personal index. Use the `v1.0.0` standalone Windows ZIP or a build from this
checkout. The `v1.0.0-rc.2` executables predate these commands.

~~~sh
cargo build --locked --release
rust-rag init
rust-rag doctor --json
rust-rag ingest /path/to/notes --json
rust-rag query "Which notes discuss cold storage?" --json
~~~

Run the executable from Cargo's target directory or add its install location to PATH. Use `--state-dir` to choose a private index directory. Evidence JSON reports the ID scheme as `rust-rag-v1`.

## Native PDF, graph, and vector preview

Use `cargo install --path crates/rust-rag --locked --features pdf,model` for these options.

~~~sh
rust-rag init --upgrade
rust-rag ingest /path/to/notes --pdf
rust-rag extract-graph --model-endpoint /private/chat.json
rust-rag query "CedarPump" --graph
rust-rag embed --embedding-endpoint /private/embeddings.json
rust-rag query "replacement component" --mode hybrid --embedding-endpoint /private/embeddings.json
rust-rag answer "CedarPump" --graph --model-endpoint /private/chat.json
~~~

Typed extraction and answers use an explicit [chat endpoint](../../infra/model-endpoint.example.json) and a trusted destination. Embeddings use an [embedding endpoint](../../infra/embedding-endpoint.example.json) or [in-process inference](../../docs/setup.md#local-semantic-search). Hosted model endpoints have separate qualification. Synthetic integration checks use loopback.

Add `"short_answer_citations": true` to the chat endpoint profile to use short
citation labels inside each model call. AnyRAG validates the labels and
returns the full chunk IDs in the answer. An unknown label fails answer
validation. The option defaults to `false`. [Paired benchmark](../../docs/benchmarks.md#compact-mcp-evidence)

Rust answers send each admitted passage to the model once and reject
citations to labels the request did not carry, as in earlier releases. Answers
now also report what was sent. The output includes `delivery`, a manifest
without passage text that lists the `sent` items with their request label,
collection, source, revision and range, and the `omitted` items with a reason:
`item_limit`, `missing_id_or_text`, `duplicate` or `context_budget`. When no
evidence was found, `delivery` is `{"considered":0,"sent":[],"omitted":[]}`.
`cited_evidence` resolves each accepted citation to its canonical chunk,
source, revision and range from that same request. Ranges count Unicode scalar
values, not UTF-16 units; in JavaScript, slice `[...text]`.

The `answer` command reopens each cited passage at its sent revision and range
before it returns. If the source changed, it withholds the answer: the JSON
report has status `unavailable`, reason `citation_unavailable`, no answer or
citations, and `delivery`, and the command exits with code 1. A shared
`/v1/answer` request does not reopen citations. It holds the service read lock
from retrieval until it returns, so no sync can retire a revision in between.

If the model call, its reply or the recheck fails, `answer --json` exits with
an error and the error output includes `delivery`. `sent` lists what the
request carried, which may not have reached a model that was unreachable.
These failures happen before a request is prepared, so they carry no
`delivery` and nothing was sent: a question that alone exceeds the answer
context budget, a model endpoint file that cannot be opened, and a shared
answer whose question exceeds the budget. A missing `delivery` does not always
mean nothing was sent. The shared service checks access again after the model
call. If that check fails, for example with `ACCESS_CHANGED` because a grant
or destination policy changed during the call, the response is only the error,
without `delivery`, so the caller learns nothing about evidence it may no
longer see. Its passages may already have reached the answer endpoint.
A valid citation shows that the model named a passage it received. It does not
show that the passage supports the answer.

`rust-rag extract-graph --source-clauses --model-endpoint /private/chat.json` stages proposed links on an owner-local Markdown and text index. It reads the active file revision and derives direction from declared dependency, requirement, ownership, membership and usage forms in ordinary sentences. A schema-capable loopback model screens proposals using up to 12,000 characters of the source document. Use `graph-review list` to see the quote and source path, then `graph-review approve <proposal-id>` to publish a confirmed link or `graph-review dismiss <proposal-id>` to remove it from the queue. Confirm against the current source and sync after edits. The mode uses a separate graph fingerprint. [Full review workflow](../../docs/setup.md#opt-into-source-first-extraction)

Run `init --upgrade` to enable durable enrichment on an older advanced index. Both enrichment commands report ready, deferred, leased, and quarantined work in `progress`. A provider failure yields to other passages on the next run. Three invalid item responses quarantine that revision and model combination. `--retry` clears deferred and quarantined items for the chosen model. It respects active leases and provider rate limits. Inspect the exact source spans and [rollback and deletion behavior](../../docs/setup.md#update-remove-export).

## Word citations

Build with `--features docx` and ingest with `--docx` into an advanced index. Citation anchors are one-based `p:N` for body paragraphs and `table:T:row:R:cell:C:p:N` for paragraphs inside physical table cells. Offsets count Unicode code points in the extracted view, with two separator code points between extracted paragraphs. Quotes and anchors stay tied to the original file's revision hash.

The worker accepts up to 16 MiB of input and checks archive size, entry count, ZIP headers, XML depth, and extracted output. The initial view uses the standard main document path, reads simple tables in document order, and suppresses hidden runs, including recognized inherited hiding. A malformed or unsupported replacement retains the prior revision. A valid blank Word document retires it. [Format scope](../../docs/setup.md#word-documents) · [Isolation controls](../../SECURITY.md)

## Local index dashboard

The dashboard build adds a local browser view and reports. Build from source with `--features dashboard`, or check that your binary reports `local_dashboard: true` in `capabilities --json`. Use an existing personal index.

~~~sh
rust-rag --state-dir /private/index dashboard
rust-rag --state-dir /private/index health --json
rust-rag --state-dir /private/index health --html /private/health.html
~~~

Pair the browser with the access code shown in your terminal. Use `dashboard --no-open` to open the displayed address yourself. The dashboard uses IPv4 loopback and refreshes while its tab is visible. It shows the time of the last successful refresh when the index cannot be read. Press Ctrl+C in the dashboard terminal to stop the session.

Use `dashboard --review-queue` to show pending graph proposals and their exact quotes for this session. Use the terminal with the same state directory to apply review decisions.

On a native Linux Docker host, a portable image that includes the dashboard can read a mounted private index. Replace the host path and image tag, and open the displayed address in a browser on the same host.

~~~sh
docker run --rm --init -it --network host --no-healthcheck --read-only \
  --user "$(id -u):$(id -g)" --workdir /index \
  --mount "type=bind,source=/absolute/private/index,target=/index,readonly" \
  any-rag-local:1.1.0 --state-dir /index dashboard --no-open
~~~

Use a native build for a desktop install when Docker cannot share the host's loopback interface.

The index and coverage views work with history off. Enable history per index when you want to see recent CLI ingest and query activity.

~~~sh
rust-rag --state-dir /private/index history enable
rust-rag --state-dir /private/index history disable
rust-rag --state-dir /private/index history clear
~~~

Disable stops recording and keeps recent observations. Clear removes observations and keeps your recording choice. The private state folder holds separate, bounded files for ingest and query observations. The dashboard reports up to 2,000 observations from the past seven days. Older query entries remain in the bounded file until overwritten or cleared. History reflects recorded operations. A failed write or interrupted shutdown can leave gaps in query history. Reports contain counts, statuses, and timing. Engine time for queries and ingest excludes opening the index, recording history and writing CLI output. Keep these files and reports private if those details are sensitive. SDK, MCP and shared service requests are outside this history scope. After replacing or restoring a personal index, enable history again to bind a fresh window when the dashboard reports a mismatch.

## Read-only MCP search

Build with `--features mcp` and use `rust-rag --state-dir /private/index mcp` as the stdio subprocess. Use `search` for ranked passages and `open_citation` with a returned chunk ID and revision to reopen one exact passage. See the [client setup and data boundary](../../docs/setup.md#search-a-personal-index-from-an-mcp-client).

Set `"format": "compact"` on graph-off searches to receive ranked passages
with full citation IDs and a shared source table. The default format includes
the full retrieval diagnostics. [Compact response and citation recipe](../../docs/setup.md#search-a-personal-index-from-an-mcp-client)

The MCP client owns delivery to its model. Each result carries the same
evidence as `structuredContent` and as JSON text in `content`; forward one of
them, not both. [Reference client](../../examples/mcp-answer-client.mjs)

## Boundaries

The default binary does not fetch models or process files through an API. Optional endpoint configuration turns on separate network egress. Parsing can skip unsupported or invalid inputs. Scanned-image PDFs need a separate OCR stage. Indexing runs on your files; the repo's smoke fixtures are invented. Shared collection grants apply in the HTTP service, never in this owner's CLI or SDK.

## Windows pilot results

The native text prototype, built with the same platform toolchain for a synthetic Markdown corpus, was 2.83 MB and ran one copied executable query in 12 ms median. The frozen Python comparison was 11.83 MB and 693 ms median on the same machine. This was a warm-cache, single-note startup comparison and does not measure model inference or service throughput. [Inspection and benchmark evidence](../../docs/project/inspect.md) · [Verification status](../../status.md)

## Maintainer check

Build and test with the matching feature flags. Python is used here to drive the copied Rust executable through synthetic inputs; it is not installed by Any RAG.

~~~sh
cargo test --locked --features pdf,docx,model
cargo build --release --locked --features pdf,docx,model
python crates/rust-rag/check.py target/release/rust-rag --check-advanced
~~~

Use `.exe` on Windows. The maintainer check validates exact quotes, hashes, offsets, source lifecycle, interrupted ingestion, and optional PDF, graph and model routes.
