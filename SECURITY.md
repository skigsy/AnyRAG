# Privacy and security boundaries

AnyRAG has an owner-local profile and an optional shared service for one organization. See [status](status.md) for the checks completed on each runtime and deployment.

## Public source boundary

Keep private documents, credentials, runtime state, and backups outside the checkout and build context. Use synthetic or appropriately licensed data in fixtures. Check new source files, assets, and release archives against the public allowlist before publication. Preserve applicable license notices.

## Runtime product requirements

- Private local processing has no external processing API, telemetry or outbound model calls. Disclose initial package/model downloads separately; verify runtime with networking disabled after prerequisites are present.
- Evidence-only describes a capability, not its privacy boundary. A hosted harness may send retrieved text, queries, filenames/paths, metadata, logs and tool/file contents to its provider. Strict-local data needs a local consuming client/harness. Hosted installation assistance must remain on nonsensitive configuration if that boundary is required. AnyRAG's offline checks cannot certify an external harness or guarantee what it reads/sends.
- External generation, embeddings, extraction, OCR, reranking and telemetry are separate data-egress choices. Default telemetry off. Keep document/query text, source URLs and credentials out of operational logs; diagnostic opt-in is bounded and reversible.
- Local service binds to loopback by default. Shared/network exposure requires identity, authorization, TLS and an explicit profile. Restrict browser origins; never combine permissive CORS with privileged sessions.
- Shared access uses the configured issuer and server-owned collection grants. Apply grants before ranking, graph expansion, citation resolution, and model dispatch. The service rereads authoritative policy per request and fails closed when it cannot verify it. Keep collection grants aligned with the source's actual permissions. Write access to an admitted source folder or prefix conveys authority to add content to that collection, so restrict writer access and treat every passage as untrusted.
- Treat retrieved text as data, not tool instructions. No arbitrary code execution in the initial product. No automatic connector permissions, external publishing or source-file mutation based on document content.
- File parsing has size/type/time/decompression bounds and safe paths. Remote URL/webhook connectors are optional; defend against SSRF, redirects, private/metadata-network destinations and oversized responses when implemented.
- Secrets use OS/provider credential mechanisms or managed secret stores, never chat or Git. Deployment manifests contain secret references, not values. Do not copy archived credential stores into a new environment.
- Stable source identities drive idempotent ingestion and derived-data removal. Incomplete/inaccessible/changing directory enumeration never authorizes deleting missing index records. Found files that fail parsing retain their prior versions and are reported separately; they do not block removal of another file known to be missing. Index removal never deletes originals. Include all derived representations and retained exports/backups in policy. Logical removal, DB/WAL purge and forensic storage/snapshot erasure have distinct guarantees.
- Managed profiles require least privilege, encrypted storage, private data-service networking, audit events without sensitive payloads, bounded spend and tested recovery.

Shared restore uses a consistent SQLite backup, starts in maintenance, and reconciles current source inventory and independently versioned access policy before an administrator reopens queries. Backups can retain earlier content until their retention expires. Reviewed facts and aliases also have a separate service control record. Lookups require current source anchors and current grants. Editing or deleting a source can make its evidence unavailable without erasing the retained review record. An unwithdrawn decision can become usable again if the exact evidence returns. Use explicit withdrawal to retire the decision across collection restores. Include review records and stored backup copies in the deployment's retention policy. Neptune stores keyed entity identifiers and opaque anchors with topology. Its snapshots have separate retention, and pending remote cleanup is reported separately from query visibility.

## Checks before public release

1. Review the exact outgoing source, assets, license notices, and Git history.
2. Run the checksum-pinned secret scanner on the allowed tree and its reachable history. Investigate findings without printing private values.
3. Inspect the build context, package and container contents, bundled fixtures, and release attachments.
4. Run synthetic checks that exercise privacy, collection isolation, deletion, and the supported offline flows.
5. Record package digests and provenance. Recheck the files downloaded from the published release.

The public allowlist and build-context rules bound what can enter the release. Content and history scans are separate checks. Run both locally before upload.
The PDF and DOCX decoders run in a native subprocess with bounded byte pipes and a 30-second wall-clock limit. The worker applies CPU and memory limits with Windows Job Objects or Unix resource limits before parsing untrusted bytes. The DOCX reader opens only the package metadata, main document, and optional styles and does not follow external relationships. Run untrusted parsing under the operating-system or container boundary required by your workload. Worker resource limits do not provide filesystem or network isolation.

## SQLite purge constraints

The SQLite store has synthetic logical-deletion and configured on-disk purge checks. Core `secure_delete` alone does not guarantee removal from FTS shadow structures; FTS5 has its own secure-delete option, introduced in SQLite 3.42.0, with older-reader compatibility implications. Check the actual bundled SQLite version/features at runtime; pin compatible support and test both settings rather than assuming a pragma succeeded. [SQLite secure-delete](https://www.sqlite.org/pragma.html#pragma_secure_delete), [FTS5 secure-delete](https://www.sqlite.org/fts5.html#the_secure_delete_configuration_option).

For WAL mode, request a truncate checkpoint after removal and inspect whether it completed: active readers can block it. Report incomplete purge honestly; successful logical deletion is a different result. Canary/store checks and a DB/WAL byte scan can test the bounded purge behavior configured here, but do not establish forensic erasure from storage, snapshots or backups. [SQLite checkpoint semantics](https://www.sqlite.org/pragma.html#pragma_wal_checkpoint).

Report suspected vulnerabilities through a private maintainer channel. If you need help finding one, ask for a private channel in an issue and keep technical details, credentials, and documents out of the public thread.

CLI export requires an explicit new output file outside checkout/known sync locations and uses owner-only POSIX mode; no full-corpus stdout route is exposed. A consuming external assistant remains outside AnyRAG's control. Strong or weak retrieval results remain candidate context with unverified answer support.
