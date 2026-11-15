//! Signed and cited relationship lookups with explicit applicability conditions.

#[path = "relationship_fact.rs"]
pub(super) mod fact;
#[path = "relationship_path.rs"]
mod path;
#[path = "relationship_question.rs"]
mod question;

use super::*;
use path::{Citation, Lookup, Outcome, Relation, resolve_matching, visit_paths};

const MAX_RELATIONS: usize = 1_000;
pub(super) const INVALID_LOOKUP: ApiError =
    ApiError::new(400, "RELATIONSHIP_REQUEST", "Invalid relationship lookup.");

#[derive(Clone, Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RelationshipRequest {
    root: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    root_alias: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    root_alias_version: Option<String>,
    terminal_predicates: Vec<String>,
    path_predicates: Option<Vec<String>>,
    min_steps: usize,
    max_steps: usize,
    #[serde(default)]
    scope: Vec<String>,
    target: Option<String>,
    via: Option<Vec<Option<String>>>,
    limit: Option<usize>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct GuidedRelationship {
    request: RelationshipRequest,
    confirmed: Option<RelationshipRequest>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum LookupMode {
    Direct,
    NaturalAlias,
    Guided,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Anchor {
    chunk_id: String,
    revision: String,
    quote: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ReviewedRelationship {
    subject: String,
    predicate: String,
    object: String,
    source: Anchor,
    context: Vec<Anchor>,
    #[serde(default)]
    scope: Vec<String>,
}

fn entity(value: &str) -> bool {
    !value.trim().is_empty() && value.chars().count() <= 160 && !value.chars().any(char::is_control)
}

fn scope_tag(value: &str) -> bool {
    !value.is_empty()
        && value.chars().count() <= 96
        && !value.chars().any(char::is_control)
        && value.trim() == value
}

fn tags(values: &[String]) -> bool {
    values.len() <= 8
        && values.iter().all(|value| scope_tag(value))
        && values.iter().collect::<BTreeSet<_>>().len() == values.len()
}

impl Anchor {
    fn valid(&self) -> bool {
        hex_id(&self.chunk_id, 64)
            && hex_id(&self.revision, 64)
            && !self.quote.trim().is_empty()
            && self.quote.chars().count() <= 512
    }

    fn load(&self, db: &Connection) -> Result<Option<String>> {
        let result = db
            .query_row(
                "SELECT s.source_hash,p.text FROM chunks c JOIN sources s USING(source_id)
             JOIN passages p ON p.rowid=c.row_id WHERE c.chunk_id=?1",
                [&self.chunk_id],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()?;
        Ok(result
            .filter(|(revision, text)| revision == &self.revision && text.contains(&self.quote))
            .map(|(revision, _)| revision))
    }
}

#[derive(Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Applicability {
    scope: Vec<String>,
    context: Vec<ReviewAnchor>,
}

#[derive(Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
struct ReviewAnchor {
    chunk_id: String,
    revision: String,
    start: usize,
    end: usize,
    quote_sha256: String,
}

impl ReviewAnchor {
    fn valid(&self) -> bool {
        hex_id(&self.chunk_id, 64)
            && hex_id(&self.revision, 64)
            && hex_id(&self.quote_sha256, 64)
            && (1..=512).contains(&self.end.saturating_sub(self.start))
    }

    fn load(&self, db: &Connection) -> Result<Option<(String, String)>> {
        let row = db
            .query_row(
                "SELECT s.path,p.text,c.start FROM chunks c JOIN sources s USING(source_id)
             JOIN passages p ON p.rowid=c.row_id WHERE c.chunk_id=?1 AND s.source_hash=?2",
                params![self.chunk_id, self.revision],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, i64>(2)?,
                    ))
                },
            )
            .optional()?;
        let Some((path, text, chunk_start)) = row else {
            return Ok(None);
        };
        let Ok(chunk_start) = usize::try_from(chunk_start) else {
            return Ok(None);
        };
        let Some((start, end)) = self
            .start
            .checked_sub(chunk_start)
            .zip(self.end.checked_sub(chunk_start))
        else {
            return Ok(None);
        };
        if !(1..=512).contains(&end.saturating_sub(start)) {
            return Ok(None);
        }
        let quote: String = text.chars().skip(start).take(end - start).collect();
        Ok((quote.chars().count() == end - start
            && files::digest(quote.as_bytes()) == self.quote_sha256)
            .then_some((path, quote)))
    }
}

impl Applicability {
    pub(super) fn validate(&self, db: &Connection, root: &Path) -> ApiResult<()> {
        if self.scope.is_empty()
            || !tags(&self.scope)
            || !(1..=4).contains(&self.context.len())
            || self.context.iter().any(|anchor| !anchor.valid())
        {
            return Err(REVIEW_INVALID);
        }
        let quotes = checked_quotes(db, root, &self.context)?;
        if self.scope.iter().any(|tag| {
            let tag = graph::entity_key(tag);
            !quotes.iter().any(|quote| graph::mentions(quote, &tag))
        }) {
            return Err(REVIEW_INVALID);
        }
        Ok(())
    }
}

