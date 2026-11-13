//! Opaque, rebuildable Neptune projection of current SQLite graph assertions.

use super::{region, run, sdk_config};
use anyhow::{Result, anyhow, ensure};
use aws_sdk_neptunedata::Client;
use aws_smithy_types::{Document, Number};
use hmac::{Hmac, Mac};
use rusqlite::Connection;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};

const MAX_COLLECTIONS: usize = 128;
const MAX_REVISIONS: usize = 10_000;
const MAX_NEIGHBORS: usize = 1001;
const MAX_PARAMETERS: usize = 1024 * 1024;
const MAX_EDGES: usize = 100_000;
const BATCH: usize = 128;
const CLEANUP_BATCHES: usize = 1024;

const STAGE: &str = "
UNWIND $edges AS edge
MERGE (a:RustRagEntity {`~id`: edge.a})
SET a.collection = $collection, a.generation = $generation
MERGE (b:RustRagEntity {`~id`: edge.b})
SET b.collection = $collection, b.generation = $generation
MERGE (a)-[r:RUST_RAG_LINK {`~id`: edge.id}]->(b)
SET r.collection = $collection, r.generation = $generation,
    r.revision = edge.revision, r.anchor = edge.anchor
RETURN count(r) AS written";

const PUBLISH: &str = "
MERGE (p:RustRagProjection {`~id`: $collection})
SET p.generation = $generation
RETURN p.generation AS generation";

const CURRENT: &str = "
MATCH (p:RustRagProjection {`~id`: $collection})
RETURN p.generation AS generation";

const REMOVE_OLD_EDGES: &str = "
MATCH ()-[r:RUST_RAG_LINK]->()
WHERE r.collection = $collection AND r.generation <> $generation
WITH r LIMIT 256
DELETE r
RETURN count(*) AS removed";

const REMOVE_OLD_NODES: &str = "
MATCH (n:RustRagEntity)
WHERE n.collection = $collection AND n.generation <> $generation
WITH n LIMIT 256
DELETE n
RETURN count(*) AS removed";

const NEIGHBORS: &str = "
UNWIND $targets AS target
OPTIONAL MATCH (p:RustRagProjection {`~id`: target.collection})
WITH count(CASE WHEN p.generation = target.generation THEN 1 ELSE null END) = $scope_count AS current
OPTIONAL MATCH (n:RustRagEntity {`~id`: $node})-[r:RUST_RAG_LINK]-()
WHERE current AND r.collection = $collection
  AND r.generation = $generation AND r.revision IN $revisions
RETURN DISTINCT current, r.anchor AS anchor
ORDER BY anchor LIMIT 1002";

fn unavailable() -> anyhow::Error {
    anyhow!(
        "The scoped Neptune graph is unavailable. Check projection readiness and request bounds."
    )
}

fn bounded_hex(value: &str, length: usize) -> bool {
    value.len() == length && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn anchor(value: &str) -> bool {
    value
        .strip_prefix("edge_")
        .is_some_and(|value| bounded_hex(value, 64))
}

fn endpoint_region(endpoint: &str) -> Option<&str> {
    let address = endpoint.strip_prefix("https://")?;
    let address = address.strip_suffix('/').unwrap_or(address);
    let host = address.strip_suffix(":8182").unwrap_or(address);
    let suffix = if host.ends_with(".cn") {
        ".neptune.amazonaws.com.cn"
    } else {
        ".neptune.amazonaws.com"
    };
    let prefix = host.strip_suffix(suffix)?;
    let labels: Vec<_> = prefix.split('.').collect();
    let valid = labels.len() == 3
        && prefix.len() <= 230
        && labels.iter().all(|part| {
            !part.is_empty()
                && part.len() <= 63
                && !part.starts_with('-')
                && !part.ends_with('-')
                && part
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        })
        && labels[1].strip_prefix("cluster-").is_some_and(|id| {
            !id.is_empty() && !id.starts_with("ro-") && !id.starts_with("custom-")
        });
    valid.then(|| labels[2])
}

fn read_key(value: &Value) -> Result<[u8; 32]> {
    let error = || {
        anyhow!(
            "Configure one protected Neptune key reference containing 64 hexadecimal characters."
        )
    };
    let secret = match (value.get("key_file"), value.get("key_env")) {
        (Some(file), None) => {
            let path = Path::new(file.as_str().ok_or_else(error)?);
            ensure!(path.is_absolute(), "{error}", error = error());
            let info = std::fs::symlink_metadata(path).map_err(|_| error())?;
            ensure!(
                info.is_file() && info.len() <= 128,
                "{error}",
                error = error()
            );
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                ensure!(info.mode() & 0o077 == 0, "{error}", error = error());
            }
            let bytes = crate::files::read(path, &mut 128).map_err(|_| error())?;
            String::from_utf8(bytes).map_err(|_| error())?
        }
        (None, Some(env)) => {
            let env = env
                .as_str()
                .filter(|env| {
                    (1..=128).contains(&env.len())
                        && env.as_bytes()[0].is_ascii_uppercase()
                        && env.bytes().all(|byte| {
                            byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'_'
                        })
                })
                .ok_or_else(error)?;
            std::env::var(env).map_err(|_| error())?
        }
        _ => return Err(error()),
    };
    let secret = secret.trim();
    ensure!(bounded_hex(secret, 64), "{error}", error = error());
    let mut key = [0; 32];
    for (position, value) in key.iter_mut().enumerate() {
        *value =
            u8::from_str_radix(&secret[position * 2..position * 2 + 2], 16).map_err(|_| error())?;
    }
    Ok(key)
}

