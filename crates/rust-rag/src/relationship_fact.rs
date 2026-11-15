//! Exact source-backed facts and aliases for signed shared relationship lookup.

use super::*;
use serde::Serialize;

const FACT_LIMIT: usize = 32;
const CONFLICT: ApiError = ApiError::new(
    409,
    "GRAPH_REVIEW_CONFLICT",
    "The fact decision conflicts with an existing decision or exceeds the review limit.",
);

pub(in super::super) fn install(db: &Connection) -> Result<()> {
    db.execute_batch(
        "CREATE TABLE IF NOT EXISTS shared_relationship_facts (
            id TEXT PRIMARY KEY NOT NULL,
            collection TEXT NOT NULL,
            fingerprint TEXT NOT NULL,
            subject_key TEXT NOT NULL,
            statement TEXT NOT NULL,
            withdrawn INTEGER NOT NULL DEFAULT 0 CHECK (withdrawn IN (0,1))
        );
        CREATE INDEX IF NOT EXISTS shared_relationship_facts_active
            ON shared_relationship_facts(collection,fingerprint,subject_key,id)
            WHERE withdrawn=0;
        CREATE TABLE IF NOT EXISTS shared_relationship_fact_reviews (
            actor TEXT NOT NULL,
            idempotency_key TEXT NOT NULL,
            request_hash TEXT NOT NULL,
            fact_id TEXT NOT NULL REFERENCES shared_relationship_facts(id),
            action TEXT NOT NULL CHECK (action IN ('add','withdraw')),
            policy_version INTEGER NOT NULL,
            reviewed_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
            PRIMARY KEY(actor,idempotency_key)
        );",
    )?;
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(in super::super) enum FactKind {
    ExplicitNegative,
    DocumentedAbsence,
}

#[derive(Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum AliasKind {
    Entity,
    Setting,
}

#[derive(Deserialize, Serialize)]
#[serde(transparent)]
pub(in super::super) struct FactReview(ReviewAction);