fn checked_quotes(
    db: &Connection,
    root: &Path,
    anchors: &[ReviewAnchor],
) -> ApiResult<Vec<String>> {
    let root = files::display_path(root).map_err(|_| REVIEW_STALE)?;
    let mut remaining = files::MAX_BATCH_BYTES;
    anchors
        .iter()
        .map(|anchor| {
            let (path, quote) = anchor.load(db).map_err(unavailable)?.ok_or(REVIEW_STALE)?;
            let path = Path::new(&path);
            if !path.strip_prefix(root).is_ok_and(|relative| {
                !relative.as_os_str().is_empty()
                    && relative
                        .components()
                        .all(|component| matches!(component, std::path::Component::Normal(_)))
            }) {
                return Err(REVIEW_STALE);
            }
            let stamp = files::file_stamp(path).map_err(|_| REVIEW_STALE)?;
            let raw =
                files::read_revision(path, &mut remaining, &stamp).map_err(|_| REVIEW_STALE)?;
            if files::digest(&raw) != anchor.revision {
                return Err(REVIEW_STALE);
            }
            Ok(graph::entity_key(&quote))
        })
        .collect()
}

impl ReviewedRelationship {
    pub(super) fn valid(&self, predicates: &[String]) -> bool {
        entity(&self.subject)
            && entity(&self.object)
            && graph::entity_key(&self.subject) != graph::entity_key(&self.object)
            && predicates.contains(&self.predicate)
            && self.source.valid()
            && self.context.len() <= 4
            && self.context.iter().all(Anchor::valid)
            && tags(&self.scope)
            && self.source.quote.contains(&self.subject)
            && self.source.quote.contains(&self.object)
            && (self.scope.is_empty() || !self.context.is_empty())
    }
}

struct Edge {
    collection: String,
    subject: String,
    predicate: String,
    object: String,
    quote: String,
    citation: String,
    revision: String,
    context: Vec<(String, String)>,
    scope: Vec<String>,
    review: Option<Value>,
}

fn scoped_edges(
    db: &Connection,
    collection: &str,
    subject: &str,
    maximum: usize,
) -> Result<Vec<Edge>> {
    if !graph::scoped_review_installed(db)? {
        return Ok(Vec::new());
    }
    let mut statement = db.prepare(
        "SELECT a.subject,a.predicate,a.object,a.quote,c.chunk_id,s.source_hash,p.text,
                c.start,a.quote_start,a.quote_end,a.applicability,d.proposal_id,d.actor
         FROM graph_scoped_decisions a JOIN graph_shared_decisions d USING(proposal_id)
         JOIN graph_chunks g ON g.chunk_row=d.chunk_row AND g.fingerprint=d.fingerprint
         JOIN graph_config f ON f.id=1 AND f.fingerprint=d.fingerprint
         JOIN chunks c ON c.row_id=d.chunk_row JOIN sources s ON s.source_id=c.source_id
         JOIN passages p ON p.rowid=c.row_id
         WHERE a.subject_key=?1 AND a.superseded=0 AND d.approve=1 AND d.revision=s.source_hash
         ORDER BY a.proposal_id LIMIT ?2",
    )?;
    let mut rows = statement.query(params![graph::entity_key(subject), maximum as i64])?;
    let mut edges = Vec::new();
    while let Some(row) = rows.next()? {
        let actual = row.get::<_, String>(0)?;
        let Some(mut edge) = published_edge(row, collection, &actual)? else {
            continue;
        };
        let applicability: Applicability = serde_json::from_str(&row.get::<_, String>(10)?)?;
        let mut anchors = Vec::new();
        for anchor in &applicability.context {
            let Some((_, quote)) = anchor.load(db)? else {
                break;
            };
            edge.context
                .push((anchor.chunk_id.clone(), anchor.revision.clone()));
            anchors.push(
                json!({"chunk_id":anchor.chunk_id,"revision":anchor.revision,
                "start":anchor.start,"end":anchor.end,"quote":quote}),
            );
        }
        if anchors.len() != applicability.context.len() {
            continue;
        }
        edge.scope = applicability.scope;
        edge.review = Some(json!({"decision_id":row.get::<_,String>(11)?,
            "reviewer":row.get::<_,String>(12)?,"start":row.get::<_,i64>(8)?,
            "end":row.get::<_,i64>(9)?,"context":anchors}));
        edges.push(edge);
    }
    Ok(edges)
}

