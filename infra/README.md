# Infrastructure as code

Any RAG ships native local profiles, a private shared-service image recipe, and CloudFormation for AWS. See [verification status](../status.md) for what has run and where. Cloud templates and adapters can be used without a maintainer evaluation; apply them only to an authorized account.

## Native local configuration as code

`local.profile.json` declares the text index format. The advanced profile declares the format used by PDFs, vectors and typed graph assertions. Retrieval mode and model endpoints are chosen explicitly per command. These files contain no secrets or personal paths.

~~~sh
rust-rag init --profile infra/local.profile.json
rust-rag doctor --json
~~~

## Keep a personal folder current on Linux

The [user service](anyrag-folder-sync.service) runs folder reconciliation and the [timer](anyrag-folder-sync.timer) schedules it every 15 minutes. Copy both files to `~/.config/systemd/user/`. Set the executable, index, and source paths in the copied service, preserving quotes around paths that contain spaces. Use the same format flags and index you use for manual ingestion.

Initialize the index and complete one manual ingest. Then enable and check the schedule:

~~~sh
systemctl --user daemon-reload
systemctl --user enable --now anyrag-folder-sync.timer
systemctl --user start anyrag-folder-sync.service
systemctl --user status anyrag-folder-sync.service
~~~

The timer runs under your user service manager. An incomplete scan, deferred work, or a failed source makes the service exit unsuccessfully and the next timer event tries again. For details, run the same ingest with `--review --json`. If you enabled dashboard history, **Activity** shows the last ingest, including how many old revisions were retained. Disable the timer with `systemctl --user disable --now anyrag-folder-sync.timer`.

## Deployment targets

Start the company build with the portable single-host profile. It uses the shared Rust service with a trusted OIDC issuer, private TLS, attached storage, and separate policy and backup locations. Each target’s infrastructure recipe specifies what it provisions and what the operator supplies. The AWS reference adds versioned S3 and scoped service roles, with Neptune available as an optional graph projection.

`rust-rag plan <name> --json` returns the recipe and its prerequisites. For shared runtimes, start with `rust-rag service-doctor --profile <file> --json` and set the operator profile before serving. The AWS runbook describes review and deployment steps for a separately authorized account.

| Target | Recipe | Scope |
| --- | --- | --- |
| Personal or Obsidian | [Local profile](local.profile.json) or [vault profile](obsidian.profile.json) | Native files and SQLite. Optional [typed graph](local-graph.profile.json) and [model endpoint](model-endpoint.example.json). |
| Shared on one host | [Shared profile](shared.profile.example.json) and [portable VM runbook](private-vm.md) | Authenticated collection scope, host-attached storage, and independent policy and backup paths. |
| AWS | [Deployment diagram](aws-deployment.md#deployment-architecture), [storage template](aws-storage.yml), [service template](aws.yml), and [AWS runbook](aws-deployment.md) | Private host and S3. Selectable Neptune projection. |
| Azure or other private Linux VM | [Portable VM runbook](private-vm.md) | Same image and evidence contract. The operator provides the host, attached disk, identity, and private TLS listener. |
| Other providers | Provider-specific IaC behind common planning/output contracts | Supported only after independent qualification |

Keep provider-specific infrastructure explicit. A universal abstraction should not hide identity, networking, persistence or cost differences. OpenTofu/Terraform may be useful later; adopting it is not a prerequisite for preserving current CloudFormation work.

## Desired-state contract

The deterministic installer will create a versioned desired-state manifest containing:

- Deployment identity and target/profile; application artifact version/digest.
- Enabled modules and compatible adapter versions/capabilities.
- Source/index storage locations, retention/backup policy and deletion/export behavior.
- Model endpoint references, data-egress policy and secret references only.
- Resource size/budget limits, region, network exposure and identity mode where relevant.
- State/backend location, schema/migration version and ownership tags.

This manifest is a later multi-service/managed-profile contract, not an executable format or a prerequisite for slice 1. Minimal local init/doctor use direct configuration without a provider registry or cloud plan/apply engine. Shared fields describe intent; future providers resolve real resources and expose differences. Allow custom configurations without evaluation; identify actual missing adapters/dependencies or incompatible settings rather than rejecting a combination for lack of qualification. Store user values/state outside Git and sync folders.

Outputs include application endpoint or local socket, resource IDs, health-check entry point, backup location, artifact identity and next steps. Outputs and logs must omit secrets and access-bearing URLs. Per-target adapters own plan/apply/status/update/export/remove behavior and report rollback limitations.

## Delivery pipeline

1. **Validate:** pin tools/dependencies; run formatting/schema/template checks. Reuse the archived `cfn-lint` approach after reviewing its version. Review IAM/network/storage policy and secret/identifier scans.
2. **Build once:** build the identified source commit into immutable packages/images; record digests, SBOM, configuration schema, migration compatibility and IaC revision. Disable historical automatic production push triggers during import.
3. **Plan:** produce a CloudFormation change set or target equivalent; report data movement, replacements/deletions, network changes and bounded cost estimates. Use workload identity for CI, with narrowly scoped planning/deploy roles.
4. **Apply the authorized plan:** only against a selected account/environment, using the verified artifact. No deployment from untrusted pull requests. Changes that spend money, expose data or destroy resources need a concrete authorized action, not a generic instruction inherited from an archive.
5. **Verify:** smoke first on synthetic inputs; exercise ingestion/query/delete, identity/ACL boundaries, service health, backup restore and rollback. Record environment and artifact digest separately from source-only checks.
6. **Promote:** reuse the same digest and IaC revision in the next environment. Retain the previous known-good artifact/configuration and recovery instructions. Separate application rollback from potentially irreversible data migrations.

Local profiles use idempotent init and doctor. The reference service preflight checks SQLite and installed dependencies. Use the [hardening plan](../docs/project/plan.md#post-build-hardening) for release checks and keep native, image, and live-cloud evidence separate.

## Baseline controls

Default data services private; local endpoints loopback. Encrypt persistent data, scope service identities, reference managed/OS secrets, and declare every outbound processing service. Parameterize regions and naming; do not reuse original accounts, tenants, buckets, endpoints or credentials. Add ownership/cost labels and budgets for managed resources. Preserve state intentionally; teardown must distinguish app removal from deletion of a user's documents and indexes.

Each target needs documented prerequisites, supported platforms/regions, exact tool/image versions, state/lock handling, recovery, and realistic resource/cost measurements. Cloud parity is a testable support claim, not a property of a shared manifest.

These define our documentation and support evidence. Users may choose their own infrastructure and IaC integrations without running our evaluation/qualification suites; describe untested status honestly and help configure the target. Normal configuration/credential checks and concrete authorization for resource changes still apply.
