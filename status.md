# AnyRAG status

| Release | What it provides |
| --- | --- |
| [v1.3.1](https://github.com/skigsy/AnyRAG/releases/tag/v1.3.1) | Corrected packaged setup, browser graph decisions and exact citation reopening, and shared MCP collection discovery. |
| [v1.3.0](https://github.com/skigsy/AnyRAG/releases/tag/v1.3.0) | Signed source inventory, Obsidian links, native Word and Excel table citations, and a Git mirror source recipe. |
| [v1.2.0](https://github.com/skigsy/AnyRAG/releases/tag/v1.2.0) | Guided confirmation, cited name bindings, and read-only shared MCP in the Linux image. Three setup walkthroughs lead to a first openable citation. |
| [v1.1.3](https://github.com/skigsy/AnyRAG/releases/tag/v1.1.3) | Optional signed, scoped relationship lookups and explicit answer states. Shared search keeps committed citations available during folder sync. |
| [v1.1.2](https://github.com/skigsy/AnyRAG/tree/v1.1.2) | Source-only candidate tag. Use v1.1.3 for packages. |
| [v1.1.1](https://github.com/skigsy/AnyRAG/tree/v1.1.1) | Source-only candidate tag. Use v1.1.3 for packages. |
| [v1.1.0](https://github.com/skigsy/AnyRAG/tree/v1.1.0) | Source-only candidate tag. Use v1.1.3 for packages. |
| [v1.0.0](https://github.com/skigsy/AnyRAG/releases/tag/v1.0.0) | Windows x64 personal search and dashboard. Portable Linux amd64 shared search, Graph RAG, and PDF and Word readers. |

The Linux image includes Excel support. Build from source for in-process embeddings, AWS adapters, or the Git mirror recipe. The Linux folder-sync timer is a separate setup option. Run `rust-rag capabilities --json` to see the features in your executable.

- [Install and verify the packages](docs/setup.md)
- [Run the personal and shared operator rehearsals](docs/rehearsal.md)
- [Review the evidence and scope](docs/evidence.md)
- [Choose a setup](docs/field-guide.md#decision-tree)

The optional OCR worker is experimental for scanned tables. Reranking is opt-in. The AWS recipes have synthetic and offline checks. Live provider deployments require qualification in the target environment.