pub(super) fn review_scoped_links(
    db: &Connection,
    fingerprint: &str,
    limit: usize,
    after: Option<&str>,
) -> Result<Value> {
    let mut records = Vec::new();
    if graph::scoped_review_installed(db)? {
        let mut sql = db.prepare(
            "SELECT a.proposal_id,a.subject,a.predicate,a.object,a.quote,c.chunk_id,s.source_hash,
                    a.quote_start,a.quote_end,a.applicability,s.path,d.policy_version
             FROM graph_scoped_decisions a JOIN graph_shared_decisions d USING(proposal_id)
             JOIN chunks c ON c.row_id=d.chunk_row JOIN sources s USING(source_id)
             WHERE a.superseded=0 AND d.fingerprint=?1 AND d.revision=s.source_hash AND a.proposal_id>?2
             ORDER BY a.proposal_id LIMIT ?3",
        )?;
        let mut rows = sql.query(params![fingerprint, after.unwrap_or(""), limit as i64 + 1])?;
        while let Some(row) = rows.next()? {
            let applicability: Applicability = serde_json::from_str(&row.get::<_, String>(9)?)?;
            let mut context = Vec::new();
            let mut stale = false;
            for anchor in &applicability.context {
                let current = anchor.load(db)?;
                stale |= current.is_none();
                context.push(json!({"anchor":anchor,"quote":current.map(|(_, quote)|quote)}));
            }
            records.push(json!({
                "proposal_id":row.get::<_,String>(0)?,"subject":row.get::<_,String>(1)?,
                "predicate":row.get::<_,String>(2)?,"object":row.get::<_,String>(3)?,
                "quote":row.get::<_,String>(4)?,"chunk_id":row.get::<_,String>(5)?,
                "revision":row.get::<_,String>(6)?,"start":row.get::<_,i64>(7)?,"end":row.get::<_,i64>(8)?,
                "fingerprint":fingerprint,"scope":applicability.scope,"context":context,
                "path":row.get::<_,String>(10)?,"policy_version":row.get::<_,i64>(11)?,
                "reason":if stale {"stale_scoped_context"} else {"current_scoped_review"}
            }));
        }
    }
    let has_more = records.len() > limit;
    records.truncate(limit);
    let next = has_more
        .then(|| records.last().map(|row| row["proposal_id"].clone()))
        .flatten();
    Ok(json!({"schema_version":1,"proposals":records,"has_more":has_more,"next_cursor":next}))
}

fn read_edges(
    db: &Connection,
    collection: &str,
    grant: &Grant,
    request: &RelationshipRequest,
    edges: &mut Vec<Edge>,
    remaining: &mut usize,
) -> Result<bool> {
    let mut sql = db.prepare(
        "SELECT a.subject,a.predicate,a.object,a.quote,c.chunk_id,s.source_hash,
                p.text,c.start,a.quote_start,a.quote_end
         FROM graph_assertions a JOIN graph_edge_provenance gp ON gp.edge_id=a.edge_id
         JOIN chunks c ON c.row_id=a.chunk_row JOIN sources s USING(source_id)
         JOIN passages p ON p.rowid=c.row_id WHERE a.subject_key=?1
         ORDER BY a.edge_id LIMIT ?2",
    )?;
    let mut frontier = BTreeSet::from([request.root.clone()]);
    let mut visited = BTreeSet::new();
    for _ in 0..request.max_steps {
        let start = edges.len();
        for subject in frontier {
            if !visited.insert(subject.clone()) {
                continue;
            }
            let mut rows =
                sql.query(params![graph::entity_key(&subject), *remaining as i64 + 1])?;
            while let Some(row) = rows.next()? {
                let Some(left) = remaining.checked_sub(1) else {
                    return Ok(false);
                };
                *remaining = left;
                if let Some(edge) = published_edge(row, collection, &subject)? {
                    edges.push(edge);
                }
            }
            for reviewed in grant
                .reviewed_relationships
                .iter()
                .filter(|edge| edge.subject == subject)
            {
                let Some(edge) = reviewed_edge(db, collection, reviewed)? else {
                    continue;
                };
                let Some(left) = remaining.checked_sub(1) else {
                    return Ok(false);
                };
                *remaining = left;
                edges.push(edge);
            }
            for edge in scoped_edges(db, collection, &subject, *remaining + 1)? {
                if edge.subject != subject {
                    continue;
                }
                let Some(left) = remaining.checked_sub(1) else {
                    return Ok(false);
                };
                *remaining = left;
                edges.push(edge);
            }
        }
        frontier = edges[start..]
            .iter()
            .map(|edge| edge.object.clone())
            .collect();
        if frontier.is_empty() {
            break;
        }
    }
    Ok(true)
}

fn published_edge(
    row: &rusqlite::Row<'_>,
    collection: &str,
    subject: &str,
) -> Result<Option<Edge>> {
    if row.get::<_, String>(0)? != subject {
        return Ok(None);
    }
    let predicate: String = row.get(1)?;
    let object: String = row.get(2)?;
    let quote: String = row.get(3)?;
    let text: String = row.get(6)?;
    let indexed_start: i64 = row.get(7)?;
    let quote_start: i64 = row.get(8)?;
    let quote_end: i64 = row.get(9)?;
    let Some(offset) = quote_start
        .checked_sub(indexed_start)
        .and_then(|n| usize::try_from(n).ok())
    else {
        return Ok(None);
    };
    if quote_end.checked_sub(quote_start) != Some(quote.chars().count() as i64)
        || !text
            .char_indices()
            .nth(offset)
            .is_some_and(|(i, _)| text[i..].starts_with(&quote))
        || graph::contradicts_direction(
            &json!({"text":text,"start":indexed_start}),
            &json!({"subject":subject,"predicate":predicate,"object":object,
                "quote":quote,"start":quote_start}),
        )
    {
        return Ok(None);
    }
    Ok(Some(Edge {
        collection: collection.into(),
        subject: subject.into(),
        predicate,
        object,
        quote,
        citation: row.get(4)?,
        revision: row.get(5)?,
        context: Vec::new(),
        scope: Vec::new(),
        review: None,
    }))
}

