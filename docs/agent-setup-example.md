# Agent setup example

This example uses invented documents and a fictional team to show how a coding agent recommends a setup.

## Request

> I want to use https://github.com/skigsy/AnyRAG for our maintenance team. We have Markdown and PDFs. I want people to ask what equipment depends on a part and see the source behind the answer. Some of the documents are restricted.

## What the agent asked

- “Who will search, and how are the restricted documents restricted?”
- “Can you paste one real question and say where its answer lives?”
- “Where is the document text allowed to go?”

We answered that 15 staff use two collections with separate grants and an existing sign-in service on the LAN. The PDFs have selectable text. One bulletin links relay R17 to a controller, and a separate service note links that controller to a pump. Both hops are in the same authorized collection. The available host is one Linux amd64 workstation, and document text and model calls must stay on the LAN.

## What it recommended

> **Smallest build:** one source-built binary with the `shared` and `pdf` features (`shared` already includes `model`), plus a local Qwen3-4B on llama-server to extract the graph. Use graph retrieval only, with no embeddings, reranker or answer model.

It proposed two collections, with the restricted one open only to the reliability group. The operator supplies the OIDC configuration, private HTTPS, local model and relation vocabulary, separate source folders, state and policy paths, and a client that keeps passages on the LAN.

The published Linux image already includes `shared`, `pdf`, and `docx`. Enable PDF ingestion with `"pdf": true` in the service profile. [Image build](../infra/private-vm.md#build-an-image) · [Shared profile example](../infra/shared.profile.example.json)

## First steps

Run the planner, then use the [private Linux runbook](../infra/private-vm.md) to prepare the service profile and the [company Graph RAG settings](../infra/private-vm.md#enable-company-graph-rag). The runbook covers the image, trusted inputs, HTTPS, and the local model.

~~~sh
rust-rag plan company --json
~~~

The planner returns `"profile": "company"` and `"required_features": ["shared"]`. For this text-layer PDF workload, the agent additionally selected `pdf`. Once the operator configures the profile, run the documented preflight before starting the service.

~~~sh
rust-rag service-doctor --profile /etc/rust-rag/profile.json
~~~

Use synthetic equivalents of the bulletin and service note. Compare graph off and on, verify one exact citation per hop, then check that a user whose grant was revoked cannot reopen a saved citation. If the workflow later needs generated answers, add and verify that endpoint separately. [Signed fixture](setup.md#rehearse-private-graph-rag-with-synthetic-sources) · [Private Linux test results](evidence.md#fresh-linux-vm-replay)
