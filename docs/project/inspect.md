# AnyRAG inspection

## Supported paths

The standalone executable indexes Markdown and text locally. Optional readers add PDF, Word, or Excel. Hybrid search, graph traversal, model answers, MCP and the shared service can be selected independently. [Architecture](../architecture.md) · [Setup](../setup.md)

The synthetic [operator rehearsal](../rehearsal.md) checks ingestion, exact citations, edits, deletions, revocation, and recovery. The [evidence guide](../evidence.md) records each measured scope and the commands to reproduce it.

## Reranker qualification

The local reranking profile uses a pinned cross-encoder, an authorized candidate pool, and bounded inference. Keep it opt-in and compare fully cited answers, evidence coverage and latency on representative queries before changing deployment defaults. [Setup](../setup.md#local-shared-reranking)

## Limits

Scanned tables need a separate OCR worker. The text layer, table association and citation behavior depend on the source and extraction profile. Shared Graph RAG supports bounded paths and requires explicit source and policy configuration. Live deployment behavior depends on the target identity, network, storage and model services.