fn reviewed_edge(
    db: &Connection,
    collection: &str,
    reviewed: &ReviewedRelationship,
) -> Result<Option<Edge>> {
    let Some(revision) = reviewed.source.load(db)? else {
        return Ok(None);
    };
    let mut context = Vec::new();
    for anchor in &reviewed.context {
        let Some(revision) = anchor.load(db)? else {
            return Ok(None);
        };
        context.push((anchor.chunk_id.clone(), revision));
    }
    Ok(Some(Edge {
        collection: collection.into(),
        subject: reviewed.subject.clone(),
        predicate: reviewed.predicate.clone(),
        object: reviewed.object.clone(),
        quote: reviewed.source.quote.clone(),
        citation: reviewed.source.chunk_id.clone(),
        revision,
        context,
        scope: reviewed.scope.clone(),
        review: None,
    }))
}

fn formatted(
    edges: &[Edge],
    facts: &[fact::Fact],
    request: &mut RelationshipRequest,
    natural_scope: Option<Option<&str>>,
    mode: LookupMode,
    matches_step: impl Fn(usize, &Relation<'_>) -> bool + Copy,
) -> Value {
    let eligible: Vec<_> = facts
        .iter()
        .filter(|fact| {
            question::normalize(&fact.subject) == question::normalize(&request.root)
                && request.min_steps == 1
                && request.max_steps == 1
                && request.terminal_predicates.contains(&fact.predicate)
                && match &fact.target {
                    Some(target) => request.target.as_ref().is_some_and(|requested| {
                        question::normalize(requested) == question::normalize(target)
                    }),
                    None => request.target.is_none(),
                }
        })
        .collect();
    let target = request.target.as_deref();
    let matches_step = |depth, step: &Relation<'_>| {
        matches_step(depth, step)
            && request
                .via
                .as_ref()
                .and_then(|via| via.get(depth))
                .and_then(Option::as_deref)
                .is_none_or(|name| question::normalize(name) == question::normalize(step.object))
            && request.path_predicates.as_ref().is_none_or(|path| {
                path.get(depth)
                    .is_some_and(|predicate| predicate == step.predicate)
            })
            && (depth + 1 != request.max_steps
                || target.is_none_or(|target| {
                    question::normalize(step.object) == question::normalize(target)
                }))
    };
    let scopes: Vec<Vec<&str>> = edges
        .iter()
        .map(|edge| edge.scope.iter().map(String::as_str).collect())
        .collect();
    let citations: Vec<Vec<Citation<'_>>> = edges
        .iter()
        .map(|edge| {
            edge.context
                .iter()
                .map(|(id, revision)| Citation { id, revision })
                .collect()
        })
        .collect();
    let relations: Vec<_> = edges
        .iter()
        .enumerate()
        .map(|(index, edge)| Relation {
            collection: &edge.collection,
            subject: &edge.subject,
            predicate: &edge.predicate,
            object: &edge.object,
            citation: Citation {
                id: &edge.citation,
                revision: &edge.revision,
            },
            binding_citations: &citations[index],
            required_scope: &scopes[index],
        })
        .collect();
    let predicates: Vec<_> = request
        .terminal_predicates
        .iter()
        .map(String::as_str)
        .collect();
    let empty_scope = BTreeSet::new();
    let limit = request.limit.unwrap_or(12);
    if let Some(explicit) = natural_scope {
        let mut settings: BTreeSet<_> = eligible
            .iter()
            .filter(|fact| !fact.scope.is_empty())
            .map(|fact| {
                fact.scope
                    .iter()
                    .cloned()
                    .collect::<BTreeSet<_>>()
                    .into_iter()
                    .collect::<Vec<_>>()
            })
            .collect();
        if !visit_paths(
            &relations,
            &Lookup {
                root: &request.root,
                terminal_predicates: &predicates,
                min_steps: request.min_steps,
                max_steps: request.max_steps,
                scope: &empty_scope,
                citation_limit: usize::MAX,
            },
            |_, _| true,
            matches_step,
            |path| {
                let tags: BTreeSet<_> = path
                    .iter()
                    .flat_map(|edge| edge.required_scope.iter().copied())
                    .collect();
                if !tags.is_empty() {
                    settings.insert(tags.into_iter().map(str::to_owned).collect::<Vec<_>>());
                }
                true
            },
        ) {
            return json!({"schema_version":1,"status":"limit_reached","value":null,"steps":[]});
        }
        if let Some(explicit) = explicit {
            let mut matching = settings
                .iter()
                .filter(|setting| question::normalize(&setting.join(" + ")) == explicit);
            if let Some(setting) = matching.next()
                && matching.next().is_none()
            {
                request.scope.clone_from(setting);
            } else if settings.is_empty() {
                return question::fallback();
            }
        }
        if request.scope.is_empty() && !settings.is_empty() {
            let choices = settings.iter().map(|tags| tags.join(" + ")).collect();
            return question::clarification("setting", &choices);
        }
    }
    let scope = request.scope.iter().map(String::as_str).collect();
    let matching: Vec<_> = eligible
        .into_iter()
        .filter(|fact| {
            fact.scope
                .iter()
                .map(String::as_str)
                .collect::<BTreeSet<_>>()
                == scope
        })
        .collect();
    if let Some(fact) = matching.first() {
        let mut positive = false;
        let complete = visit_paths(
            &relations,
            &Lookup {
                root: &request.root,
                terminal_predicates: &predicates,
                min_steps: request.min_steps,
                max_steps: request.max_steps,
                scope: &scope,
                citation_limit: usize::MAX,
            },
            |_, _| true,
            matches_step,
            |path| {
                let applied: BTreeSet<_> = path
                    .iter()
                    .flat_map(|edge| edge.required_scope.iter().copied())
                    .collect();
                // An unconditional positive also contradicts a scoped assertion.
                positive = applied.is_subset(&scope);
                !positive
            },
        );
        let status = if !complete {
            "limit_reached"
        } else if positive || matching.len() != 1 {
            "conflict"
        } else {
            match fact.kind {
                fact::FactKind::ExplicitNegative => "explicit_negative",
                fact::FactKind::DocumentedAbsence => "documented_absence",
            }
        };
        let mut response = json!({"schema_version":1,"status":status,"value":null,"steps":[]});
        if matches!(status, "explicit_negative" | "documented_absence") {
            let context = fact.evidence["context"].as_array().into_iter().flatten();
            let citations = std::iter::once(&fact.evidence["primary"])
                .chain(context)
                .filter_map(|anchor| anchor["chunk_id"].as_str())
                .collect::<BTreeSet<_>>();
            if citations.is_empty() || citations.len() > limit {
                response["status"] = json!("limit_reached");
            } else {
                response["fact"] = fact.evidence.clone();
            }
        }
        return response;
    }
    match resolve_matching(
        &relations,
        &Lookup {
            root: &request.root,
            terminal_predicates: &predicates,
            min_steps: request.min_steps,
            max_steps: request.max_steps,
            scope: &scope,
            citation_limit: limit,
        },
        |_, _| true,
        matches_step,
        mode != LookupMode::Direct,
    ) {
        Outcome::Value(path) => {
            let steps: Vec<_> = path.iter().map(|step| {
                let source = edges.iter().find(|edge| edge.collection == step.collection && edge.citation == step.citation.id
                    && edge.subject == step.subject && edge.predicate == step.predicate && edge.object == step.object
                    && edge.scope.iter().map(String::as_str).eq(step.required_scope.iter().copied())
                    && edge.context.iter().map(|(id, revision)| (id.as_str(), revision.as_str()))
                        .eq(step.binding_citations.iter().map(|cite| (cite.id, cite.revision))))
                    .expect("Resolved step has a validated source");
                let mut evidence = json!({"subject":step.subject,"predicate":step.predicate,"object":step.object,
                    "scope":step.required_scope,"quote":source.quote,"citation":{"chunk_id":step.citation.id,"revision":step.citation.revision},
                    "bindings":step.binding_citations.iter().map(|binding| json!({"chunk_id":binding.id,"revision":binding.revision})).collect::<Vec<_>>()});
                if let Some(review) = &source.review {
                    evidence["review"] = review.clone();
                }
                evidence
            }).collect();
            json!({"schema_version":1,"status":"value","value":path.last().map(|step| step.object),"steps":steps})
        }
        outcome => {
            let status = match outcome {
                Outcome::NoSupportedValue => "no_supported_value",
                Outcome::Ambiguous => "ambiguous",
                Outcome::OutsideRequestedScope => "outside_requested_scope",
                Outcome::LimitReached => "limit_reached",
                Outcome::Value(_) => unreachable!(),
            };
            json!({"schema_version":1,"status":status,"value":null,"steps":[]})
        }
    }
}