pub struct NeptuneGraph {
    client: Client,
    key: [u8; 32],
}

pub struct ProjectionScope {
    generation: String,
    revisions: Vec<(String, String)>,
}

struct ProjectionEdge {
    id: String,
    subject: String,
    object: String,
    source: String,
    revision: String,
}

fn local_revisions(db: &Connection) -> Result<Vec<(String, String)>> {
    let (source_join, revision) = crate::store::source_revision_sql(db)?;
    let mut statement = db.prepare(&format!(
        "SELECT s.source_id,{revision} FROM sources s {source_join} ORDER BY s.source_id LIMIT ?1"
    ))?;
    let values = statement
        .query_map([MAX_REVISIONS as i64 + 1], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    ensure!(values.len() <= MAX_REVISIONS, "{}", unavailable());
    Ok(values)
}

fn scope_in_snapshot(db: &Connection) -> Result<ProjectionScope> {
    let revisions = local_revisions(db)?;
    let mut digest = Sha256::new();
    digest.update(b"rust-rag-neptune-v1");
    for (source, revision) in &revisions {
        ensure!(
            bounded_hex(source, 64) && (1..=1024).contains(&revision.len()),
            "{}",
            unavailable()
        );
        digest.update(source.as_bytes());
        digest.update((revision.len() as u64).to_be_bytes());
        digest.update(revision.as_bytes());
    }
    digest.update(b"|");
    let mut statement =
        db.prepare("SELECT edge_id FROM graph_assertions ORDER BY edge_id LIMIT ?1")?;
    let mut rows = statement.query([MAX_EDGES as i64 + 1])?;
    let mut count = 0;
    while let Some(row) = rows.next()? {
        let id: String = row.get(0)?;
        count += 1;
        ensure!(count <= MAX_EDGES && anchor(&id), "{}", unavailable());
        digest.update(id.as_bytes());
    }
    Ok(ProjectionScope {
        generation: format!("{:x}", digest.finalize()),
        revisions,
    })
}

pub fn projection_scope(db: &mut Connection) -> Result<ProjectionScope> {
    let tx = db.transaction()?;
    let scope = scope_in_snapshot(&tx)?;
    tx.commit()?;
    Ok(scope)
}

fn load_edges(db: &Connection) -> Result<Vec<ProjectionEdge>> {
    let (source_join, revision) = crate::store::source_revision_sql(db)?;
    let mut statement = db.prepare(&format!(
        "SELECT a.edge_id,a.subject_key,a.object_key,s.source_id,{revision}
         FROM graph_assertions a JOIN chunks c ON c.row_id=a.chunk_row
         JOIN sources s ON s.source_id=c.source_id {source_join} ORDER BY a.edge_id LIMIT ?1"
    ))?;
    let rows = statement
        .query_map([MAX_EDGES as i64 + 1], |row| {
            Ok(ProjectionEdge {
                id: row.get(0)?,
                subject: row.get(1)?,
                object: row.get(2)?,
                source: row.get(3)?,
                revision: row.get(4)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    ensure!(rows.len() <= MAX_EDGES, "{}", unavailable());
    Ok(rows)
}

fn valid_collection(collection: &str) -> bool {
    !collection.is_empty() && collection.len() <= 200 && !collection.chars().any(char::is_control)
}

fn opaque(key: &[u8; 32], kind: &str, parts: &[&str]) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("HMAC accepts a 32-byte key");
    mac.update(kind.as_bytes());
    for part in parts {
        mac.update(&(part.len() as u64).to_be_bytes());
        mac.update(part.as_bytes());
    }
    format!("rust_rag_{kind}_{:x}", mac.finalize().into_bytes())
}

fn encode_edge(
    key: &[u8; 32],
    collection: &str,
    generation: &str,
    edge: &ProjectionEdge,
) -> Result<Value> {
    ensure!(
        anchor(&edge.id)
            && (1..=512).contains(&edge.subject.len())
            && (1..=512).contains(&edge.object.len()),
        "{}",
        unavailable()
    );
    Ok(json!({
        "id": opaque(key, "edge", &[collection,generation,&edge.id]),
        "a": opaque(key, "entity", &[collection,generation,&edge.subject]),
        "b": opaque(key, "entity", &[collection,generation,&edge.object]),
        "revision": opaque(key, "revision", &[collection,&edge.source,&edge.revision]),
        "anchor": edge.id,
    }))
}

fn results(rows: &Document) -> Result<&[Document]> {
    rows.as_array().map(Vec::as_slice).ok_or_else(unavailable)
}

fn validated_anchors(rows: &Document, limit: usize) -> Result<Vec<String>> {
    let rows = results(rows)?;
    ensure!(
        !rows.is_empty() && rows.len() <= MAX_NEIGHBORS + 1,
        "{}",
        unavailable()
    );
    let mut anchors = BTreeSet::new();
    for row in rows {
        let row = row.as_object().ok_or_else(unavailable)?;
        ensure!(
            row.get("current").and_then(Document::as_bool) == Some(true),
            "{}",
            unavailable()
        );
        match row.get("anchor").ok_or_else(unavailable)? {
            Document::Null => {}
            Document::String(id) if anchor(id) => {
                anchors.insert(id.clone());
            }
            _ => return Err(unavailable()),
        }
    }
    Ok(anchors.into_iter().take(limit).collect())
}

impl NeptuneGraph {
    pub fn new(value: &Value) -> Result<Self> {
        let endpoint = value["endpoint"]
            .as_str()
            .unwrap_or_default()
            .trim_end_matches('/');
        let endpoint_region = endpoint_region(endpoint).ok_or_else(|| {
            anyhow!(
                "Configure the private HTTPS endpoint of an IAM-enabled Neptune Database cluster."
            )
        })?;
        ensure!(
            value.as_object().is_some_and(|fields| fields
                .keys()
                .all(
                    |field| ["endpoint", "key_file", "key_env", "region"].contains(&field.as_str())
                )),
            "Use the documented Neptune projection settings."
        );
        let key = read_key(value)?;
        let selected = region(
            value
                .get("region")
                .or(Some(&Value::String(endpoint_region.to_owned()))),
        )?;
        ensure!(
            selected.as_deref() == Some(endpoint_region),
            "The Neptune endpoint and AWS region must match."
        );
        let endpoint = if endpoint.ends_with(":8182") {
            endpoint.to_owned()
        } else {
            format!("{endpoint}:8182")
        };
        let client = run(async move {
            let shared = sdk_config(selected).await?;
            Ok(Client::from_conf(
                aws_sdk_neptunedata::config::Builder::from(&shared)
                    .endpoint_url(endpoint)
                    .build(),
            ))
        })?;
        Ok(Self { client, key })
    }

    fn query(&self, statement: &'static str, parameters: Value) -> Result<Document> {
        let parameters = parameters.to_string();
        ensure!(parameters.len() <= MAX_PARAMETERS, "{}", unavailable());
        let client = self.client.clone();
        run(async move {
            let response = client
                .execute_open_cypher_query()
                .open_cypher_query(statement)
                .parameters(parameters)
                .send()
                .await
                .map_err(|_| unavailable())?;
            Ok(response.results)
        })
    }

    fn identifiers(&self, collection: &str, generation: &str) -> (String, String) {
        (
            opaque(&self.key, "collection", &[collection]),
            opaque(&self.key, "generation", &[collection, generation]),
        )
    }

    pub fn scoped_neighbors(
        &self,
        entity: &str,
        collection: &str,
        scopes: &BTreeMap<String, ProjectionScope>,
        limit: usize,
    ) -> Result<Vec<String>> {
        ensure!(
            (1..=MAX_NEIGHBORS).contains(&limit)
                && (1..=512).contains(&entity.len())
                && (1..=MAX_COLLECTIONS).contains(&scopes.len())
                && valid_collection(collection)
                && scopes.keys().all(|label| valid_collection(label)),
            "{}",
            unavailable()
        );
        let scope = scopes.get(collection).ok_or_else(unavailable)?;
        let (collection_id, generation) = self.identifiers(collection, &scope.generation);
        let targets: Vec<_> = scopes
            .iter()
            .map(|(label, scope)| {
                let (collection, generation) = self.identifiers(label, &scope.generation);
                json!({"collection": collection, "generation": generation})
            })
            .collect();
        let revisions: Vec<_> = scope
            .revisions
            .iter()
            .map(|(source, revision)| {
                opaque(&self.key, "revision", &[collection, source, revision])
            })
            .collect();
        let node = opaque(
            &self.key,
            "entity",
            &[collection, &scope.generation, entity],
        );
        let rows = self.query(
            NEIGHBORS,
            json!({
                "targets": targets, "scope_count": targets.len(), "revisions": revisions,
                "collection": collection_id, "generation": generation, "node": node,
            }),
        )?;
        validated_anchors(&rows, limit)
    }

    pub fn reconcile(&self, db: &mut Connection, collection: &str) -> Result<bool> {
        ensure!(valid_collection(collection), "{}", unavailable());
        // The single service writer and this SQLite lock keep the scope unchanged
        // until the remote marker is activated. Incomplete batches remain hidden.
        let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let scope = scope_in_snapshot(&tx)?;
        let (collection_id, generation) = self.identifiers(collection, &scope.generation);
        let parameters = json!({"collection": collection_id, "generation": generation});
        let rows = self.query(CURRENT, json!({"collection": collection_id}))?;
        let rows = results(&rows)?;
        let current = rows.len() == 1
            && rows[0]
                .as_object()
                .and_then(|row| row.get("generation"))
                .and_then(Document::as_string)
                .is_some_and(|value| value == generation);
        if !current {
            let edges = load_edges(&tx)?;
            for batch in edges.chunks(BATCH) {
                let batch = batch
                    .iter()
                    .map(|edge| encode_edge(&self.key, collection, &scope.generation, edge))
                    .collect::<Result<Vec<_>>>()?;
                let response = self.query(
                    STAGE,
                    json!({
                        "collection": collection_id, "generation": generation, "edges": batch,
                    }),
                )?;
                ensure!(
                    count(&response, "written") == Some(batch.len()),
                    "{}",
                    unavailable()
                );
            }
            let response = self.query(PUBLISH, parameters.clone())?;
            let rows = results(&response)?;
            ensure!(
                rows.len() == 1
                    && rows[0]
                        .as_object()
                        .and_then(|row| row.get("generation"))
                        .and_then(Document::as_string)
                        .is_some_and(|value| value == generation),
                "{}",
                unavailable()
            );
        }
        let cleaned = self.cleanup(parameters).unwrap_or(false);
        tx.commit()?;
        Ok(cleaned)
    }

    fn cleanup(&self, parameters: Value) -> Result<bool> {
        for statement in [REMOVE_OLD_EDGES, REMOVE_OLD_NODES] {
            let mut complete = false;
            for _ in 0..CLEANUP_BATCHES {
                let rows = self.query(statement, parameters.clone())?;
                let removed = count(&rows, "removed")
                    .filter(|count| *count <= 256)
                    .ok_or_else(unavailable)?;
                if removed < 256 {
                    complete = true;
                    break;
                }
            }
            if !complete {
                return Ok(false);
            }
        }
        Ok(true)
    }
}

fn count(rows: &Document, key: &str) -> Option<usize> {
    let [row] = results(rows).ok()? else {
        return None;
    };
    let number = row.as_object()?.get(key)?.as_number()?;
    match number {
        Number::PosInt(value) => usize::try_from(*value).ok(),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn projection_never_sends_labels_or_source_ids() {
        let edge = ProjectionEdge {
            id: format!("edge_{}", "a".repeat(64)),
            subject: "synthetic subject label".into(),
            object: "synthetic object label".into(),
            source: "source identity".into(),
            revision: "source revision".into(),
        };
        let result = encode_edge(&[7; 32], "restricted collection", "generation", &edge).unwrap();
        for private in [
            edge.subject,
            edge.object,
            edge.source,
            edge.revision,
            "restricted collection".into(),
        ] {
            assert!(!result.to_string().contains(&private));
        }
        assert_ne!(
            opaque(&[7; 32], "entity", &["ab", "c"]),
            opaque(&[7; 32], "entity", &["a", "bc"])
        );
    }

    #[test]
    fn stale_or_malformed_scope_has_no_graph_anchors() {
        let id = format!("edge_{}", "a".repeat(64));
        let row = |current| {
            Document::Object(std::collections::HashMap::from([
                ("current".into(), Document::Bool(current)),
                ("anchor".into(), Document::String(id.clone())),
            ]))
        };
        assert_eq!(
            validated_anchors(&Document::Array(vec![row(true)]), 1).unwrap(),
            vec![id.clone()]
        );
        assert!(validated_anchors(&Document::Array(vec![row(true), row(false)]), 1).is_err());
        assert!(validated_anchors(&Document::Array(vec![]), 1).is_err());
        assert_eq!(
            endpoint_region("https://test.cluster-sample.us-east-1.neptune.amazonaws.com:8182"),
            Some("us-east-1")
        );
        assert!(
            endpoint_region("https://test.cluster-ro-sample.us-east-1.neptune.amazonaws.com:8182")
                .is_none()
        );
        assert!(
            endpoint_region(
                "https://test.cluster-sample.us-east-1.neptune.amazonaws.com.evil.test"
            )
            .is_none()
        );
    }
}
