# Shared operator console and Graph review

The shared console shows collection coverage and graph work behind the same per-user gateway as signed Graph review. Operators can run an evidence trace, check current citation access, and open proposals with their source quotes. [Shared Graph review](../private-vm.md#review-shared-graph-relationships) · [Scoped decision contract](../private-vm.md#look-up-a-reviewed-relationship)

## Private gateway

Serve `index.html`, `review.css`, `review.js`, `operations.js` and `diagnostics.js` at `/review/` on the private HTTPS gateway. Serve JavaScript as `text/javascript`. The same browser origin provides these routes to the private AnyRAG upstream. Add the operator status and query routes to the gateway allowlist to enable coverage and evidence tracing.

| Method | Path |
| --- | --- |
| GET | `/v1/operator/status` |
| POST | `/v1/query` |
| POST | `/v1/graph-review/list` |
| GET | `/v1/evidence/<64-character hex chunk ID>` |
| POST | `/v1/graph-review/decide` |
| POST | `/v1/relationship/guided` |

- Require a per-user authenticated session from the deployment's trusted OIDC integration. Use an opaque `Secure`, `HttpOnly`, same-site session cookie. Keep access tokens server-side.
- Forward only the signed-in user's short-lived JWT for AnyRAG's configured issuer and audience. Strip client-supplied authorization and identity headers at the proxy boundary and inject the upstream authorization server-side. AnyRAG applies its existing collection read and reviewer grants. Keep its upstream listener private. The general TLS proxy example does not establish a browser login session.
- For each browser POST, have the gateway reject missing, null or nonmatching `Origin`, require `Content-Type: application/json` and the `X-AnyRAG-Review: 1` request marker, and reject cross-origin CORS. Compare against the configured HTTPS origin. Treat the marker as an extra browser request check and the per-user session as identity. Apply the gateway's normal request-size and rate limits.
- Send `Cache-Control: no-store`, `Referrer-Policy: no-referrer`, `X-Content-Type-Options: nosniff` and a Content Security Policy with `default-src 'none'; script-src 'self'; style-src 'self'; connect-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'none'`. Disable response caching and avoid logging JWTs, source text, decision bodies or cookies.

Before enabling the page, verify that a signed reviewer can list and decide in a granted collection, a read-only identity cannot list or decide, a revoked session cannot reopen protected evidence, and a cross-origin POST is rejected at the gateway. The page's checks supplement the gateway and AnyRAG authorization.

## Inspect shared operations

Select a collection on **Overview** and open a watchlist item. **Coverage** shows current indexing and enrichment state. The observation time is when AnyRAG read the status. Review counts marked with a plus sign are lower bounds from a bounded queue. Deployment maintenance and sync details require the existing admin grant.

Run an explicit trace in **Activity** to see retrieved evidence, directed paths, and any context omissions reported by the service. **Sources** checks whether a saved citation reopens for the signed-in user. A non-reopening citation can be unavailable, retired, or out of scope. Query text, excerpts, and citation check results stay in the page session. Answer-model use is opt-in per trace and follows current deployment grants.

## Review a relationship

1. Enter the collection and load **Pending proposals** or **Scoped reviews**. Open the primary citation to verify its exact quote and indexed revision.
2. For a pending proposal or a scoped review with stale context, reopen a context chunk ID from the same collection. Obtain the ID from authorized search or an existing citation. Select the exact words establishing the setting, or add the full passage if it is at most 512 Unicode characters. Select up to four quotes and enter the exact setting tags, one per line.
3. Check the direction and context, confirm, then approve. Replacement creates a new signed decision for the stale scoped review. A stale or changed-access response requires reloading the queue. If the service is busy, retry the same decision. The page keeps its idempotency key while the payload stays the same.

The page shows committed indexed text. The decision endpoint checks current source bytes and policy before committing.

## Confirm a read-only lookup

Open **Confirm a relationship lookup** and enter a starting entity, an ordered list of relationship types and the setting if one applies. Enter intermediate names if they are part of the question. Preview displays the starting entity, ordered relationships, resolved intermediate entities and setting. Confirm to see the current result and reopen its citations. If the interpretation changes or has no current support, preview again. The lookup uses the current reader's access and requires no review permission.