struct LookupAliases {
    request: RelationshipRequest,
    evidence: Vec<Value>,
}

impl LookupAliases {
    fn attach(&self, result: &mut Value) {
        if !has_answer(result) {
            return;
        }
        // Keep one passage budget across the relationship and its name bindings.
        let mut ids = BTreeSet::new();
        for step in result["steps"].as_array().into_iter().flatten() {
            ids.extend(step["citation"]["chunk_id"].as_str());
            ids.extend(
                step["bindings"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|a| a["chunk_id"].as_str()),
            );
        }
        ids.extend(result["fact"]["primary"]["chunk_id"].as_str());
        ids.extend(
            result["fact"]["context"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|a| a["chunk_id"].as_str()),
        );
        ids.extend(self.evidence.iter().filter_map(|a| a["chunk_id"].as_str()));
        if ids.len() > self.request.limit.unwrap_or(12) {
            *result = without_value("limit_reached");
        } else {
            result["name_bindings"] = json!(self.evidence);
        }
    }
}

fn has_answer(result: &Value) -> bool {
    matches!(
        result["status"].as_str(),
        Some("value" | "documented_absence" | "explicit_negative")
    )
}

fn attach_interpretation(result: &mut Value, request: &RelationshipRequest) {
    if !has_answer(result) {
        return;
    }
    let mut resolved = request.clone();
    if let Some(steps) = result["steps"].as_array()
        && steps.len() > 1
    {
        resolved.via = Some(
            steps[..steps.len() - 1]
                .iter()
                .map(|step| step["object"].as_str().map(str::to_owned))
                .collect(),
        );
    }
    result["interpretation"] = json!(resolved);
    result["root_identity"] = match &request.root_alias {
        Some(alias) => json!({"kind":"reviewed_alias","alias":alias,"canonical":request.root}),
        None => json!({"kind":"caller_supplied","canonical":request.root}),
    };
    result["identity_unverified"] = json!(request.root_alias.is_none());
}