#[derive(Deserialize, Serialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
enum ReviewAction {
    Add {
        collection: String,
        fingerprint: String,
        policy_version: i64,
        idempotency_key: String,
        subject: String,
        predicate: String,
        kind: FactKind,
        target: Option<String>,
        scope: Vec<String>,
        primary: ReviewAnchor,
        #[serde(default)]
        context: Vec<ReviewAnchor>,
    },
    AddAlias {
        collection: String,
        fingerprint: String,
        policy_version: i64,
        idempotency_key: String,
        alias_kind: AliasKind,
        surface: String,
        canonical: String,
        primary: ReviewAnchor,
    },
    Withdraw {
        collection: String,
        fingerprint: String,
        policy_version: i64,
        idempotency_key: String,
        fact_id: String,
    },
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Statement {
    subject: String,
    predicate: String,
    kind: FactKind,
    target: Option<String>,
    scope: Vec<String>,
    anchors: Vec<ReviewAnchor>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct AliasStatement {
    alias_kind: AliasKind,
    surface: String,
    canonical: String,
    primary: ReviewAnchor,
}

#[derive(Serialize)]
#[serde(untagged)]
enum ReviewEntry {
    Fact(Statement),
    Alias(AliasStatement),
}

pub(super) struct Alias {
    pub(super) canonical: String,
    pub(super) evidence: Value,
}

impl AliasKind {
    fn lookup_key(self, surface: &str) -> String {
        // The control separator cannot occur in a valid entity or setting.
        let kind = match self {
            Self::Entity => "entity",
            Self::Setting => "setting",
        };
        format!("\u{1f}{kind}:{}", graph::entity_key(surface))
    }
}

impl ReviewEntry {
    fn lookup_key(&self) -> String {
        match self {
            Self::Fact(statement) => graph::entity_key(&statement.subject),
            Self::Alias(alias) => alias.alias_kind.lookup_key(&alias.surface),
        }
    }

    fn validate(&self, db: &Connection, root: &Path, predicates: &[String]) -> ApiResult<()> {
        match self {
            Self::Fact(statement) => statement.validate(db, root, predicates),
            Self::Alias(alias) => {
                let valid_name = |name: &str| match alias.alias_kind {
                    AliasKind::Entity => entity(name) && name.trim() == name,
                    AliasKind::Setting => scope_tag(name),
                };
                let surface = graph::entity_key(&alias.surface);
                let canonical = graph::entity_key(&alias.canonical);
                if !valid_name(&alias.surface)
                    || !valid_name(&alias.canonical)
                    || surface == canonical
                    || !alias.primary.valid()
                {
                    return Err(REVIEW_INVALID);
                }
                let quotes = checked_quotes(db, root, std::slice::from_ref(&alias.primary))?;
                // The reviewer judges the equivalence. The server proves the
                // exact source includes both names and is current.
                if !graph::mentions(&quotes[0], &surface)
                    || !graph::mentions(&quotes[0], &canonical)
                {
                    return Err(REVIEW_INVALID);
                }
                Ok(())
            }
        }
    }
}

pub(in super::super) struct Fact {
    pub(in super::super) subject: String,
    pub(in super::super) predicate: String,
    pub(in super::super) kind: FactKind,
    pub(in super::super) target: Option<String>,
    pub(in super::super) scope: Vec<String>,
    pub(in super::super) evidence: Value,
}

impl Statement {
    fn validate(&self, db: &Connection, root: &Path, predicates: &[String]) -> ApiResult<()> {
        let names = std::iter::once(&self.subject)
            .chain(self.target.as_ref())
            .chain(&self.scope);
        if !entity(&self.subject)
            || self.subject.trim() != self.subject
            || !predicates.contains(&self.predicate)
            || self.scope.is_empty()
            || !tags(&self.scope)
            || self
                .scope
                .iter()
                .map(|tag| graph::entity_key(tag))
                .collect::<BTreeSet<_>>()
                .len()
                != self.scope.len()
            || !(1..=5).contains(&self.anchors.len())
            || self.anchors.iter().any(|anchor| !anchor.valid())
            || self
                .anchors
                .iter()
                .map(|a| (&a.chunk_id, a.start, a.end))
                .collect::<BTreeSet<_>>()
                .len()
                != self.anchors.len()
        {
            return Err(REVIEW_INVALID);
        }
        match (self.kind, &self.target) {
            (FactKind::ExplicitNegative, Some(target))
                if entity(target)
                    && target.trim() == target
                    && graph::entity_key(target) != graph::entity_key(&self.subject) => {}
            (FactKind::DocumentedAbsence, None) => {}
            _ => return Err(REVIEW_INVALID),
        }
        let quotes = checked_quotes(db, root, &self.anchors)?;
        if names.map(|name| graph::entity_key(name)).any(|name| {
            name.is_empty() || !quotes.iter().any(|quote| graph::mentions(quote, &name))
        }) {
            return Err(REVIEW_INVALID);
        }
        Ok(())
    }

    fn evidence(&self, db: &Connection, decision_id: &str) -> Result<Option<Value>> {
        let mut spans = Vec::with_capacity(self.anchors.len());
        for anchor in &self.anchors {
            let Some((_, quote)) = anchor.load(db)? else {
                return Ok(None);
            };
            spans.push(json!({
                "chunk_id": anchor.chunk_id, "revision": anchor.revision,
                "start": anchor.start, "end": anchor.end, "quote": quote,
            }));
        }
        let Some((primary, context)) = spans.split_first() else {
            return Ok(None);
        };
        Ok(Some(
            json!({"decision_id": decision_id, "primary": primary, "context": context}),
        ))
    }
}

impl Service {
    pub(in super::super) fn graph_review_fact(
        &self,
        principal: &Principal,
        request: FactReview,
    ) -> ApiResult<Value> {
        let request_hash = files::digest(&serde_json::to_vec(&request).map_err(unavailable)?);
        let (collection, fingerprint, version, key, statement, withdrawal) = match request.0 {
            ReviewAction::Add {
                collection,
                fingerprint,
                policy_version,
                idempotency_key,
                subject,
                predicate,
                kind,
                target,
                scope,
                primary,
                context,
            } => (
                collection,
                fingerprint,
                policy_version,
                idempotency_key,
                Some(ReviewEntry::Fact(Statement {
                    subject,
                    predicate,
                    kind,
                    target,
                    scope,
                    anchors: std::iter::once(primary).chain(context).collect(),
                })),
                None,
            ),
            ReviewAction::AddAlias {
                collection,
                fingerprint,
                policy_version,
                idempotency_key,
                alias_kind,
                surface,
                canonical,
                primary,
            } => (
                collection,
                fingerprint,
                policy_version,
                idempotency_key,
                Some(ReviewEntry::Alias(AliasStatement {
                    alias_kind,
                    surface,
                    canonical,
                    primary,
                })),
                None,
            ),
            ReviewAction::Withdraw {
                collection,
                fingerprint,
                policy_version,
                idempotency_key,
                fact_id,
            } => (
                collection,
                fingerprint,
                policy_version,
                idempotency_key,
                None,
                Some(fact_id),
            ),
        };
        if key.is_empty()
            || key.len() > 128
            || key.chars().any(char::is_control)
            || withdrawal.as_ref().is_some_and(|id| !hex_id(id, 64))
        {
            return Err(REVIEW_INVALID);
        }
        let _write = self.try_write()?;
        self.ready()?;
        let (policy, digest) = self.policy()?;
        let grant = policy.graph_reviewer(principal, &collection)?;
        let rules = grant.graph_publication.as_ref().ok_or(REVIEW_STALE)?;
        if version != policy.version
            || fingerprint != self.publication_fingerprint(rules).map_err(unavailable)?
        {
            return Err(REVIEW_STALE);
        }
        if let Some(statement) = &statement {
            let db = self
                .open_collection(&collection, false)
                .map_err(unavailable)?;
            if !self
                .graph_publication_current(&db, grant)
                .map_err(unavailable)?
            {
                return Err(REVIEW_STALE);
            }
            let root = self.sources[&collection].folder().ok_or(REVIEW_STALE)?;
            let extractor = self.extractor.as_ref().ok_or(REVIEW_STALE)?;
            statement.validate(&db, root, self.endpoints[extractor].predicates())?;
        }
        self.confirm_graph_reviewer(principal, &collection, &digest)?;
        let mut meta = self.meta.lock().map_err(unavailable)?;
        let tx = meta
            .transaction_with_behavior(Immediate)
            .map_err(unavailable)?;
        let still_current = tx
            .query_row(
                "SELECT policy_version=?1 AND policy_digest=?2 FROM service_meta WHERE id=1",
                params![version, digest],
                |row| row.get::<_, bool>(0),
            )
            .map_err(unavailable)?;
        if !still_current {
            return Err(ACCESS_CHANGED);
        }
        let prior: Option<(String, String)> = tx
            .query_row(
                "SELECT request_hash,fact_id FROM shared_relationship_fact_reviews
             WHERE actor=?1 AND idempotency_key=?2",
                params![principal.subject, key],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(unavailable)?;
        let action = if statement.is_some() {
            "add"
        } else {
            "withdraw"
        };
        let response = |id: &str| {
            json!({
                "schema_version": 1, "status": if action == "add" { "recorded" } else { "withdrawn" },
                "fact_id": id, "collection": collection, "fingerprint": fingerprint, "policy_version": version,
            })
        };
        if let Some((hash, id)) = prior {
            return if hash == request_hash {
                Ok(response(&id))
            } else {
                Err(CONFLICT)
            };
        }
        let id = if let Some(statement) = statement {
            let subject = statement.lookup_key();
            let count: i64 = tx
                .query_row(
                    "SELECT count(*) FROM shared_relationship_facts
                 WHERE collection=?1 AND fingerprint=?2 AND subject_key=?3 AND withdrawn=0",
                    params![collection, fingerprint, subject],
                    |row| row.get(0),
                )
                .map_err(unavailable)?;
            if count >= FACT_LIMIT as i64 {
                return Err(CONFLICT);
            }
            let id = files::digest(
                &serde_json::to_vec(&(&principal.subject, &key, &request_hash))
                    .map_err(unavailable)?,
            );
            tx.execute(
                "INSERT INTO shared_relationship_facts(id,collection,fingerprint,subject_key,statement)
                 VALUES (?1,?2,?3,?4,?5)",
                params![id, collection, fingerprint, subject, serde_json::to_string(&statement).map_err(unavailable)?],
            ).map_err(unavailable)?;
            id
        } else {
            let id = withdrawal.ok_or(REVIEW_INVALID)?;
            if tx
                .execute(
                    "UPDATE shared_relationship_facts SET withdrawn=1
                 WHERE id=?1 AND collection=?2 AND withdrawn=0",
                    params![id, collection],
                )
                .map_err(unavailable)?
                != 1
            {
                return Err(REVIEW_STALE);
            }
            id
        };
        tx.execute(
            "INSERT INTO shared_relationship_fact_reviews
             (actor,idempotency_key,request_hash,fact_id,action,policy_version)
             VALUES (?1,?2,?3,?4,?5,?6)",
            params![principal.subject, key, request_hash, id, action, version],
        )
        .map_err(unavailable)?;
        principal.ensure_current()?;
        tx.commit().map_err(unavailable)?;
        Ok(response(&id))
    }

    /// Read only after admitting the collection under the caller's current policy.
    pub(in super::super) fn current_facts(
        &self,
        db: &Connection,
        collection: &str,
        grant: &Grant,
        subject: &str,
    ) -> Result<Option<Vec<Fact>>> {
        let Some(rows) =
            self.current_review_entries(db, collection, grant, &graph::entity_key(subject))?
        else {
            return Ok(None);
        };
        let mut facts = Vec::new();
        for (id, raw) in rows {
            let statement: Statement = serde_json::from_str(&raw)?;
            let Some(evidence) = statement.evidence(db, &id)? else {
                continue;
            };
            facts.push(Fact {
                subject: statement.subject,
                predicate: statement.predicate,
                kind: statement.kind,
                target: statement.target,
                scope: statement.scope,
                evidence,
            });
        }
        Ok(Some(facts))
    }

    pub(super) fn current_aliases(
        &self,
        db: &Connection,
        collection: &str,
        grant: &Grant,
        kind: AliasKind,
        surface: &str,
    ) -> Result<Option<Vec<Alias>>> {
        let Some(rows) =
            self.current_review_entries(db, collection, grant, &kind.lookup_key(surface))?
        else {
            return Ok(None);
        };
        let mut aliases = Vec::new();
        for (id, raw) in rows {
            let alias: AliasStatement = serde_json::from_str(&raw)?;
            if let Some((_, quote)) = alias.primary.load(db)? {
                aliases.push(Alias {
                    canonical: alias.canonical.clone(),
                    evidence: json!({
                        "decision_id":id, "requested":alias.surface, "canonical":alias.canonical,
                        "kind":alias.alias_kind,
                        "chunk_id":alias.primary.chunk_id, "revision":alias.primary.revision,
                        "start":alias.primary.start, "end":alias.primary.end, "quote":quote
                    }),
                });
            }
        }
        Ok(Some(aliases))
    }

    fn current_review_entries(
        &self,
        db: &Connection,
        collection: &str,
        grant: &Grant,
        key: &str,
    ) -> Result<Option<Vec<(String, String)>>> {
        let Some(rules) = &grant.graph_publication else {
            return Ok(Some(Vec::new()));
        };
        if !self.graph_publication_current(db, grant)? {
            return Ok(Some(Vec::new()));
        }
        let fingerprint = self.publication_fingerprint(rules)?;
        let rows = {
            let meta = self
                .meta
                .lock()
                .map_err(|_| anyhow::anyhow!("Review state unavailable."))?;
            let mut query = meta.prepare(
                "SELECT id,statement FROM shared_relationship_facts
                 WHERE collection=?1 AND fingerprint=?2 AND subject_key=?3 AND withdrawn=0
                 ORDER BY id LIMIT ?4",
            )?;
            query
                .query_map(
                    params![collection, fingerprint, key, FACT_LIMIT as i64 + 1],
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
                )?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        Ok((rows.len() <= FACT_LIMIT).then_some(rows))
    }
}
