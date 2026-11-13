# Setup and operation

Pick a workload in the [decision tree](field-guide.md#decision-tree). Use `rust-rag plan <profile> --json` for a recipe or `rust-rag capabilities --json` to inspect what your executable contains. Evaluation is optional for every setup. Verify access and data movement for your workload.

Choose extraction, embeddings and answer generation independently. The extractor may see every eligible passage processed in its batch. An embedding service sees indexed text and search queries. An answer service sees the question and selected evidence. You can run any of these locally and leave generation off. An external model needs approval for its own data and destination, even when the index and the other models are local.

## Install from this repository

Install Rustup, then run Cargo from the checkout so it uses the pinned Rust toolchain. Windows source builds need the MSVC x64 C++ build tools. Linux builds need a C compiler and standard linker. On Ubuntu, the `build-essential` package supplies both. The binary bundles SQLite. [Rustup installation](https://rust-lang.github.io/rustup/installation/index.html) · [Pinned toolchain](../rust-toolchain.toml)

~~~sh
cargo install --path crates/rust-rag --locked
rust-rag --help
~~~

Choose the features you need. Re-run install with the same options and `--force` if replacing an existing binary.

| Cargo feature | Adds |
| --- | --- |
| None | Markdown, text, Obsidian exclusions, SQLite FTS5, exact evidence, update and removal. |
| `pdf` | Bounded text-layer PDF worker and page citations. No OCR. |
| `ocr` | Experimental Linux worker for scanned and table PDFs. Uses a fresh index and a separate offline worker image. |
| `docx` | Bounded Word main-body extraction and paragraph or table-cell citations. |
| `xlsx` | Optional personal-index reader for Excel cells with worksheet and cell citations. |
| `model` | OpenAI-compatible endpoint for embeddings, typed graph extraction, or cited answers, plus a model-free graph demo. |
| `inference` | Pinned BGE weights and in-process ONNX embeddings. Operator supplies and pins the ONNX Runtime shared library. |
| `mcp` | Read-only stdio search and citation reopening for an existing personal index. |
| `dashboard` | Local read-only browser dashboard and health snapshots for a personal index, with opt-in CLI activity history. |
| `shared` | Authenticated HTTP, independently versioned policy, trusted collections, and local backup and recovery. |
| `aws` | Shared service with optional S3 and Neptune adapters. See [deployment verification](../status.md). |

## Install the Windows ZIP

Put the standalone Windows x64 ZIP and its release checksum manifest in the same local folder. In PowerShell, change to that folder and run:

~~~powershell
$archive = 'rust-rag-1.0.0-windows-x64.zip'
$expected = (Get-Content -LiteralPath 'SHA256SUMS.txt' |
  Where-Object { $_ -match ('^[a-fA-F0-9]{64}  ' + [regex]::Escape($archive) + '$') } |
  ForEach-Object { ($_ -split '  ', 2)[0] })
if (!$expected -or (Get-FileHash -Algorithm SHA256 -LiteralPath $archive).Hash -ne $expected) {
  throw 'The ZIP does not match the release checksum.'
}
$install = Join-Path $env:LOCALAPPDATA ('AnyRAG\' + [guid]::NewGuid())
Expand-Archive -LiteralPath $archive -DestinationPath $install -ErrorAction Stop
$env:PATH = "$install;$env:PATH"
rust-rag --version
~~~

The ZIP supplies `rust-rag.exe` and bundles SQLite. The install folder and PATH change are local to your account and the PATH change applies to the current shell. Continue with the [source review walkthrough](#review-sources-and-explain-results) in that shell.

## Try personal search in the Linux image

On a Linux amd64 machine with Docker, [verify and load the v1.0.0 image](../infra/private-vm.md#load-a-reviewed-release-archive). In the same shell, as a regular user with Docker access, create a private workspace and run an offline search:

~~~sh
work=$(mktemp -d) || exit 1
mkdir -m 700 "$work/sources" "$work/index"
printf '%s\n' 'The workshop spare key is in locker 14.' > "$work/sources/workshop.md"

anyrag() {
  docker run --rm --pull=never --network none --read-only \
    --user "$(id -u):$(id -g)" \
    --mount "type=bind,src=$work/sources,dst=/sources,readonly" \
    --mount "type=bind,src=$work/index,dst=/index" \
    "$RUST_RAG_IMAGE" --state-dir /index "$@"
}

anyrag init
anyrag ingest /sources --review --json
anyrag query 'workshop spare key' --explain --json
~~~

Look for the locker 14 passage under `evidence` and its ranking under `explanation`. Copy that result's `chunk_id` and `revision` into `anyrag citation CHUNK_ID --revision REVISION --json` to reopen it. The container reads the notes through a read-only mount and keeps its index in `$work/index` between calls. For your own notes, set the source mount to the absolute folder path and choose a new private index directory.

## Choose your setup

The commands below use one private index per workload. Set a private path outside the repo and cloud sync folders. Quote actual paths if they contain spaces.

~~~sh
rust-rag --state-dir /path/to/private/index init --profile infra/local.profile.json
rust-rag --state-dir /path/to/private/index doctor --json
rust-rag --state-dir /path/to/private/index ingest /path/to/notes
rust-rag --state-dir /path/to/private/index query "What changed last week?" --json
~~~

For an Obsidian vault, use `infra/obsidian.profile.json` and a separate index. Any RAG ignores application metadata, version control folders, and trash. See the [vault recipe](field-guide.md#personal-obsidian-recipe).

To use PDFs, install with `--features pdf`, initialize with `--upgrade`, and ingest with `--pdf`. If you need graph or semantic retrieval, initialize the advanced index too. Use [the Rust command reference](../crates/rust-rag/README.md) for the next steps.

To try scanned PDF ingestion, use the [experimental offline Linux OCR recipe](../infra/ocr/README.md). It creates a fresh index with region citations and binds the index to the worker fingerprint. The native PDF feature handles text-layer documents. [OCR qualification status](../status.md#all-in-one-release-gates)

## Local graph for declared relationships

Use the `model,inference` features and a private advanced index for this owner-local setup. Start with explicit relationships such as “Aster Panel depends on Vale Relay” so each graph hop has its own source. Point the [chat profile](../infra/model-endpoint.example.json) at your trusted local model and choose the relations relevant to your workload. The documented Windows Vulkan and Linux CPU comparisons use pinned Qwen3-4B and [local BGE embeddings](#local-semantic-search). [Runtime, model and benchmark pins](evidence.md#reproduce-the-mixed-graph-quality-run)

~~~sh
rust-rag --state-dir /private/index extract-graph --model-endpoint /private/graph-local.json --json
rust-rag --state-dir /private/index query "Which controller depends on the relay using this part?" --graph --mode hybrid --model-cache /private/model --ort-library /trusted/onnxruntime --ort-sha256 <sha256> --limit 5 --json
~~~

Ingest the sources and run `embed` first. Use the actual platform library file path for `--ort-library`. Repeat the query without `--graph` and check both citations. Explicit source text still needs a direction check when the edge comes from a model. For broader prose, compare a stronger local or approved hosted extractor on a separate index. If a value or relation exists in authoritative records, use the record lookup first.

### Opt into source-first extraction

For declared dependency, requirement, ownership, membership and usage in owner-local Markdown or text, use a source build with `--features model,inference,dashboard`. The Windows standalone ZIP covers personal keyword search and the dashboard. Use a fresh advanced index containing only Markdown or text. The source-first lane derives direction from the source, uses a local model to screen proposals and waits for operator confirmation before publication. Unrecognized wording can produce an empty result. A document over 12,000 characters can remain pending.

Separate current material from historical or retracted material with an explicit Markdown heading such as `Historical operating notes`, `Retracted dependency records`, or `Wiring records (superseded)`. The status words are archived, historical, obsolete, retracted, superseded and withdrawn. Scope applies to nested sections until the next heading of the same or higher level. Use explicit labels such as `Retraction notice:` for unheaded material. The model can still propose teaching examples and retracted statements, or miss valid links. Check the quote in the source document and confirm only current relationships in the stated direction.

The Windows Vulkan reference uses Qwen3-14B Q4_K_M, llama.cpp b12207 and local BGE embeddings. It used an RTX 5080 with 16 GB VRAM. Verify the 9.0 GB weight file against SHA-256 `500a8806e85ee9c83f3ae08420295592451379b4f8cf2d0f41c15dffeb6b81f0`. Configure the [chat profile](../infra/model-endpoint.example.json) with a literal loopback URL, `"response_format": "schema"` and `"max_tokens": 512`. The verifier can receive the bounded full active document over loopback. [Pinned model](https://huggingface.co/Qwen/Qwen3-14B-GGUF/commit/c75e7b2d0234068f674a1bacf548ea32e27ccd29) · [Runtime pins](evidence.md#reproduce-the-mixed-graph-quality-run)

~~~sh
rust-rag --state-dir /private/index extract-graph --source-clauses --model-endpoint /private/graph-local.json --json
rust-rag --state-dir /private/index graph-review list --json
rust-rag --state-dir /private/index graph-review approve <proposal-id> --json
rust-rag --state-dir /private/index graph-review dismiss <proposal-id> --json
~~~

Use `dashboard --review-queue` with the same state directory to inspect pending proposals and their source quotes in your browser. The dashboard reads the queue. The terminal applies decisions. Pending and dismissed proposals are unavailable to graph traversal. Ordinary keyword and vector retrieval can still return their source passages. An approval checks the file against the indexed revision. Sync after an edit or deletion to retire affected proposals, approved links and citations. Extraction with a different model profile replaces the previous graph and review queue.

Extraction defaults to 32 passages. Repeat it while `progress.ready` is positive and inspect any deferred or quarantined work. Use the hybrid query above with graph off and on after confirming the relevant links. Check the ordered relation and open both exact citations. Sync an edit before re-extraction, then verify the retired citation is unavailable. The scoped recipe leaves shared extraction, broader paraphrases, deeper paths, other hardware and scanned documents to their own qualification.

## Review sources and explain results

Use the standalone Windows ZIP from `v1.0.0` or later, or a source build from this checkout for this walkthrough. The `v1.0.0-rc.2` executables predate these commands.

Use `ingest --review --json` to see what happened to individual files. Review rows show the path, relative to the selected folder when available, the outcome, and the reason when one applies. A failed update can retain a previously indexed revision. A `recheck_deferred` row means the indexed revision was kept and ingest will check its content on a later pass. A successful sync lists retired paths as `removed`.

This PowerShell walkthrough uses the default build and writes three invented files in a new temporary folder. It opens the exact passage for a reader's question, then tests the saved citation after an edit and a deletion.

~~~powershell
$work = Join-Path $env:TEMP ("anyrag-review-" + [guid]::NewGuid())
$source = Join-Path $work 'sources'
$state = Join-Path $work 'index'
New-Item -ItemType Directory -Path $source -Force | Out-Null
$note = Join-Path $source 'workshop.md'
[IO.File]::WriteAllText($note, 'After hours, the workshop spare key is in locker 14.')
[IO.File]::WriteAllText((Join-Path $source 'stock.csv'), 'item,count')
[IO.File]::WriteAllBytes((Join-Path $source 'broken.txt'), [byte[]]@(255))

rust-rag --state-dir $state init --json
rust-rag --state-dir $state ingest $source --review --json
$result = rust-rag --state-dir $state query 'Where is the workshop spare key?' --explain --json |
  ConvertFrom-Json
$result.explanation | ConvertTo-Json -Depth 8
$saved = $result.evidence[0]
rust-rag --state-dir $state citation $saved.chunk_id --revision $saved.revision --json

[IO.File]::WriteAllText($note, 'After hours, the workshop spare key is in locker 27.')
rust-rag --state-dir $state ingest $source --sync --review --json
rust-rag --state-dir $state citation $saved.chunk_id --revision $saved.revision --json
$current = (rust-rag --state-dir $state query 'Where is the workshop spare key?' --json |
  ConvertFrom-Json).evidence[0]
rust-rag --state-dir $state citation $current.chunk_id --revision $current.revision --json
Remove-Item -LiteralPath $note
rust-rag --state-dir $state ingest $source --sync --review --json
rust-rag --state-dir $state citation $current.chunk_id --revision $current.revision --json
~~~

The first ingest indexes `workshop.md`, skips the CSV and reports the invalid encoding in `broken.txt`. The first citation opens the locker 14 passage. The next two successful ingests retire the saved revision, first by replacing it with the locker 27 passage and then by removing the source. Both retired citation requests return an error and exit with a nonzero status. Citation reads use the committed index. Ingest changes or sync deletions before checking a citation against the latest indexed state. The revision and passage IDs are derived from the source and its content. Indexing the same bytes again at the same path can make the original citation active again.

In `query --explain`, match each result to its evidence by `chunk_id`. The explanation lists the observed retrieval signals, original rank before reranking and final rank. The evidence carries lexical scores, cosine similarity, term coverage or graph assertions when available. The query also reports warnings and coverage. A disabled stage was unselected, incomplete coverage reflects a limit or pending work, and a reranker fallback keeps the first-stage order with the reported reason. The explanation flag leaves evidence, ranking and citation IDs intact. Ranking signals help diagnose retrieval. Read the passage before using it to support an answer.

Graph results use `graph_support.assertion_status` to identify how a link was published. `extracted_unverified` is a model assertion published without review, `reviewed` is a confirmed link, and `declared` is a catalog link published by policy. `graph_support.path_contains_unverified_assertion` flags an unverified assertion in the path even when the displayed link was reviewed or declared. Inspect the cited source and the full path before using a relationship. [Graph evidence and limitations](evidence.md#graph-retrieval-and-link-accuracy)

Use this route with an owner-controlled personal index. The [shared service](#personal-notes-and-shared-deployments) checks current grants when readers reopen citations. Treat paths and passage text as private when deciding where to run a consuming harness.

## Word documents

Install with `--features docx`, run `rust-rag init --upgrade` for your private index, then run `rust-rag ingest <folder> --docx --json`. Combine `--docx` and `--pdf` when both features are built. Shared folder and S3 connectors admit Word files when built with `docx`.

The reader handles Strict and Transitional DOCX packages with the standard main document path. It indexes the visible main-body view of paragraphs, headings, and simple tables. Inserted text is included. Deleted and hidden runs, headers, footnotes, comments, and embedded content are excluded. Table anchors identify physical XML cells in reading order. Display layout, merged-cell reconstruction, and nested tables are outside this view. The reader skips external relationships and rejects malformed, encrypted, macro-enabled, or oversized packages. [Anchor format and limits](../crates/rust-rag/README.md#word-citations)

## Local semantic search

Install with `--features inference`. Inspect `rust-rag model-manifest` before fetching weights. Use `rust-rag model-install --cache <private-cache> --download` to download the pinned files or `--from <verified-files-folder>` to import them offline. Get an ONNX Runtime 1.24+ library from a trusted distribution, verify the release, then calculate the library's SHA-256. Pass the absolute library path and hash on each invocation.

~~~sh
rust-rag init --upgrade
rust-rag ingest /path/to/notes
rust-rag embed --model-cache /private/model --ort-library /trusted/libonnxruntime.so --ort-sha256 <sha256>
rust-rag query "Find the maintenance procedure" --mode hybrid --model-cache /private/model --ort-library /trusted/libonnxruntime.so --ort-sha256 <sha256> --json
~~~

Repeat `embed` while `progress.ready` is positive. If work is deferred, check `next_retry_unix` or `provider_paused_until_unix`. Inspect quarantined passages and use `embed --retry` with the same model arguments after correcting a problem. Linux operators should use trusted runtime paths and loader environments. Keyword and graph modes work independently of this library.

To change the active embedding model, run `embed` with the new model settings. For example, configure a trusted endpoint using the [embedding profile](../infra/embedding-endpoint.example.json), then run the following commands against the same private index.

~~~sh
rust-rag embed --embedding-endpoint /private/embeddings.json --json
rust-rag query "Find the maintenance procedure" --mode vector --embedding-endpoint /private/embeddings.json --json
~~~

Changing the model replaces the previous vectors. Source passages and their citations stay anchored to their indexed revisions. Wait for enrichment to finish before relying on complete vector coverage. Use a separate private index when comparing both models or when you want to keep the previous vectors available for rollback. The endpoint receives the text it embeds, so use a destination approved for that data.

### Local shared reranking

Opt into the pinned Ettin 150M model to reorder the first 15 authorized candidates using up to 68 tokens per question–passage pair. It keeps the first-stage order for an exact identifier found in the evidence. The signed Linux reference run gained 0.06138 nDCG@10 and added 124.02 ms at p95, with 23 timeouts in 120 requests. This benchmark used a synthetic reciprocal-procedure workload on a Ryzen 7 9800X3D under Ubuntu WSL. [Paired results](project/inspect.md#reranker-qualification)

Build from source with `shared,inference`. On x86-64 the runtime checks for AVX2. The reference used the publisher's AVX-512-named INT8 export and Linux x64 ONNX Runtime 1.24.4 on the CPU above. An AVX2-only host needs its own compatibility check. Verify the library SHA-256 `d132535d051344ff5c64c9c200004150559049a81ed330eb4422c1962fb6b7e4` before using the example profile. Use the corresponding trusted library and checksum on another platform.

Prepare the model on a local Linux filesystem, such as your Linux home directory under WSL. The build helper composes the quantized encoder and the classifier head from five publisher files, verifies every file, and writes the two pinned runtime files to a new directory. The tested preparation used Python 3.12, NumPy 2.5.4, ONNX 1.17.0 and protobuf 7.36.2. [Publisher revision](https://huggingface.co/cross-encoder/ettin-reranker-150m-v1/commit/cfa56e0d46b3a6ad6a710afbc3f202a5f2290aae)

~~~sh
model_root="$HOME/.local/share/rust-rag-models"
mkdir -p "$model_root/ettin-input"
publisher="https://huggingface.co/cross-encoder/ettin-reranker-150m-v1/resolve/cfa56e0d46b3a6ad6a710afbc3f202a5f2290aae"
curl -fL "$publisher/onnx/model_qint8_avx512.onnx" -o "$model_root/ettin-input/encoder.onnx"
curl -fL "$publisher/2_Dense/model.safetensors" -o "$model_root/ettin-input/dense.safetensors"
curl -fL "$publisher/3_LayerNorm/model.safetensors" -o "$model_root/ettin-input/norm.safetensors"
curl -fL "$publisher/4_Dense/model.safetensors" -o "$model_root/ettin-input/output.safetensors"
curl -fL "$publisher/tokenizer.json" -o "$model_root/ettin-input/tokenizer.json"
python3 -m venv "$model_root/ettin-build"
"$model_root/ettin-build/bin/python" -m pip install numpy==2.5.4 onnx==1.17.0 protobuf==7.36.2
"$model_root/ettin-build/bin/python" scripts/prepare_ettin_reranker.py \
  --source "$model_root/ettin-input" --output "$model_root/ettin150"
~~~

Run the helper from the repository root. Use absolute paths in the administrator's shared profile, with `model_dir` pointing to the new bundle.

~~~json
{
  "reranker": {
    "model": "ettin-150m",
    "model_dir": "/private/ettin150",
    "runtime_library": "/trusted/libonnxruntime.so.1.24.4",
    "runtime_sha256": "<verified library sha256>"
  }
}
~~~

Run `service-doctor` with the profile and look for `"reranker":"ready"`. To opt in, include `"rerank":true` in an authenticated `POST /v1/query` body. The model scores only the question and authorized candidate passages in process. When scoring is unavailable or times out, the service returns the first-stage order with a bounded fallback reason. Existing queries keep their first-stage order by default.

Profiles without `model` still select the earlier `minilm-l2-v2` export. Its latency met the target and its ranking quality missed the gate. [MiniLM results](project/inspect.md#reranker-qualification)

## Search an Excel workbook

Build with `--features xlsx`, initialize a personal index with `init --upgrade`, and run:

~~~sh
rust-rag --state-dir /private/index ingest /private/workbooks --xlsx --sync --review --json
rust-rag --state-dir /private/index query "azurite relay" --json
~~~

Each indexed value carries an anchor such as `sheet:Inventory!C7`. Pass the returned chunk ID and revision to `citation` to reopen the value. The reader uses visible worksheets and literal cell values. It skips formula cells, including their cached values. Numbers use their stored values. For a sum, join, or filtered report, use a spreadsheet or database tool and cite the contributing cells separately.

## Inspect what the dashboard indexed

Open the live personal dashboard and pair it with the access code from the terminal. In **Indexed sources**, browse the first 50 sources or paste a `source_id` from query output. Open a source to see its first 30 indexed passages, including their offsets and any page or cell anchor. Each preview shows up to 1,200 characters.

To review skipped, failed, or deferred files, save the JSON from `ingest --review --json` to a private file and select it in **Review skipped and failed files**. The browser reads the report locally.

## Search a personal index from an MCP client

Build with `mcp`. Add `pdf` if you want to index the text-layer PDF in the synthetic example. Keep the index outside the checkout and cloud sync folders. Substitute your own absolute paths.

~~~sh
cargo install --path crates/rust-rag --locked --features mcp,pdf
rust-rag --state-dir /private/rust-rag-index init --upgrade
rust-rag --state-dir /private/rust-rag-index ingest examples/library --pdf
~~~

In an MCP client that uses an `mcpServers` configuration, register the executable as a local stdio server. The executable path on Windows ends in `rust-rag.exe`.

~~~json
{
  "mcpServers": {
    "rust-rag": {
      "command": "/absolute/path/to/rust-rag",
      "args": ["--state-dir", "/private/rust-rag-index", "mcp"]
    }
  }
}
~~~

The server supports MCP `2025-11-25` over stdio. Try `search` with `{"question":"greenhouse","limit":5}`. Use `"graph":true` to include indexed typed graph evidence from an advanced personal index. The server uses the CLI's lexical retrieval and citation fields, including page and offsets when available. It accepts up to 20 results per call. If the response exceeds 512 KiB, retry with a smaller limit. [MCP stdio specification](https://modelcontextprotocol.io/specification/2025-11-25/basic/transports)

Use `open_citation` with the `chunk_id` and `revision` from a search hit. It returns the same exact evidence as the CLI citation command. An edit or deletion followed by a successful sync retires the old citation.

To exercise discovery and a PDF citation without a model, save the configuration as `mcp.json` and run the pinned MCP Inspector CLI. The recipe was exercised with Node.js 24.22.1.

~~~sh
npx --yes @modelcontextprotocol/inspector@2.14.0 --cli --config mcp.json --server rust-rag --method tools/list --format json
npx --yes @modelcontextprotocol/inspector@2.14.0 --cli --config mcp.json --server rust-rag --method tools/call --tool-name search --tool-arg 'question=demonstration pump cabinet nine' --format json
~~~

The tool reads one index chosen at launch. Each search sees committed index changes on its next call. Update or remove documents separately with the CLI. The server uses SQLite read-only access with rollback journal DELETE mode. Stop MCP before changing journal modes with external tools. If the server refuses a WAL index, restore or rebuild an index in the supported mode. [SQLite journal format](https://www.sqlite.org/fileformat.html#file_format_version_numbers)

The stdio server uses model-free search by default. Your MCP client receives passages and citation paths and may send them to its model provider. Use a local client and local model for data that must stay on the machine.

For a model-connected client, forward `structuredContent` when available and
use the JSON text in `content` as a compatibility fallback. Keep each
`chunk_id` and `revision` with the answer. To verify a quote against the
current index, run the citation command with both values.

~~~sh
rust-rag --state-dir /private/rust-rag-index citation CHUNK_ID --revision REVISION --json
~~~

If the source changed and the saved citation is unavailable, search the
current index again and cite the new result.

For a graph-off search, request compact evidence explicitly.

~~~json
{"question":"greenhouse","limit":5,"format":"compact"}
~~~

Each ranked passage has a full `chunk_id`, its text and source location.
Its `source` is a zero-based index into `sources`, which holds the source
path, hash and revision. Pass the passage's `chunk_id` and its source's
`revision` to the citation command. Use the default `full` format for
per-result ranking diagnostics or graph evidence. [Paired benchmark](benchmarks.md#compact-mcp-evidence)

AnyRAG controls delivery only for its own `answer` command and the shared
service's `answer_endpoint`. An MCP client decides what reaches its model.
Every `search` result returns the evidence twice for compatibility: as
`structuredContent` and as JSON text in `content`. A client that forwards both
sends each passage twice. Forward one representation, label passages per
request, accept only labels sent in that request, and resolve them in code to
`chunk_id`, source, `revision` and range before reopening the citation.
Ranges count Unicode scalar values, not UTF-16 units; in JavaScript, slice
`[...text]`.
[`examples/mcp-answer-client.mjs`](../examples/mcp-answer-client.mjs) is a
dependency-free reference client that does this against an OpenAI-compatible
chat endpoint and reports omitted passages without their text. Its offline
test uses synthetic files and a loopback endpoint that records each request.

~~~sh
RUST_RAG_BIN=/absolute/path/to/rust-rag node --test examples/mcp-answer-client.test.mjs
~~~

### Local personal reranking

Add the `inference` build feature and use the same pinned model files and trusted runtime described in [local shared reranking](#local-shared-reranking). Save the inner `reranker` object as a standalone JSON file with `model`, `model_dir`, `runtime_library` and `runtime_sha256`. Choose this profile when launching the command or MCP server.

~~~sh
rust-rag --state-dir /private/rust-rag-index query 'demonstration pump cabinet nine' --limit 5 --reranker-profile /private/reranker.json --json
rust-rag --state-dir /private/rust-rag-index mcp --reranker-profile /private/reranker.json
~~~

For MCP, append `--reranker-profile` and the profile path to the server's launch arguments, then add `"rerank":true` to a search call. The request can retrieve up to 20 candidates. Ettin scores at most the first 15. The response preserves complete citation objects, including PDF page and offsets, and reports `reranking.status`. A fallback returns the original candidate order.

## Personal notes and shared deployments

For team use, choose a trusted OIDC issuer and enforce grants with the [shared profile](../infra/shared.profile.example.json) and [private VM runbook](../infra/private-vm.md). For company Graph RAG, use `rust-rag plan company --json` to start with the portable shared service and select a target. The [deployment recipes](../infra/README.md#deployment-targets) describe host and provider requirements. The [reference](rag-reference.md) explains adapters you can integrate, including pgvector and rerankers.

Use the v1.0.0 portable Linux image or a source build with `shared` to enable a collection publication policy. A team can publish routine catalog links automatically and send prose, conflicts, or high-impact links to review. Reviewers use their own signed tokens and need read and review grants for the collection. [Company publication and review](../infra/private-vm.md#review-shared-graph-relationships) · [Operator rehearsal](rehearsal.md)

### Rehearse private Graph RAG with synthetic sources

Build the shared executable from the checkout. With Python 3.10+ and OpenSSL available, run the rehearsal against that binary. On Windows, use `python`, `rust-rag.exe`, and a new receipt path under your temporary directory. Pass `--openssl` with the path to OpenSSL if it is outside `PATH`.

~~~sh
cargo build --locked --features shared --bin rust-rag
receipt_dir="$(mktemp -d)"
python3 -B scripts/rehearse_shared.py --binary target/debug/rust-rag --receipt "$receipt_dir/team.json"
cat "$receipt_dir/team.json"
~~~

The rehearsal starts the actual shared service on loopback with short-lived synthetic identities and a controlled model endpoint. It tests both graph hops, exact citation spans, denied evidence, source changes, revocation, and recovery. The receipt records pass, fail, and unexercised checks. Follow the private VM runbook when you are ready to configure a trusted issuer and real storage. The synthetic rehearsal checks service behavior with deterministic inputs. Model quality and live deployment have their own [release checks](project/plan.md#execution-order-and-release-criteria).

For the current source's publication and review workflow, run the separate signed service test. It checks declared links, the review queue, access grants, stale requests, policy changes, and restore with synthetic sources.

~~~sh
cargo test --locked --features shared signed_http_company_graph_publication_review_and_restore
~~~

### Rehearse a cited company answer on Linux

Use the shared build, Python 3.10+, OpenSSL, and the pinned CPU model and server from the private VM guide. The Ubuntu package `libgomp1` supplies a library used by that server. Verify the archive and weights against the published hashes before unpacking or running them. Store both outside the checkout. [Pinned model and runtime](../infra/private-vm.md#verify-a-generated-graph-rag-answer)

Start the model in one terminal on the same Linux host as the source-built executable. Adjust the paths to your verified inputs.

```sh
llama_dir=/path/to/llama-b12207
LD_LIBRARY_PATH="$llama_dir" "$llama_dir/llama-server" \
  --model /path/to/Qwen3-4B-Q4_K_M.gguf --alias local-qwen \
  --host 127.0.0.1 --port 8081 --threads 4 --ctx-size 4096 \
  --parallel 1 --n-gpu-layers 0 --seed 42 --jinja --reasoning off
```

Once the model is ready, run the signed rehearsal from the checkout in a second terminal.

```sh
cargo build --locked --release --features shared
receipt_dir="$(mktemp -d)"
python3 -B scripts/rehearse_company.py --binary target/release/rust-rag \
  --receipt "$receipt_dir/company.json"
cat "$receipt_dir/company.json"
```

The rehearsal creates a fresh index, two invented source notes, a restricted control, and temporary signing keys. It asks for the catalog identifier alone, verifies `KS-5815` and both exact citations, and checks refusal of a reader's restricted model destination. It returns a nonzero exit code and a redacted receipt on an observed failure. Stop the model after the run. The receipt covers the shared process and its configured loopback endpoint. Record the verified archive and weight hashes separately when pinning a run. The private VM route covers container networking, TLS and operator-supplied identity. [Portable deployment and recovery](../infra/private-vm.md)

## Update, remove, export

- Re-run `ingest <folder>` while `deferred_sources` is positive to continue bounded work. Changed files receive priority and unchanged-looking files are periodically rehashed. Use `--sync` to remove missing sources after a fresh complete scan. An unreadable file keeps its previous revision. A successfully decoded blank file retires its previous passages and derived evidence.
- `rust-rag remove <source-id>` or `remove --all` removes derived data from the active index and leaves original files. Backups, exports, and physical media have separate retention.
- `rust-rag export --output /private/location/evidence.jsonl` writes active passages to a new private file. It does not print the corpus. Treat the export as sensitive.
- `init --upgrade` can save a pre-upgrade rollback copy in the state directory. `status` and `remove` report retained copies. These copies may contain passages you later remove from the live index. Use `discard-rollback --confirm` to remove them once you no longer need them.

### Upgrade, roll back, or uninstall

Before replacing a binary, record its version and features with `rust-rag capabilities --json`. Keep the old binary and a verified pre-upgrade backup. For personal use, close other readers and writers before copying the private state directory. For shared use, use the service backup and recovery procedure. Run the new binary against a separate copy or test deployment first and confirm a known citation and the current grants before switching traffic.

If you need to roll back a personal index after a schema upgrade, stop the new binary and run the previous binary against the verified pre-upgrade state. For shared rollback, keep client traffic closed while you start the previous image with its compatible profile and state, then restore, reconcile, and verify current sources and grants. An extractor change can alter the deployment fingerprint even when the endpoint URL and model name stay the same. Use a fresh index for the new image if `service-doctor` rejects the previous state. [Shared upgrade and recovery procedure](../infra/private-vm.md#upgrade-or-roll-back-the-service)

To remove a Cargo-installed executable, run `cargo uninstall rust-rag`. For a copied executable, remove the program from its install location and remove any PATH entry you added. For a shared image, stop the service and follow the host’s deployment teardown. Manage indexes, model caches, exports, and backups through their separate retention procedures.

## Privacy

A hosted agent may send paths, queries, and returned passages to its model. For strict local use, run the CLI and the consuming application locally. Any embedding, extraction, or answer endpoint is a separate processing choice. Keep credentials out of chat, logs, profiles, and Git. See [security](../SECURITY.md).

## Current limits

Query results are candidate context, including weak matches. Verify the cited text before drawing a conclusion. The default tokenizer can conflate identifiers that differ by punctuation, such as C++ and C#. It can match complete CJK terms, with no language-specific segmentation. Graph coverage reports whether a seed came from a ranked exact entity, an active catalog identifier, or lexical coverage. The native shared service traverses within each collection. See [status](../status.md) for portable service and cloud qualification.

## Development

`cargo test --locked` runs the default tests. Run the matching feature tests and the synthetic binary walkthrough before claiming support for a composition. See the [plan](project/plan.md) for the release and verification gates.

### Windows candidate qualification

Build the minimal x64 archive with [the release builder](../scripts/build_native_release.py) from a reviewed commit. Choose a short output path outside sync folders. The builder creates two MSVC build trees under the output parent, and deep paths can prevent the bundled SQLite object from being written. Keep the ZIP, SHA-256 manifest, SBOM, and provenance together. For the clean client check, copy the ZIP and [test script](../scripts/test_native_windows.ps1) to the guest. Use a standard account with a Unicode profile path containing spaces and a locally redirected Documents folder. Disconnect networking before running.

~~~powershell
powershell.exe -NoProfile -File .\test_native_windows.ps1 -Archive .\rust-rag-1.0.0-windows-x64.zip -ExpectedSha256 <recorded-ZIP-SHA256>
~~~

Record Defender and SmartScreen observations for the exact distribution bytes. The isolated Windows container test and host walkthrough are separate from this guest check.