fn without_value(status: &str) -> Value {
    json!({"schema_version":1,"status":status,"value":null,"steps":[]})
}

enum AliasLookup {
    Unchanged,
    NoMatch,
    Bound(LookupAliases),
    Ambiguous,
    Limit,
}

impl Service {
    fn literal_competitor(
        &self,
        policy: &Policy,
        scope: &[String],
        request: &RelationshipRequest,
        root_aliased: bool,
        remaining: &mut usize,
        matches_step: impl Fn(usize, &Relation<'_>) -> bool + Copy,
    ) -> ApiResult<Option<&'static str>> {
        let [setting] = request.scope.as_slice() else {
            return Err(INVALID_LOOKUP);
        };
        let setting = question::normalize(setting);
        for name in scope {
            let grant = &policy.collections[name];
            if grant.graph_publication.is_none() {
                continue;
            }
            let db = self.open_collection(name, false).map_err(unavailable)?;
            if !self
                .graph_publication_current(&db, grant)
                .map_err(unavailable)?
            {
                continue;
            }
            let mut roots: BTreeSet<_> =
                question::roots(&db, grant, &question::normalize(&request.root))
                    .map_err(unavailable)?
                    .into_iter()
                    .collect();
            let facts = if request.min_steps == 1 {
                let Some(current) = self
                    .current_facts(&db, name, grant, &request.root)
                    .map_err(unavailable)?
                else {
                    return Ok(Some("limit_reached"));
                };
                roots.extend(current.iter().map(|fact| fact.subject.clone()));
                current
            } else {
                Vec::new()
            };
            if root_aliased && !roots.is_empty() {
                return Ok(Some("ambiguous"));
            }
            for root in roots {
                let mut exact = request.clone();
                exact.root = root;
                exact.scope.clear();
                let mut edges = Vec::new();
                if !read_edges(&db, name, grant, &exact, &mut edges, remaining)
                    .map_err(unavailable)?
                {
                    return Ok(Some("limit_reached"));
                }
                let literal = formatted(
                    &edges,
                    &facts,
                    &mut exact,
                    Some(Some(&setting)),
                    LookupMode::NaturalAlias,
                    matches_step,
                );
                match literal["status"].as_str() {
                    Some("limit_reached") => return Ok(Some("limit_reached")),
                    Some(
                        "value" | "explicit_negative" | "documented_absence" | "conflict"
                        | "ambiguous",
                    ) => return Ok(Some("ambiguous")),
                    _ => {}
                }
            }
        }
        Ok(None)
    }

    fn bind_lookup_aliases(
        &self,
        db: &Connection,
        collection: &str,
        grant: &Grant,
        request: &RelationshipRequest,
    ) -> Result<AliasLookup> {
        let mut result = LookupAliases {
            request: request.clone(),
            evidence: Vec::new(),
        };
        for (field, kind, selected_surface) in std::iter::once((
            &mut result.request.root,
            fact::AliasKind::Entity,
            request.root_alias.as_deref(),
        ))
        .chain(
            result
                .request
                .scope
                .iter_mut()
                .map(|setting| (setting, fact::AliasKind::Setting, None)),
        )
        .chain(
            result
                .request
                .target
                .iter_mut()
                .map(|target| (target, fact::AliasKind::Entity, None)),
        ) {
            let surface = selected_surface.unwrap_or(field);
            let Some(aliases) = self.current_aliases(db, collection, grant, kind, surface)? else {
                return Ok(AliasLookup::Limit);
            };
            if selected_surface.is_some() {
                // A caller can select a canonical referent. The reviewed
                // alias still has to support it in this reader's collection.
                let Some(alias) = aliases.into_iter().find(|alias| alias.canonical == *field)
                else {
                    return Ok(AliasLookup::NoMatch);
                };
                result.evidence.push(alias.evidence);
                continue;
            }
            let mut canonical = aliases.into_iter();
            if let Some(alias) = canonical.next() {
                if canonical.any(|other| other.canonical != alias.canonical) {
                    return Ok(AliasLookup::Ambiguous);
                }
                *field = alias.canonical;
                result.evidence.push(alias.evidence);
            }
        }
        if result.evidence.is_empty() {
            return Ok(AliasLookup::Unchanged);
        }
        if !tags(&result.request.scope) {
            return Ok(AliasLookup::Ambiguous);
        }
        Ok(AliasLookup::Bound(result))
    }

    pub(super) fn relationship(
        &self,
        principal: &Principal,
        request: RelationshipRequest,
    ) -> ApiResult<Value> {
        self.relationship_matching(principal, request, LookupMode::Direct, |_, _| true)
    }

