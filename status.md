# AnyRAG status

| Release | What it provides |
| --- | --- |
| [v1.1.2](https://github.com/skigsy/AnyRAG/releases/tag/v1.1.2) | Optional signed, scoped relationship lookups and explicit answer states. Shared search keeps committed citations available during folder sync. |
| [v1.1.1](https://github.com/skigsy/AnyRAG/tree/v1.1.1) | Source-only candidate tag. Use v1.1.2 for packages. |
| [v1.1.0](https://github.com/skigsy/AnyRAG/tree/v1.1.0) | Source-only candidate tag. Use v1.1.2 for packages. |
| [v1.0.0](https://github.com/skigsy/AnyRAG/releases/tag/v1.0.0) | Windows x64 personal search and dashboard. Portable Linux amd64 shared search, Graph RAG, and PDF and Word readers. |

Build from source for Excel, read-only MCP, in-process embeddings, and AWS adapters. The Linux folder-sync timer is a separate setup option. Run `rust-rag capabilities --json` to see the features in your executable.

- [Install and verify the packages](docs/setup.md)
- [Run the personal and shared operator rehearsals](docs/rehearsal.md)
- [Review the evidence and scope](docs/evidence.md)
- [Choose a setup](docs/field-guide.md#decision-tree)

The optional OCR worker is experimental for scanned tables. Reranking is opt-in. The AWS recipes have synthetic and offline checks. Live provider deployments require qualification in the target environment.
