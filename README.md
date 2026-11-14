# AnyRAG

![AnyRAG — open-source Rust foundation for private document search and Graph RAG](docs/assets/any-rag-banner.svg)

**Start with working RAG. Build the version you need.**

Start with a working pipeline that indexes your files, finds relevant passages, and links back to the source. Use it for a folder of notes, then add private team access or Graph RAG when your project calls for it.

**[Start with your coding agent ↓](#set-it-up-with-an-agent)** · [Manual quickstart](#quickstart-rust-personal-search)

## Choose a setup

[![AnyRAG building blocks: keyword search, PDF and Word, embeddings and reranking, cited answers, typed Graph RAG, shared access, and recovery. Solo, team, and company setups can combine the same blocks.](docs/assets/setup-progression.svg)](docs/assets/setup-progression.svg)

| Example build | What you get |
| --- | --- |
| [Search my notes](docs/field-guide.md#personal-obsidian-recipe) | Find an exact passage in a folder or Obsidian vault on your laptop. No model required. |
| [Give my team a private handbook search](infra/private-vm.md) | Share one search service and control which collections each reader can see. |
| [Trace dependencies across engineering docs](infra/private-vm.md#enable-company-graph-rag) | Add Graph RAG to private team search. Follow a chain across files and open the source for each step. |

[Choose a different combination](docs/field-guide.md#decision-tree)

### Set it up with an agent

Give your coding agent this repo and describe what you need. For example:

```text
Use AnyRAG to search my Obsidian vault. Keep my notes on this laptop
and show me how to reopen a citation.
```

```text
Use AnyRAG to build private Graph RAG for our engineering documents.
We need to trace dependencies across files and restrict who can see them.
```

The agent recommends a build, explains the choices, and walks you to a first cited result. Add or swap pieces as you go. [Agent instructions](AGENTS.md) · [Example setup conversation](docs/agent-setup-example.md)

## See what comes back

[![Query: Where is the workshop spare key? Answer: Locker 14. The exact quote reopens from workshop.md at its saved revision.](docs/assets/citation-proof.svg)](docs/setup.md#review-sources-and-explain-results)

That citation points to a specific passage and source revision. Sync an edit or deletion and the old citation stops resolving. Shared search checks current access when someone opens a citation. [Try source review](docs/setup.md#review-sources-and-explain-results)

**What if the answer is split across files?** The [Graph RAG demo](docs/architecture.md#run-the-graph-demo) follows `CedarPump → RelayKit → SpareBracket` and shows the quote behind each link.

## What AnyRAG builds for you

- **A searchable document index.** Index a folder of notes and sync it as files change. Enable PDF and Word readers when you need them.
- **Search with citations you can open.** Find exact terms or add search by meaning. Results point to the passage and the source version it came from.
- **Team access, Graph RAG, and recovery.** Add permissions, follow relationships across files, and back up or restore a shared index.

AnyRAG includes the code, deployment recipes, and tests for the work that starts once people rely on search. The tests check that deleted passages disappear, revoked readers lose access even to saved links, and a restored service checks current files and permissions before it serves results again. [Run the same checks](docs/rehearsal.md)

You choose the files, the model if you need one, who gets access, and where it runs. [Explore the architecture](docs/architecture.md)

## What's new

| Feature | What you can do |
| --- | --- |
| [Resolve a scoped relationship](infra/private-vm.md#look-up-a-reviewed-relationship) | Follow a directed chain through current, reviewed sources and cite the setting where it applies. |
| [Reopen citations in MCP](docs/setup.md#search-a-personal-index-from-an-mcp-client) | Ask your assistant to reopen a saved citation and verify the exact passage. |
| [Preview indexed sources](docs/setup.md#inspect-what-the-dashboard-indexed) | Open a document's indexed passages in the dashboard and review files that were skipped or failed. |
| [Schedule folder sync](infra/README.md#keep-a-personal-folder-current-on-linux) | Keep a personal index current with a Linux timer. |
| [Search Excel by cell](docs/setup.md#search-an-excel-workbook) | Index visible worksheets and cite the exact cell behind a result. |

Use the linked source-build instructions for these features.

## Live operator dashboard

[![The live AnyRAG dashboard showing indexed sources, graph coverage, and recent activity.](docs/assets/dashboard/overview.png)](crates/rust-rag/README.md#local-index-dashboard)

See what’s indexed, what still needs enrichment, and what changed on the last sync. Activity history is opt-in. [Open the dashboard](crates/rust-rag/README.md#local-index-dashboard)

## Quickstart: Rust personal search

[Windows ZIP](docs/setup.md#install-the-windows-zip) · [Linux image](docs/setup.md#try-personal-search-in-the-linux-image) · [Build from source](docs/setup.md#install-from-this-repository) · [v1.1.0 release](https://github.com/skigsy/AnyRAG/releases/tag/v1.1.0)

Start with local Markdown and text search. The default build needs no model or database service.

## Build and deploy

For a team, use the [single-host Linux runbook](infra/private-vm.md). For cloud, start from the [AWS infrastructure reference](infra/aws-deployment.md#deployment-architecture) or adapt the [deployment recipes](infra/README.md) to your provider.

[Architecture and SDK](docs/architecture.md) · [Graph RAG pipeline](docs/architecture.md#full-graph-rag-pipeline) · [Verification](docs/evidence.md) · [Security](SECURITY.md) · [MIT license](LICENSE)