    pub(super) fn guided_relationship(
        &self,
        principal: &Principal,
        guidance: GuidedRelationship,
    ) -> ApiResult<Value> {
        if guidance.request.path_predicates.is_none()
            || guidance.request.terminal_predicates.len() != 1
        {
            return Err(INVALID_LOOKUP);
        }
        let result =
            self.relationship_matching(principal, guidance.request, LookupMode::Guided, |_, _| {
                true
            })?;
        if let Some(confirmed) = guidance.confirmed {
            if !has_answer(&result) || result["interpretation"] != json!(confirmed) {
                return Err(ApiError::new(
                    409,
                    "RELATIONSHIP_CHANGED",
                    "Preview the current relationship before confirming.",
                ));
            }
            return Ok(result);
        }
        if has_answer(&result) {
            return Ok(json!({"schema_version":1,"status":"confirm_relationship",
                    "interpretation":result["interpretation"], "root_identity":result["root_identity"],
                    "identity_unverified":result["identity_unverified"]}));
        }
        Ok(without_value(
            result["status"].as_str().unwrap_or("no_supported_value"),
        ))
    }

    fn relationship_matching(
        &self,
        principal: &Principal,
        mut request: RelationshipRequest,
        mode: LookupMode,
        matches_step: impl Fn(usize, &Relation<'_>) -> bool + Copy,
    ) -> ApiResult<Value> {
        let _read = self.read_during_sync()?;
        self.ready()?;
        let (policy, digest) = self.policy()?;
        let scope = policy.scope(principal)?;
        let only_aliases = mode == LookupMode::NaturalAlias;
        if !entity(&request.root)
            || request.root_alias_version.is_some()
            || request.root_alias.as_deref().is_some_and(|alias| {
                mode != LookupMode::Guided
                    || !entity(alias)
                    || question::normalize(alias) == question::normalize(&request.root)
            })
            || request.min_steps == 0
            || request.min_steps > request.max_steps
            || request.max_steps > 3
            || !tags(&request.scope)
            || request
                .target
                .as_deref()
                .is_some_and(|target| !entity(target) || request.min_steps != request.max_steps)
            || request.terminal_predicates.is_empty()
            || !tags(&request.terminal_predicates)
            || request.path_predicates.as_ref().is_some_and(|path| {
                request.min_steps != request.max_steps
                    || path.len() != request.max_steps
                    || path.iter().any(|predicate| !scope_tag(predicate))
                    || path
                        .last()
                        .is_none_or(|last| !request.terminal_predicates.contains(last))
            })
            || request.via.as_ref().is_some_and(|via| {
                request.min_steps != request.max_steps
                    || via.len().saturating_add(1) != request.max_steps
                    || via.iter().flatten().any(|name| !entity(name))
            })
            || !(1..=20).contains(&request.limit.unwrap_or(12))
        {
            return Err(INVALID_LOOKUP);
        }
        let mut edges = Vec::new();
        let mut facts = Vec::new();
        let mut remaining = MAX_RELATIONS;
        let mut failure = None;
        let mut alternative = None;
        let mut outside_alias_scope = false;
        let mut alias_roots = BTreeSet::new();
        for name in &scope {
            let grant = &policy.collections[name];
            if grant.graph_publication.is_none() {
                continue;
            }
            let db = self.open_collection(name, false).map_err(unavailable)?;
            if self
                .graph_publication_current(&db, grant)
                .map_err(unavailable)?
            {
                let mut binding = match self
                    .bind_lookup_aliases(&db, name, grant, &request)
                    .map_err(unavailable)?
                {
                    AliasLookup::Unchanged => None,
                    AliasLookup::NoMatch => continue,
                    AliasLookup::Bound(binding) => Some(binding),
                    AliasLookup::Ambiguous => {
                        failure = Some("ambiguous");
                        break;
                    }
                    AliasLookup::Limit => {
                        failure = Some("limit_reached");
                        break;
                    }
                };
                if only_aliases {
                    let Some(bound) = &mut binding else {
                        continue;
                    };
                    let mut names: BTreeSet<_> =
                        question::roots(&db, grant, &question::normalize(&bound.request.root))
                            .map_err(unavailable)?
                            .into_iter()
                            .collect();
                    if request.min_steps == 1 {
                        let Some(current) = self
                            .current_facts(&db, name, grant, &bound.request.root)
                            .map_err(unavailable)?
                        else {
                            failure = Some("limit_reached");
                            break;
                        };
                        names.extend(current.into_iter().map(|fact| fact.subject));
                    }
                    if names.len() > 1 {
                        failure = Some("ambiguous");
                        break;
                    }
                    if let Some(name) = names.pop_first() {
                        bound.request.root = name;
                    }
                }
                let mut local_edges = Vec::new();
                let query = binding.as_ref().map_or(&request, |bound| &bound.request);
                if !read_edges(&db, name, grant, query, &mut local_edges, &mut remaining)
                    .map_err(unavailable)?
                {
                    failure = Some("limit_reached");
                    break;
                }
                let mut local_facts = Vec::new();
                if query.min_steps == 1 {
                    let Some(current) = self
                        .current_facts(&db, name, grant, &query.root)
                        .map_err(unavailable)?
                    else {
                        failure = Some("limit_reached");
                        break;
                    };
                    local_facts = current;
                }
                if let Some(bound) = &mut binding {
                    let substituted_root = question::normalize(&bound.request.root)
                        != question::normalize(&request.root);
                    if substituted_root {
                        alias_roots.insert(bound.request.root.clone());
                        if alias_roots.len() > 1 {
                            failure = Some("ambiguous");
                            break;
                        }
                    }
                    let same_name = |left: &str, right: &str| {
                        graph::entity_key(left) == graph::entity_key(right)
                    };
                    // Ambiguous even if only one interpretation can currently
                    // finish a path. An alias is not priority over a literal.
                    let setting_collision = request.scope.iter().zip(&bound.request.scope).any(
                        |(literal, canonical)| {
                            literal != canonical
                                && (local_edges.iter().any(|edge| {
                                    edge.scope.iter().any(|tag| same_name(literal, tag))
                                }) || local_facts.iter().any(|fact| {
                                    fact.scope.iter().any(|tag| same_name(literal, tag))
                                }))
                        },
                    );
                    let target_collision = request
                        .target
                        .as_deref()
                        .zip(bound.request.target.as_deref())
                        .is_some_and(|(literal, canonical)| {
                            literal != canonical
                                && (local_edges
                                    .iter()
                                    .any(|edge| same_name(literal, &edge.object))
                                    || local_facts.iter().any(|fact| {
                                        fact.target
                                            .as_deref()
                                            .is_some_and(|tag| same_name(literal, tag))
                                    }))
                        });
                    if !only_aliases && (setting_collision || target_collision) {
                        failure = Some("ambiguous");
                        break;
                    }
                    let canonical_setting =
                        only_aliases.then(|| question::normalize(&bound.request.scope.remove(0)));
                    let mut result = formatted(
                        &local_edges,
                        &local_facts,
                        &mut bound.request,
                        canonical_setting.as_deref().map(Some),
                        mode,
                        matches_step,
                    );
                    bound.attach(&mut result);
                    if mode == LookupMode::Guided {
                        if substituted_root {
                            bound.request.root_alias = Some(request.root.clone());
                        }
                        if bound.request.root_alias.is_some() {
                            // Root proof is pushed before setting and target bindings.
                            let proof = &bound.evidence[0];
                            let identity = json!([proof["decision_id"], proof["revision"]]);
                            bound.request.root_alias_version =
                                Some(files::digest(identity.to_string().as_bytes()));
                        }
                        attach_interpretation(&mut result, &bound.request);
                    }
                    if only_aliases && result["name_bindings"].is_array() {
                        result["interpretation"] = json!({
                            "root":bound.request.root,
                            "predicates":bound.request.path_predicates,
                            "scope":bound.request.scope,
                            "target":bound.request.target,
                        });
                    }
                    // An exact name and a different matching alias in one
                    // collection must not silently pick a path.
                    if substituted_root && !only_aliases {
                        let mut current = Vec::new();
                        if !read_edges(&db, name, grant, &request, &mut current, &mut remaining)
                            .map_err(unavailable)?
                        {
                            failure = Some("limit_reached");
                            break;
                        }
                        if current.iter().any(|edge| edge.subject == request.root)
                            || self
                                .current_facts(&db, name, grant, &request.root)
                                .map_err(unavailable)?
                                .is_none_or(|facts| !facts.is_empty())
                        {
                            failure = Some("ambiguous");
                            break;
                        }
                    }
                    if matches!(
                        result["status"].as_str(),
                        Some("ambiguous" | "limit_reached" | "conflict")
                    ) {
                        failure = Some(match result["status"].as_str() {
                            Some("ambiguous") => "ambiguous",
                            Some("conflict") => "conflict",
                            _ => "limit_reached",
                        });
                        break;
                    }
                    outside_alias_scope |= result["status"] == "outside_requested_scope";
                    if has_answer(&result) {
                        if alternative.is_some() {
                            failure = Some("ambiguous");
                            break;
                        }
                        alternative = Some(result);
                    }
                } else {
                    edges.extend(local_edges);
                    facts.extend(local_facts);
                }
            }
        }
        let literal_root = !alias_roots.is_empty()
            && (edges.iter().any(|edge| edge.subject == request.root)
                || facts.iter().any(|fact| {
                    question::normalize(&fact.subject) == question::normalize(&request.root)
                }));
        let result = if let Some(status) = failure {
            without_value(status)
        } else if literal_root {
            without_value("ambiguous")
        } else if only_aliases {
            if let Some(bound) = alternative
                && bound["name_bindings"].is_array()
            {
                match self.literal_competitor(
                    &policy,
                    &scope,
                    &request,
                    !alias_roots.is_empty(),
                    &mut remaining,
                    matches_step,
                )? {
                    Some(status) => without_value(status),
                    None => bound,
                }
            } else {
                question::fallback()
            }
        } else {
            let mut exact = formatted(&edges, &facts, &mut request, None, mode, matches_step);
            if mode == LookupMode::Guided {
                attach_interpretation(&mut exact, &request);
            }
            match alternative {
                Some(bound) if exact["status"] == "no_supported_value" => bound,
                Some(_) => without_value("ambiguous"),
                None if outside_alias_scope && exact["status"] == "no_supported_value" => {
                    without_value("outside_requested_scope")
                }
                None => exact,
            }
        };
        principal.ensure_current()?;
        if self.policy()?.1 != digest {
            return Err(ACCESS_CHANGED);
        }
        self.ready()?;
        Ok(result)
    }
}
