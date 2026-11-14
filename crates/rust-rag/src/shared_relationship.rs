//! Signed and cited relationship lookups with explicit applicability conditions.

#[path = "relationship_path.rs"]
mod path;

use super::*;
use path::{Citation, Lookup, Outcome, Relation, resolve};

const MAX_RELATIONS: usize = 1_000;
const INVALID_LOOKUP: ApiError =
    ApiError::new(400, "RELATIONSHIP_REQUEST", "Invalid relationship lookup.");

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RelationshipRequest {
    root: String,
    terminal_predicates: Vec<String>,
    min_steps: usize,
    max_steps: usize,
    #[serde(default)]
    scope: Vec<String>,
    limit: Option<usize>,
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
            for reviewed in grant.reviewed_relationships.iter().filter(|edge| edge.subject == subject) {
                let Some(edge) = reviewed_edge(db, collection, reviewed)? else {
                    continue;
                };
                let Some(left) = remaining.checked_sub(1) else {
                    return Ok(false);
                };
                *remaining = left;
                edges.push(edge);
            }
        }
        frontier = edges[start..].iter().map(|edge| edge.object.clone()).collect();
        if frontier.is_empty() {
            break;
        }
    }
    Ok(true)
}

fn published_edge(row: &rusqlite::Row<'_>, collection: &str, subject: &str) -> Result<Option<Edge>> {
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
    }))
}

fn reviewed_edge(db: &Connection, collection: &str, reviewed: &ReviewedRelationship) -> Result<Option<Edge>> {
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
    }))
}

fn formatted(edges: &[Edge], request: &RelationshipRequest) -> Value {
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
    let scope = request.scope.iter().map(String::as_str).collect();
    let limit = request.limit.unwrap_or(12);
    match resolve(
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
    ) {
        Outcome::Value(path) => {
            let steps: Vec<_> = path.iter().map(|step| {
                let source = edges.iter().find(|edge| edge.collection == step.collection && edge.citation == step.citation.id
                    && edge.subject == step.subject && edge.predicate == step.predicate && edge.object == step.object)
                    .expect("Resolved step has a validated source");
                json!({"subject":step.subject,"predicate":step.predicate,"object":step.object,
                    "scope":step.required_scope,"quote":source.quote,"citation":{"chunk_id":step.citation.id,"revision":step.citation.revision},
                    "bindings":step.binding_citations.iter().map(|binding| json!({"chunk_id":binding.id,"revision":binding.revision})).collect::<Vec<_>>()})
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

impl Service {
    pub(super) fn relationship(
        &self,
        principal: &Principal,
        request: RelationshipRequest,
    ) -> ApiResult<Value> {
        let _read = self.read_during_sync()?;
        self.ready()?;
        let (policy, digest) = self.policy()?;
        let scope = policy.scope(principal)?;
        if !entity(&request.root)
            || request.min_steps == 0
            || request.min_steps > request.max_steps
            || request.max_steps > 3
            || !tags(&request.scope)
            || request.terminal_predicates.is_empty()
            || !tags(&request.terminal_predicates)
            || !(1..=20).contains(&request.limit.unwrap_or(12))
        {
            return Err(INVALID_LOOKUP);
        }
        let mut edges = Vec::new();
        let mut remaining = MAX_RELATIONS;
        let mut complete = true;
        for name in scope {
            let grant = &policy.collections[&name];
            if grant.graph_publication.is_none() {
                continue;
            }
            let db = self.open_collection(&name, false).map_err(unavailable)?;
            if self
                .graph_publication_current(&db, grant)
                .map_err(unavailable)?
                && !read_edges(&db, &name, grant, &request, &mut edges, &mut remaining)
                    .map_err(unavailable)?
            {
                complete = false;
                break;
            }
        }
        let result = if !complete {
            json!({"schema_version":1,"status":"limit_reached","value":null,"steps":[]})
        } else {
            formatted(&edges, &request)
        };
        principal.ensure_current()?;
        if self.policy()?.1 != digest {
            return Err(ACCESS_CHANGED);
        }
        self.ready()?;
        Ok(result)
    }
}
