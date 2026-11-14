# What we verified

Use this page to match a claim to a build, a workload, and a check you can run. See the [benchmark](benchmarks.md) for the paired compact-evidence comparison. The [aggregate receipt](evidence-results.json) records the historical results and the hashes of the reports that produced them. The blinded fixture, raw model requests, and individual decisions remain private.

## Builds and packages

| Build | What you can verify |
| --- | --- |
| [v1.1.0 packages](https://github.com/skigsy/AnyRAG/releases/tag/v1.1.0) | Check the archive digests and signed build provenance for the Windows x64 ZIP and portable Linux amd64 image. |
| [v1.0.0 packages](https://github.com/skigsy/AnyRAG/releases/tag/v1.0.0) | Use the checksums and signed build provenance to identify the exact Windows ZIP and Linux image. The Windows receipt covers the standalone CLI. The Linux receipt covers the full operator workflow using the executable from the archived image. |

[Verify and install the Windows ZIP](setup.md#install-the-windows-zip) · [Load the portable Linux image](../infra/private-vm.md#load-a-reviewed-release-archive)

### Verify package provenance

Download the ZIP or Linux image and `SHA256SUMS.txt` from the same release. Compare your archive hash to its entry. With the GitHub CLI installed and signed in, verify the build attestation against the source commit named by the release tag.

~~~sh
archive=rust-rag-linux-amd64.tar   # Or rust-rag-1.1.0-windows-x64.zip
source_commit=$(gh api repos/skigsy/AnyRAG/commits/v1.1.0 --jq .sha)
gh attestation verify "$archive" --repo skigsy/AnyRAG \
  --source-digest "$source_commit" \
  --source-ref refs/tags/v1.1.0 \
  --signer-workflow skigsy/AnyRAG/.github/workflows/release-provenance.yml \
  --deny-self-hosted-runners
~~~

The Windows ZIP contains a CycloneDX inventory of the selected dependencies. The Linux release provides a CycloneDX inventory of the image filesystem plus a separate inventory of the locked Rust build graph. The [Cargo advisory check](../scripts/check_advisories.py) queries OSV for exact locked registry versions. It accepts the unscored `paste@1.0.15` maintenance notice and fails on other returned advisories. [RustSec maintenance notice](https://rustsec.org/advisories/RUSTSEC-2024-0436.html)

## Graph retrieval and link accuracy

| Question | Workload and result |
| --- | --- |
| Can graph traversal recover supporting passages? | On a public, frozen **synthetic development fixture**, hybrid search found both passages for **10/24** two-hop questions. Hybrid plus graph found both for **23/24**. All three Windows runs used fresh indexes and a top-five passage budget. The outcome measures passage retrieval. [Replay the public fixture](#reproduce-the-mixed-graph-quality-run) |
| Can the model-selected prose path publish a false relationship? | Yes. On a **separate blind synthetic holdout** of 31 documents and 27 target links, an earlier development prose extractor with pinned Qwen3-4B stored **5 correct links and 29 false links**. A verbatim quote can describe an example, a retired configuration, or a relationship in the opposite direction. This is a historical result for the pinned run. |
| What did local source-first review achieve? | On a **different blind synthetic fixture** with 27 eligible chunks and 12 target declared links, the opt-in lane with pinned Qwen3-14B proposed **12 correct and six false links**. A separate reviewer saw the source context and decided before seeing the oracle. It confirmed all 12 correct links, dismissed six, and published **zero false links**. Three cited paths were attributable to graph traversal. The source-first predecessor without review published the same 12 correct links plus six false links on that fixture. |

The three rows answer different questions and use different fixtures. Use [source-first review](setup.md#opt-into-source-first-extraction) for local Markdown and text when a relationship will drive decisions. The verified reference uses a loopback Qwen3-14B verifier, BGE embeddings and Windows Vulkan. An unrecognized sentence can produce no proposal. A document above 12,000 characters can remain pending. The lane requires a text-only index. Proposal accuracy and reviewed publication are separate outcomes.

The historical development prose runs used the same build and pinned model across five distinct blind synthetic sets. Reversed links count toward the false total.

| Documents | Labeled links | Correct published | False published | Reversed |
| ---: | ---: | ---: | ---: | ---: |
| 10 | 9 | 0 on the targeted catalog subset | 5 | No separate count |
| 24 | 17 | 2 | 18 | 8 |
| 33 | 26 | 14 | 16 | 9 |
| 23 | 21 | 7 | 21 | 14 |
| 31 | 27 | 5 | 29 | 18 |

The [aggregate receipt](evidence-results.json) gives the executable, model, fixture, and archived score report digests. These sets were built to expose specific mistakes. Their ratios do not estimate error rates on production documents.

### Reproduce the mixed graph quality run

The [synthetic fixture bundle](../examples/graph-quality/fixture.json) preserves the frozen manifest and all 75 input files. It contains 74 invented documents and 33 questions, including 24 two-hop questions. The [runner](../scripts/rehearse_graph_quality.py) checks the manifest and each input, builds three independent indexes, and records graph-on and graph-off queries over the same top-five hybrid evidence budget. It also verifies that a source and its saved evidence retire after deletion. The runner reads the questions and source documents. The [scorer](../scripts/score_graph_quality.py) checks the digest of the [synthetic answer key](../examples/graph-quality/answers.json) and reads it after the run.

Use Python 3.11+, Rust from the pinned toolchain, and the six BGE files listed by `rust-rag model-manifest`. Obtain Qwen3-4B Q4_K_M from the pinned model revision in the [private VM guide](../infra/private-vm.md#verify-a-generated-graph-rag-answer). The runner verifies the weights, extracted ONNX Runtime library, and `llama-server` executable. Verify the archives before unpacking them. The Windows Vulkan reference used `llama-b12207-bin-win-vulkan-x64.zip` with SHA-256 `c8c84eff8b7eae83602d3cf2dd6defe16c47d1011b4a2514905c1817e3a909ca`. For Windows x64, use ONNX Runtime 1.24.4; the archive SHA-256 is `d2319fddfb6ea4db99ccc4b60c85c517bcd855721f5daa6a06d40d7cb2ee2357`. The separately selectable `linux-cpu` profile uses the pinned Ubuntu x64 b12207 archive from the VM guide and the [pinned Linux ONNX Runtime library](setup.md#local-shared-reranking). Record that profile separately from the Windows Vulkan results.

Build with `cargo build --locked --release --bin rust-rag --features inference`. From the repository root, run the following in PowerShell on the Windows reference setup. Replace the paths with your verified assets and an unused local output directory whose parent already exists. The runner starts and stops the model on loopback port 17917.

~~~powershell
python -B scripts/rehearse_graph_quality.py --profile windows-vulkan `
  --binary target/release/rust-rag.exe --bge-files C:/model-assets/bge `
  --ort-library C:/model-assets/ort/onnxruntime.dll `
  --llama-server C:/model-assets/llama/llama-server.exe `
  --model C:/model-assets/Qwen3-4B-Q4_K_M.gguf --output C:/benchmarks/graph-replay
python -B scripts/score_graph_quality.py --answer-key examples/graph-quality/answers.json `
  C:/benchmarks/graph-replay/run-1.json C:/benchmarks/graph-replay/run-2.json `
  C:/benchmarks/graph-replay/run-3.json
~~~

The scorer reports the extraction, retrieval, citation, and answer-control checks. Preserve the three run receipts with the score. The runner reports deletion retirement separately. Use this fixture for reproduction and a fresh blind fixture for any future held-out claim.

## Shared operation and recovery

The [operator rehearsal](rehearsal.md) runs the actual CLI and signed HTTP service on disposable synthetic inputs. It records source review, citation reopening, graph provenance, approval, dismissal, revoked access, and reconciliation after restore. The deterministic model response keeps the lifecycle check repeatable.

Run this from the source checkout with Rust from the pinned toolchain. It uses invented sources, temporary keys and a deterministic loopback model.

```sh
cargo test --locked --features shared --lib signed_http_company_graph_publication_review_and_restore
```

The signed HTTP check passed during development locally on Windows and on an unprivileged offline Linux run. It verifies exact citations, automatic publication from authorized declarations, a review queue for other valid candidates, rejection of reviewers without both grants, stale decision rejection, conflict handling, and restore followed by reconciliation and reopening. This verifies the policy and lifecycle with controlled inputs. [Signed HTTP test](../crates/rust-rag/src/shared_tests.rs) · [Company policy runbook](../infra/private-vm.md#review-shared-graph-relationships)

An earlier image passed 23 shared lifecycle checks on a fresh private Ubuntu VM, including TLS and non-root container controls. The AWS templates passed lint and the AWS reference image started offline in hosted CI. A live AWS deployment, Neptune service, outside-team rollout, and production delivery timings remain unverified. [Fresh VM replay](#fresh-linux-vm-replay) · [AWS reference](../infra/aws-deployment.md)

### Fresh Linux VM replay

A separate internal operator loaded the same image into a fresh Ubuntu 24.04 VirtualBox VM running the standard Linux 6.8 generic kernel and Docker 29.1.3. Three attached virtual block devices held the ext4 state, backup, and policy mounts. The unchanged lifecycle oracle passed all 23 checks through a TLS 1.3 proxy on the VM host and a Docker bridge published to guest loopback. Both service starts confirmed the non-root user, dropped capabilities, read-only root and input mounts, and bounded `noexec` temporary storage. The harness verified that an outbound connection to a controlled bridge listener was blocked.

A temporary namespace anchor held the loopback publication so the synthetic signing-key and model endpoints could survive a service restart. The shipped image command was unchanged. The operator removed the test containers, bridge, and fixture files. The redacted receipt SHA-256 is `3fb04ef43436d9713fafe97e0959fc9534459391d37740acce35301cccebdab2`. This verifies the local Linux VM lifecycle with that test adapter. Live network ingress, hosted identity and models, cloud storage and teardown, and outside-team delivery still require their own runs.

### Linux shared upgrade and rollback

A second build from the reviewed source produced the same Linux executable SHA-256, `ae2cded7a9de7582e59130020c7934bf2f15c471ab6234e8b6fc0d80fc6bcd36`. The original image was separately tested in the fresh VM replay.

An isolated Linux run passed 16 compatibility and recovery checks between two development builds. An unchanged lexical profile opened the same state in both directions and preserved citation access rules. The graph extractor changed its fingerprint, so both builds rejected the other version’s graph index and backup. The candidate built a fresh graph from current sources and preserved the exact citations for unchanged passages. Rollback ran the previous executable with its matching backup and no routed client traffic. After restore and reconciliation, the reopened service withheld revoked and deleted evidence, returned the retained passage, and extracted a graph hop from a new source.

## Dashboard overhead

Dashboard polling has missed its p95 overhead budget in two Windows qualifications. The first development run added **1.39 ms** against a **1.06 ms** budget. The second development run measured warm query p95 at **23.4343 ms** with recording off and **25.1825 ms** with recording on and a poller. The **1.7482 ms** increase exceeded the **1.171715 ms** budget by **0.576485 ms**. Both ingest arms passed, and all 36 scheduled polls were available in the second run. These measurements cover one Windows host.
