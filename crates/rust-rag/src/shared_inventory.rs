use super::{
    ACCESS_CHANGED, ApiError, ApiResult, Principal, Service, files, hex_id, label, store,
    unavailable,
};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use ring::hmac;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

pub(crate) const API_PATH: &str = "/v1/sources/list";
pub(crate) const MAX_PAGE_ENTRIES: usize = 100;
pub(crate) const DEFAULT_PAGE_ENTRIES: usize = 50;
pub(crate) const CURSOR_LENGTH: usize = 111;
const MAX_PAGE_BYTES: usize = 128 * 1024;
const INVALID_INVENTORY: ApiError = ApiError::new(
    400,
    "INVENTORY_REQUEST",
    "Choose a collection, a limit from 1 to 100, and an optional inventory cursor.",
);
const INVENTORY_NOT_FOUND: ApiError = ApiError::new(
    404,
    "INVENTORY_NOT_FOUND",
    "No readable inventory matches that collection.",
);
const INVENTORY_CHANGED: ApiError = ApiError::new(
    409,
    "INVENTORY_CHANGED",
    "Inventory or access changed. Start again without a cursor.",
);
const INVENTORY_LIMIT: ApiError = ApiError::new(
    503,
    "INVENTORY_LIMIT",
    "The committed inventory exceeds its supported size.",
);

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Request {
    collection: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    limit: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cursor: Option<String>,
}

impl Request {
    pub(crate) fn valid(&self) -> bool {
        label(&self.collection)
            && (1..=MAX_PAGE_ENTRIES).contains(&self.limit.unwrap_or(DEFAULT_PAGE_ENTRIES))
            && self
                .cursor
                .as_deref()
                .is_none_or(|cursor| parse_cursor(cursor).is_ok())
    }
}

fn parse_cursor(cursor: &str) -> ApiResult<(&str, [u8; 32])> {
    if cursor.len() != CURSOR_LENGTH {
        return Err(INVALID_INVENTORY);
    }
    let (after, tag) = cursor
        .strip_prefix("v1.")
        .and_then(|value| value.split_once('.'))
        .filter(|(after, _)| hex_id(after, 64))
        .ok_or(INVALID_INVENTORY)?;
    let bytes = URL_SAFE_NO_PAD.decode(tag).map_err(|_| INVALID_INVENTORY)?;
    Ok((after, bytes.try_into().map_err(|_| INVALID_INVENTORY)?))
}

fn hash_field(digest: &mut Sha256, value: &str) {
    digest.update((value.len() as u64).to_le_bytes());
    digest.update(value.as_bytes());
}

fn cursor_input(digest: &[u8], after: &str) -> [u8; 96] {
    let mut message = [0; 96];
    message[..32].copy_from_slice(digest);
    message[32..].copy_from_slice(after.as_bytes());
    message
}

impl Service {
    pub(super) fn inventory(&self, principal: &Principal, request: Request) -> ApiResult<Value> {
        if !request.valid() {
            return Err(INVALID_INVENTORY);
        }
        let limit = request.limit.unwrap_or(DEFAULT_PAGE_ENTRIES);
        let cursor = request.cursor.as_deref().map(parse_cursor).transpose()?;
        let after = cursor.as_ref().map_or("", |(after, _)| *after);

        let _read = self.read_during_sync()?;
        self.ready()?;
        let (policy, policy_digest) = self.policy()?;
        let scope = policy.scope(principal)?;
        let collection = &request.collection;
        if !scope.contains(collection) {
            return Err(INVENTORY_NOT_FOUND);
        }

        let db = self
            .open_collection(collection, false)
            .map_err(unavailable)?;
        let (source_join, revision) = store::source_revision_sql(&db).map_err(unavailable)?;
        let sql = format!(
            "SELECT s.source_id,s.path,s.source_hash,{revision}
             FROM sources s {source_join}
             ORDER BY s.source_id"
        );
        let mut query = db.prepare(&sql).map_err(unavailable)?;
        let mut rows = query.query([]).map_err(unavailable)?;
        let mut digest = Sha256::new();
        for value in [
            "shared-source-inventory-v1",
            &policy_digest,
            &principal.subject,
            collection,
        ] {
            hash_field(&mut digest, value);
        }
        let mut total = 0;
        let mut items = Vec::with_capacity(limit);
        let mut page_bytes = 0;
        let mut last = None;
        let mut has_more = false;
        while let Some(row) = rows.next().map_err(unavailable)? {
            total += 1;
            if total > files::MAX_ENTRIES {
                return Err(INVENTORY_LIMIT);
            }
            let id: String = row.get(0).map_err(unavailable)?;
            let path: String = row.get(1).map_err(unavailable)?;
            let hash: String = row.get(2).map_err(unavailable)?;
            let revision: String = row.get(3).map_err(unavailable)?;
            if !hex_id(&id, 64) {
                return Err(INVENTORY_LIMIT);
            }
            for value in [&id, &path, &hash, &revision] {
                hash_field(&mut digest, value);
            }
            if id.as_str() <= after || has_more {
                continue;
            }
            if items.len() == limit {
                has_more = true;
                continue;
            }
            let mut item =
                json!({"source_id": id,"path":path,"source_hash":hash,"revision":revision});
            self.sources[collection]
                .cite(collection, &mut item)
                .map_err(unavailable)?;
            let item = json!({"source_id":id,"path":item["path"],"revision":item["revision"]});
            let next_bytes = page_bytes + serde_json::to_vec(&item).map_err(unavailable)?.len() + 1;
            if next_bytes > MAX_PAGE_BYTES {
                if items.is_empty() {
                    return Err(INVENTORY_LIMIT);
                }
                has_more = true;
                continue;
            }
            page_bytes = next_bytes;
            last = Some(id);
            items.push(item);
        }

        let (current, current_digest) = self.policy()?;
        if !current.scope(principal)?.contains(collection) {
            return Err(INVENTORY_NOT_FOUND);
        }
        if current_digest != policy_digest {
            return Err(ACCESS_CHANGED);
        }
        self.ready()?;
        let digest = digest.finalize();
        if let Some((after, tag)) = cursor {
            hmac::verify(
                &self.inventory_cursor_key,
                &cursor_input(&digest, after),
                &tag,
            )
            .map_err(|_| INVENTORY_CHANGED)?;
        }
        let next_cursor = last.filter(|_| has_more).map(|after| {
            let tag = hmac::sign(&self.inventory_cursor_key, &cursor_input(&digest, &after));
            format!("v1.{after}.{}", URL_SAFE_NO_PAD.encode(tag.as_ref()))
        });
        Ok(
            json!({"schema_version":1,"collection":collection,"total_sources":total,
            "sources":items,"next_cursor":next_cursor}),
        )
    }
}
