# RAG architecture reference

Use this guide to choose a stack, explain the tradeoffs, and identify what to implement. The [field guide](field-guide.md) starts with the question and points to runnable Any RAG profiles. This reference covers the decisions behind those profiles and the extensions an adopter can build.

[Pipeline](#follow-the-evidence-through-the-pipeline) · [Sources and chunking](#sources-parsing-and-chunks) · [Embedding and retrieval](#embeddings-and-first-pass-retrieval) · [Models](#choose-models-by-job) · [Storage](#storage-and-indexing-options) · [Graph RAG](#graph-rag-patterns) · [Structured records](#structured-records-and-federated-retrieval) · [Reranking and answers](#reranking-context-and-answers) · [Evaluation](#evaluate-the-failing-stage) · [Operations](#operations-and-verification) · [Custom integrations](#when-a-workload-needs-an-adapter)

| Workload | Start with | Add when the workload calls for it |
| --- | --- | --- |
| Answer from a few short, permitted documents | Put the authorized material directly in context with source labels | Add an index when the corpus, cost, update rate, or permission model makes this impractical. |
| Find a policy, error code, or exact clause | Keyword search and source links | Semantic retrieval for paraphrases. Metadata filters for time, region, or version. |
| Find the same idea in differently worded passages | Dense embeddings and a lexical baseline | Hybrid fusion when both exact terms and meaning matter. Reranking when the right passages surface too low. |
| Trace a dependency or relationship across documents | Typed graph with evidence for every hop | Explicit link or ontology adapters where the source already carries relationships. |
| Summarize the themes in a whole collection | Partitioned or hierarchical summaries | Graph community summaries when relationships organize the corpus. |
| Answer an exact question about live records | Scoped SQL or an authoritative API | Federated retrieval when documents explain the result. |
| Answer across scans, tables, diagrams, or recordings | Layout-aware or multimodal extraction with original anchors | Separate retrieval routes by modality when one representation loses detail. |

Any RAG ships local text and Markdown retrieval and SQLite. Optional builds add text-layer PDF and Word readers, semantic and hybrid search, typed graph traversal, local reranking, cited answers, and read-only MCP. Its authenticated shared service scopes reads by collection. Scanned-table OCR is **experimental**. The AWS reference includes an optional Neptune topology projection and remains **unqualified** for live cloud use. PostgreSQL/pgvector, Qdrant, and global community search are **custom integrations**. Check the status of each profile and platform before relying on it. [Setup](setup.md) · [Current status](../status.md)

## Follow the evidence through the pipeline

Retrieval-augmented generation, or **RAG**, gives a model selected evidence when it answers. It can also stop at search and return passages for a person or an application to inspect. An embedding is a representation used to find candidates. A citation points to the source that supports the claim.

1. **Prepare.** Read and parse the source. Preserve its durable ID, revision, access mapping, and location. Split or represent the content at the granularity the question needs. Add embeddings, typed relationships, or summaries when they help retrieval.
2. **Retrieve.** Resolve the caller and eligible sources, then search or look up candidates. Combine lexical and semantic rankings if both are useful. Follow graph edges or query structured records when the question calls for those paths.
3. **Use and verify.** Optionally rerank the authorized candidates, assemble the smallest useful context, and generate an answer with citations. Resolve each citation against current evidence. If evidence is missing or contradictory, show what the sources do establish.
4. **Maintain.** Process edits, deletes, access changes, and restores. Retire stale chunks, vectors, relationships, and saved citations together. Track whether the index is current enough for the question.

**Classic RAG** uses a predictable retrieve-then-answer flow. **Agentic retrieval** can choose sources or run several searches based on intermediate results. Add it when a fixed route fails a representative question, and bound its tools, data destinations, and calls. **Multimodal retrieval** preserves evidence from images, layout, audio, or video. It starts with a representation that retains the original page region or timestamp. These choices can coexist. [Classic and agentic retrieval](https://learn.microsoft.com/en-us/azure/search/retrieval-augmented-generation-overview) · [Any RAG evidence lifecycle](architecture.md#source-and-evidence-lifecycle)

## Sources, parsing, and chunks

| Choice | Use it when | Tradeoff and integration boundary |
| --- | --- | --- |
| Snapshots or incremental connectors | A small collection can be scanned completely. A large or frequently updated collection needs checkpoints or change events. | Map durable source IDs, versions, tombstones, and trusted ACLs. Handle replay and out-of-order events. Reconcile a complete inventory before inferring deletion from absence. |
| Text layer, OCR, or layout-aware parsing | The meaning is in prose, scans, or page layout respectively. | Keep page and region coordinates, reading order, confidence, and links to the original. Extract tables with headers and units. Keep spreadsheet sheet/row, image region, or audio timestamp anchors when relevant. |
| Fixed windows, headings, or parent-child chunks | Fixed windows give a predictable baseline. Headings preserve structure. Small child chunks can retrieve precisely and expand to a larger parent for context. | Keep chunks within the embedding model's token limit to prevent truncation. Add a title or section path to embedding input when useful and preserve the literal text for citations. Overlap increases index size. Version the parser and chunking rules. |
| Metadata and enrichment | Questions depend on dates, jurisdictions, product versions, named entities, or document type. | Keep trusted ownership and access fields separate from labels inferred from text. Preserve both source and ingestion times. Record unknown values instead of silently inventing them. |

## Embeddings and first-pass retrieval

An embedding model converts content or a query into vectors. A retrieval engine compares the vectors. Record the model revision, dimensions, language and modality fit, token limits, and approved processing destination. Use compatible query and document encoders with the prefixes expected by each. Match normalization, similarity metric, and index operator class. Re-embed affected content when the model or representation changes and keep the new vectors in a separate index until cutover. The default keyword starter needs no downloaded model. Any RAG's Porter stemmer and optional `BAAI/bge-small-en-v1.5` model are English-oriented. For other languages, choose a suitable analyzer and model, and test cross-language queries if users need them. The custom embedder seam accepts other models. [Local semantic setup](setup.md#local-semantic-search) · [Custom adapter](architecture.md#using-the-rust-sdk-as-an-application-foundation) · [BGE model card](https://huggingface.co/BAAI/bge-small-en-v1.5)

A local embedding model uses host memory and compute and needs explicit model acquisition. A hosted API handles serving and scaling but receives input text and adds network latency, credentials, and per-call cost. Pin the model and endpoint behavior in either case. Benchmark the languages and document types users actually search.

| Retrieval | Strength | Cost or failure mode |
| --- | --- | --- |
| Lexical with FTS or BM25 | Exact words, rare identifiers, quoted terms, fast first pass | Tokenization and language affect matching. Paraphrases may have no terms in common. Shared scoring statistics can couple otherwise separate scopes. |
| Dense embeddings with a bi-encoder | Paraphrases and meaning-based matching. Document vectors are reusable. | Domain and language mismatch reduce recall. Indexing costs scale with chunks. Dense similarity can miss exact IDs and numbers. |
| Learned sparse retrieval | Expands the vocabulary while retaining a term-like representation | Needs a compatible sparse index and model. Measure language coverage and expansion noise. |
| Late interaction, such as ColBERT | Token-level matching retains more fine-grained signals | Multiple vectors per chunk raise storage and search cost. Select an index and scorer that support the representation. |
| Hybrid fusion, such as reciprocal rank fusion (RRF) | Merges keyword and vector rank lists without comparing their raw scores | Each branch needs a candidate budget. RRF combines rankings and cannot recover evidence missed by both branches. |
| Query routing, rewriting, or decomposition | Routes exact facts to structured sources and splits compound questions into bounded subqueries | Preserve the original question. Bound calls, carry permissions to every branch, deduplicate by evidence identity, and report unavailable sources. Rewrites, multi-query and HyDE can add model latency and egress. HyDE embeds a generated hypothetical passage as a search probe. The probe supplies no evidence for the answer. |

[SQLite FTS5](https://www.sqlite.org/fts5.html) · [ColBERTv2 paper](https://aclanthology.org/2022.naacl-main.272/) · [Elastic hybrid search](https://www.elastic.co/docs/solutions/search/hybrid-search) · [HyDE paper](https://arxiv.org/abs/2212.10496)

## Choose models by job

| Model job | Add it when | Check before committing |
| --- | --- | --- |
| Embedding | Important questions use different wording from their sources. | Test domain and language fit, recall, dimensions, and re-embedding cost. Start with the [documented local model](setup.md#local-semantic-search) if it fits. |
| Graph extraction | Useful relationships are written in prose and the source lacks reliable links. | Require a typed relation and an exact supporting quote. Prefer source-provided IDs and links when they exist. |
| Reranking | The right evidence is often among candidates but a plausible near match outranks it. | Check ranking lift, exact identifiers, end-to-end latency, and what happens on timeout. |
| Answer generation | Users need a synthesis, explanation, or comparison they can verify against sources. | Test the answer separately from retrieval. Require supporting citations and check how the model handles missing evidence. |

The jobs can run on different hosts, use different models, or be omitted. For each, record what it receives and whether it runs locally or at an approved endpoint. Compare memory, indexing throughput, and p95 latency on your hardware. Estimate hosted cost across ingestion, refresh, and query traffic. Fine-tuning can change model behavior or output format. Keep changing facts in a source you can update, authorize, and cite.

## Storage and indexing options

Choose the store based on data ownership, permission filters, index lifecycle, and measured query cost. Exact nearest-neighbor search scores every eligible vector and provides a recall baseline. Approximate nearest-neighbor search (ANN) reduces work through an index and can miss true neighbors. Test the same filter, metric, and data when comparing the two. A graph database serves relationship traversal and a vector index serves similarity search. A deployment can use both, or keep both in one store.

| Store or pattern | Good fit | Tradeoff | Any RAG |
| --- | --- | --- | --- |
| SQLite with FTS5 and exact vector scoring in the application | A personal or bounded single-host deployment with a simple backup and restore path | Embeddings live in SQLite blobs. Exact scoring grows with the number of eligible chunks. One host owns the write lifecycle. | Shipped. |
| PostgreSQL and pgvector | An organization already runs PostgreSQL, or vector retrieval needs SQL joins, transactions, and metadata or access filters alongside relational data | Adds network calls, connection pooling, database operations, and ANN tuning. Shared indexes can couple scopes. | Custom. See below. |
| In-process vector library | The service needs a local ANN index and already owns source records, filtering, and publication | The library supplies similarity search. The application supplies durable IDs, metadata, permission isolation, and rebuild or persistence semantics. | Custom. |
| Search engine, such as Elasticsearch, OpenSearch, or Azure AI Search | Lexical relevance, fielded search, facets, and hybrid ranking are core requirements | Adds mappings, analyzers, index refresh and operational or service cost. Validate permission filters and deletion in every search branch. | Custom. |
| Vector service, such as Qdrant, or managed vector search | Vector query scale or filtering needs its own lifecycle | Keep document authority and evidence pointers consistent with the derived index. Review tenant isolation, reindexing, egress, and recovery. | Custom. |
| Private graph store, such as Neptune or a property-graph service | Relationships need their own query or projection lifecycle | Adds a separate store, synchronization, and scope checks on every hop. A graph engine does not establish evidence or permissions by itself. | Neptune projection in the AWS reference. Other graph stores are custom. |

[Qdrant indexing and filters](https://qdrant.tech/documentation/manage-data/indexing/) · [Azure AI Search vector filters](https://learn.microsoft.com/en-us/azure/search/vector-search-filters) · [Any RAG AWS reference](../infra/aws-deployment.md#opt-in-neptune)

### PostgreSQL with pgvector

Use **pgvector** when keeping vectors beside transactional data, metadata joins, or row policies simplifies the system. For example, a procedures service can retrieve similar passages within a user's permitted collections and return an immutable revision and citation ID from the same query. The extension provides exact and approximate search, including cosine distance. You still supply the embedding model, stable source identities, and authorization policy.

| Index choice | Advantage | Cost or limitation |
| --- | --- | --- |
| Exact search with a normal index on scope or other selective metadata | Measures true top-k within the permitted set. Good baseline when filters narrow the search. | Ranking cost grows with the eligible set. |
| HNSW | Navigates a proximity graph. Often offers a strong speed and recall tradeoff without a training step. | More memory, slower builds, and tuning for search breadth and updates. The proximity graph represents vector similarity. |
| IVFFlat | Clusters vectors into lists. Typically builds faster and uses less memory than HNSW. | Needs representative data before building the index. List and probe settings trade latency against recall. Recheck as the corpus changes. |

**Filtered ANN and permissions.** With pgvector's approximate indexes, the index can generate candidates before applying SQL filters. A highly selective grant can therefore yield fewer than k matches even when enough permitted rows exist. Begin with exact ranking over the permitted set and compare ANN recall and latency per grant shape. pgvector 0.8.0 and later support iterative scans. Within a transaction, `SET LOCAL hnsw.iterative_scan = strict_order` or `SET LOCAL ivfflat.iterative_scan = relaxed_order` can widen a scan within its limits. Relaxed order can require an explicit final sort. For boundaries where denied rows must have no effect on recall or budgets, use a permission-aligned partition or separate index, or use exact scoped ranking. Stronger timing isolation may require separate deployments. Filter and authorize inside the backend query. Send only authorized results to downstream rerankers and models.

Match the query metric to the index operator class, such as `vector_cosine_ops` for cosine distance. For pgvector 0.8.7, the documented HNSW and IVFFlat limits are 2,000 dimensions for `vector` and 4,000 for `halfvec`. These are index limits. Quantization, half precision, or a model-supported shorter representation can reduce storage and memory. Compare recall and consider exact rescoring over the candidates when using compressed vectors. Token-level late interaction needs its own scorer and index design.

**Integration path.** Store chunk ID, source ID, active revision, collection, model fingerprint, and a fixed-dimension vector. Bind request-derived scope from verified policy and use parameterized queries. If using row-level security, run as a least-privileged role that cannot bypass it, and test with the actual application role and connection pool. PostgreSQL owners normally bypass row security unless it is forced. Superusers and roles with `BYPASSRLS` bypass it. Keep scope context transaction-bound so a reused connection cannot inherit it. [PostgreSQL row security](https://www.postgresql.org/docs/current/ddl-rowsecurity.html)

For a first Any RAG integration, keep SQLite authoritative and make the pgvector index rebuildable. Synchronize active revisions and deletes, retrieve IDs and scores, then resolve them against current authorized SQLite evidence. Discard stale candidates and report an index lag or unavailable state when needed. A full PostgreSQL store replacement also needs atomic revision publication, backups, restore, and scope parity across lexical, vector, graph, and citations. These are implementation options beyond the shipped profiles.

An exact baseline for a custom `chunk_vectors` projection can materialize the eligible rows before sorting. Bind `$1` to permitted collections from trusted policy, `$2` to the embedding fingerprint, `$3` to a same-model query vector, and `$4` to a bounded result count. The `is_current` marker represents the projection's last synced state. Recheck every returned revision against the authoritative store.

```sql
WITH eligible AS MATERIALIZED (
    SELECT chunk_id, source_revision, embedding
    FROM chunk_vectors
    WHERE collection = ANY($1::text[])
      AND model_fingerprint = $2
      AND is_current
)
SELECT chunk_id, source_revision, embedding <=> $3::vector(384) AS distance
FROM eligible
ORDER BY distance, chunk_id
LIMIT $4;
```

### SQLite, pgvector, or Qdrant

| Start with | Why | What would change the choice |
| --- | --- | --- |
| **SQLite** | One local or bounded shared runtime can own the evidence and its updates. It gives you an exact scoped baseline and fewer services to run. | Measure how eligible chunk count, concurrent queries, and write load affect the target latency and recovery time. |
| **PostgreSQL with pgvector** | Your app already keeps source metadata or access policies in PostgreSQL, and transactions or relational joins simplify the design. | Benchmark filtered recall and latency against the exact authorized set. Include connection pooling, grants, and rollback. |
| **Qdrant** | Vector retrieval needs independent capacity, indexed payload filters, or a tenant-aware storage layout. | Measure the whole stack, including keeping revisions and grants in sync, citation lookup after revocation, and restoring the service and authoritative store together. |

Use the same corpus, embeddings, distance metric, scope selectivity, update mix, and concurrency for the comparison. Measure recall at k against exact scoped neighbors, p95 latency, update-to-visible time, and restore-to-ready time. Include a revoked caller and a deleted source. A tenant filter selects candidates. The application must still enforce the caller's current policy on every result and saved citation. [Qdrant payload indexes](https://qdrant.tech/documentation/manage-data/indexing/) · [Qdrant multitenancy](https://qdrant.tech/documentation/manage-data/multitenancy/)

## Graph RAG patterns

Graph RAG represents relationships and uses them during retrieval. For a dependency question, seed the graph from authorized lexical or vector hits, expand a bounded path, and resolve each edge to an allowed passage. Record the node identity, predicate, direction, source revision, anchor, and when a relationship is valid. Distinguish literal source support from an inference across several edges. Cap hops and fan-out, expose incomplete coverage, and resolve aliases and contradictory sources explicitly.

| Pattern | Best question | Design tradeoff |
| --- | --- | --- |
| Explicit document links or curated knowledge graph | “What does this approved dependency record link to?” | High-quality relationships if the source maintains them. Resolve links and grants against current source versions. |
| Entity co-occurrence | “Where do these names appear together?” | Cheap candidate expansion. Shared mention alone supplies no predicate, direction, or causal claim. |
| Extracted typed edges and local traversal | “Which components does this service depend on, and where is that documented?” | Extraction can miss or mislabel edges. Use a constrained predicate set, literal anchors, versioned extraction, and abstention when a link lacks support. |
| Community summaries and global or hierarchical search | “What risks recur across the entire portfolio?” | Generates broader coverage at indexing and query cost. Summaries can mix scopes and go stale. Build them within a permission boundary and retain supporting sources and rebuild rules. |
| Iterative graph retrieval or DRIFT-style search | “Which related projects might explain this recurring problem?” | Can widen coverage through follow-up questions. Bound traversal and model calls, carry permissions, and preserve traceable evidence for each branch. |

Local graph retrieval, collection-wide summaries, and iterative retrieval are separate integration choices. Any RAG implements scoped, bounded evidence traversal. Its optional Neptune Database adapter projects opaque topology, validates active generations, and resolves returned edges against authoritative SQLite evidence. Neptune Analytics is a separate AWS service for analytical workloads. [Full Any RAG pipeline](architecture.md#full-graph-rag-pipeline) · [Neptune Analytics](https://docs.aws.amazon.com/neptune-analytics/latest/userguide/what-is-neptune-analytics.html)

## Structured records and federated retrieval

Use a scoped query or source API for exact values, dates, counts, and current status. For example, select a patient's lab history by the source's patient and test identifiers. Keep the original value, unit, collection time, and result revision together. Follow the source's order or encounter references where they exist. Record missing links and distinguish amended results from current ones. The clinical source and its access policy own those facts. An EHR adapter and patient-level access checks are custom work. [FHIR Observation](https://hl7.org/fhir/R4/observation.html) · [FHIR DiagnosticReport](https://hl7.org/fhir/R4/diagnosticreport.html)

Use federated retrieval when no single source can answer the question. A scoped record query can supply the current value while text search retrieves the policy that explains a process. Give each branch its own access and freshness checks. Return source versions, timestamps, and unavailable branches with the evidence. Keep an audit trail of which source and caller each branch used. Start with a fixed route for known question types and add query planning when a representative question needs it.

## Reranking, context, and answers

Reranking takes a small candidate set and scores each candidate against the original question. Use it when retrieval finds the right evidence but puts it below weaker matches. A **cross-encoder** sees the query and passage together, which can improve relevance at the cost of inference for each pair. A late-interaction scorer can reuse passage token vectors. An LLM can compare several passages at once, with more token cost and a need to validate returned IDs. A local reranker keeps model traffic on the chosen host and uses CPU or GPU time. A hosted reranker adds a network call and sees the supplied query and passages. Heuristic boosts are cheaper and interpretable but need rules suited to the corpus. Bound candidate count, length, and timeout. Test a defined fallback such as retaining the initial authorized ordering. Any RAG offers opt-in local learned reranking alongside first-pass ranking and RRF. The verified recipe and its timeout behavior are documented separately. [Local shared reranking](setup.md#local-shared-reranking) · [Local personal reranking](setup.md#local-personal-reranking) · [Sentence Transformers retrieve and rerank](https://sbert.net/examples/sentence_transformer/applications/retrieve_rerank/README.html) · [ColBERTv2 paper](https://aclanthology.org/2022.naacl-main.272/)

Build context from authorized, deduplicated passages within a token budget. Preserve neighboring passages or required hops when a citation needs them. Track conflicts, dates, and provenance. Treat retrieved text and model output as untrusted data in query transformation, reranking, and answer generation. Separate tool privileges from model prompts. Enforce explicit read scopes in the application. Use structured answer output and validate citations against evidence actually supplied. Citation membership alone cannot establish that a passage supports a claim. For missing evidence, return the available passages or an explicit insufficient-evidence result. [Any RAG security boundaries](../SECURITY.md) · [OWASP guidance for LLM applications](https://owasp.org/projects/top-10-for-large-language-model-applications)

## Evaluate the failing stage

| What you observe | What to measure | First change to try |
| --- | --- | --- |
| The source fact is absent or misread | Exact anchors, table associations, extraction coverage | Fix parsing or use structured source fields. |
| The right passage never reaches the candidate list | Recall at k, or the fraction of questions with every required hop among the candidates | Change the source representation, first-pass retrieval, or graph seeds. |
| The right passage is present but ranks too low | Top result accuracy or nDCG, which rewards placing more relevant results higher | Compare fusion with the same retrieval inputs, or rerank the same candidate list. |
| The evidence is right but the answer is wrong | Answer accuracy, citation support, abstention on unanswerable questions | Inspect context assembly, generation, and citation resolution separately. |
| Denied or outdated evidence surfaces | Unauthorized and stale results in search, citation reads, graph paths, and model requests | Correct scope and revision enforcement before comparing ranking quality. |
| Useful answers take too long | End-to-end p95, per-stage latency, fallback rate, and update lag | Reduce the work in the measured bottleneck and report what fallback returns. |

Keep the same questions, expected evidence, and access policy for before-and-after runs. Include exact identifiers, paraphrases, genuinely multi-hop questions, and questions with no supported answer. Record the corpus, settings, model revisions, and limits so another operator can reproduce the comparison. [Any RAG benchmark examples](benchmarks.md)

| Try it | What to inspect |
| --- | --- |
| [Keyword search and source review](setup.md#review-sources-and-explain-results) | Open the exact indexed quote, read the retrieval explanation, then replace or remove the source and try the saved citation again. |
| [Local hybrid search](setup.md#local-semantic-search) and [local reranking](setup.md#local-personal-reranking) | Ask an exact identifier question and a paraphrase. Check that each change addresses a miss or a rank problem without displacing the exact match. |
| [Typed graph demo](architecture.md#run-the-graph-demo) and [mixed graph quality replay](evidence.md#reproduce-the-mixed-graph-quality-run) | Inspect the evidence for each hop. Compare graph off and on at the same result limit. |

**Test prompts with the evidence held fixed.** Save a few synthetic question and passage pairs with stable evidence IDs. Change one prompt or model setting at a time. Check claim support, citation IDs, abstention when a key passage is removed, and behavior when a passage contains instructions. Fix retrieval first if the necessary evidence never reached the model.

**Fine-tune only for a recurring model behavior.** Consider it when a stable task such as classifying approved relation labels or emitting a domain-specific schema still fails after prompt and schema tests. Use authorized training material and a held-out set. Compare quality and operating cost against the same prompt baseline. Continue retrieving changing facts from their authoritative sources.

## Operations and verification

| Decision | What to record or check |
| --- | --- |
| Access and egress | Identify the caller and compute current grants before retrieval. Recheck before external processing and before response. Track all destinations, including the coding or answer harness, embedding APIs, caches, logs, and traces. Key any semantic, reranker, and answer caches to the caller's current permission scope, policy version, source revisions, and model configuration. Test allow, deny, revoke, stale cache, and in-flight model calls. |
| Ingest and freshness | Track source, parser, embedding, extraction, and policy versions. Make retries idempotent. Publish complete revisions and retire old rows and edges. Cover deletions in summaries, search indexes, vectors, caches, and backups under their own retention rules. Use a reconciled checkpoint for cutover and preserve a rollback path. |
| Capacity and failure | Measure p95 query latency, freshness lag, cost per indexed document and query, index footprint, and candidate recall under realistic permissions and concurrency. Bound queues, provider retries and timeouts, budget expensive graph hops, and state fallback behavior for stale indexes or unavailable models. |
| Retrieval and answer quality | Freeze development and held-out questions with expected source passages and unanswerable cases. Compare Recall@k for evidence coverage, MRR or nDCG for ordering, and path or edge support for graph. Check citation correctness and claim support independently, plus abstention and false positives. Compare a no-graph baseline before adding expensive stages. |
| Recovery and monitoring | Back up authoritative evidence and keep policy independently recoverable. Restore in maintenance, reconcile current versions and grants, then reopen explicitly. Track stage timings and counts without retaining source text by default. Check health separately from readiness to answer. |

Distinguish logical removal from physical retention. A superseded SQLite revision becomes ineligible for retrieval, and WAL and backup retention need their own controls. PostgreSQL's MVCC can retain dead row versions until maintenance, and vacuum is no guarantee of erasure from disk or backups. Search and vector stores can retain old segments, index entries, or snapshots after an item stops appearing in search. Graph projections need cleanup as well as invalidation. Verify when query visibility ends and record the purge or expiry policy for each copy, including caches and provider logs. [Any RAG deletion and retention](../SECURITY.md) · [PostgreSQL vacuuming](https://www.postgresql.org/docs/current/routine-vacuuming.html)

**Trace the work behind one request.** Start with the [local retrieval explanation](setup.md#review-sources-and-explain-results). In a multi-service app, propagate a request ID and record stage, elapsed time, candidate count, model fingerprint, and fallback reason where each component can report them. Record whether a stage was disabled, incomplete, or attempted and fell back. Keep raw questions, passages, and credentials out of default trace exports. Use synthetic requests to verify the trace and check its retention and destination before enabling it on a shared service.

### Record a build decision

1. Write one representative question, an expected source location, and who can see it.
2. Choose the source path, representation, retrieval method, storage, and optional graph or reranking path. Identify which parts use an existing Any RAG profile and which require adapters.
3. Record which processes receive text, vectors, metadata, or credentials. Define how updates, revocations, failures, and deletes propagate.
4. Run a first query over synthetic or approved data. Follow one result back to its source. Use the operations checks to qualify claims about scale, quality, and isolation. Installation and custom configuration remain available without evaluation.

[Profile recipes](field-guide.md#example-compositions) · [Infrastructure](../infra/README.md) · [Verification evidence](../status.md)

## When a workload needs an adapter

Use the supported profile for ingestion, retrieval, or evidence where it fits. The routes below are custom integrations. The first check tells you whether the added component solves the named problem.

| Workload trigger | Integration route | First check |
| --- | --- | --- |
| A manual's table, a spreadsheet row, or a slide region holds a fact the text parser loses | Compare a layout-aware parser with the native reader. Keep the original file, revision, page and region or sheet and cell, plus the row and header that identify the fact. Use normalized records for search and resolve the result back to the original anchor through your adapter. | Retrieve a cell with repeated values in adjacent rows. Open the exact original cell and its header. Measure wrong or missing links and parsing time. |
| A wiki or drive changes too often for full folder snapshots | Start with the source API or an existing loader. Map each item to a durable source ID and revision, map native grants separately, and checkpoint upserts and deletions. Use the [existing deployment connectors](../infra/README.md) where they fit. | Replay a duplicate event, an edit, a deletion and a permission change. Verify that every search and saved citation uses the latest authorized revision. |
| A team needs to edit and operate a branching workflow visually | Use a visual workflow tool as a caller of the shared search API. A trusted gateway can propagate the verified caller and sign requests where the deployment requires signatures. Keep the application's current grant checks on both search and citation lookup. [Shared service runbook](../infra/private-vm.md) | Compare results with direct signed requests. Revoke a caller and verify the workflow stops returning both search hits and saved citations. |
| A portfolio question needs evidence from the whole permitted collection | Compare summaries by project or time period with graph community summaries. Partition by access boundary. Attach each theme to supporting revisions and rebuild affected summaries when those revisions or grants change. | Plant a supported theme across several low-ranked documents. Verify coverage and exact support, then remove a document and test the rebuilt summary. |
| An existing PostgreSQL app needs joins, or a vector workload needs independently scaled search | Benchmark a rebuildable pgvector or Qdrant projection against exact scoped retrieval. Send back candidate IDs and resolve their current evidence and policy in the authoritative store. [Store chooser and comparison protocol](#sqlite-pgvector-or-qdrant) | Measure scoped recall, p95, update lag and recovery. Revoke a caller and retire a revision during the trial. |
