# AnyRAG delivery plan

## Execution order and release criteria

Start from the requested workload and pick the smallest working configuration. Verify cited retrieval with synthetic inputs before adding model endpoints or shared access. For shared changes, verify that edits, deletion, revocation and restore preserve the documented citation and access rules.

Before publishing a release, verify the public file allowlist, full Git history, hosted checks, exact package digests, and build attestations. Confirm the release assets from their public download URLs.

## Company Graph RAG reference build

Use the [private Linux runbook](../../infra/private-vm.md) and [operator rehearsal](../rehearsal.md). Start with a folder-backed collection, use source evidence to check each hop, and set a publication policy based on the collection's risk.

## Post-build hardening

Run the documented smoke tests for the changed workload. Measure quality and latency before expanding defaults or making provider-specific claims. Keep the deployment guide and limitations aligned with the release evidence.
