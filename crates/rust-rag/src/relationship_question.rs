//! Reader-scoped mapping from supported questions to the signed relationship resolver.

use super::*;
#[derive(Clone)]
struct Intent {
    roots: Vec<String>,
    predicates: Vec<&'static str>,
    scope: Option<String>,
    intermediate: Vec<String>,
    target: Option<String>,
}

pub(super) use crate::graph::entity_key as normalize;

fn interpret(question: &str) -> Option<Intent> {
    let normalized = normalize(question);
    let question = normalized.strip_suffix('?').unwrap_or(&normalized).trim();
    let (question, scope) = question
        .rsplit_once(" during ")
        .map_or((question, None), |(query, scope)| {
            (query, Some(scope.to_owned()))
        });
    if let Some(probe) = question.strip_prefix("does ") {
        let (root, predicate, target) = [
            (" require ", "requires"),
            (" depend on ", "depends_on"),
            (" use ", "uses"),
            (" belong to ", "part_of"),
        ]
        .into_iter()
        .find_map(|(cue, predicate)| {
            let (root, target) = probe.split_once(cue)?;
            entity(target).then_some((root, predicate, target.to_owned()))
        })?;
        return Some(Intent {
            roots: root_names(root)?,
            predicates: vec![predicate],
            scope,
            intermediate: Vec::new(),
            target: Some(target),
        });
    }
    let (mut root, terminal) = if let Some(target) = question.strip_prefix("who owns ") {
        (target, "owned_by")
    } else {
        let target = question.strip_prefix("what does ")?;
        [
            (" require", "requires"),
            (" depend on", "depends_on"),
            (" use", "uses"),
            (" belong to", "part_of"),
        ]
        .into_iter()
        .find_map(|(suffix, predicate)| target.strip_suffix(suffix).map(|root| (root, predicate)))?
    };
    let mut predicates = vec![terminal];
    let mut intermediate = Vec::new();
    while let Some((index, cue, relation)) = [
        (" used by ", "uses"),
        (" required by ", "requires"),
        (" depended on by ", "depends_on"),
        (" containing ", "part_of"),
    ]
    .into_iter()
    .filter_map(|(cue, relation)| root.find(cue).map(|index| (index, cue, relation)))
    .min_by_key(|(index, _, _)| *index)
    {
        let part = &root[..index];
        let part = part.strip_prefix("the ").unwrap_or(part);
        if !entity(part) || predicates.len() == 3 {
            return None;
        }
        intermediate.push(part.to_owned());
        predicates.push(relation);
        root = &root[index + cue.len()..];
    }
    predicates.reverse();
    intermediate.reverse();
    Some(Intent {
        roots: root_names(root)?,
        predicates,
        scope,
        intermediate,
        target: None,
    })
}

fn root_names(root: &str) -> Option<Vec<String>> {
    let names: Vec<_> = root
        .split(" or ")
        .map(|root| root.strip_prefix("the ").unwrap_or(root).trim().to_owned())
        .collect();
    (names.len() <= 4 && names.iter().all(|name| entity(name))).then_some(names)
}

pub(super) fn roots(db: &Connection, grant: &Grant, requested: &str) -> Result<Vec<String>> {
    let mut statement = db.prepare(
        "SELECT a.subject,a.predicate,a.object,a.quote,c.chunk_id,s.source_hash,
                p.text,c.start,a.quote_start,a.quote_end FROM graph_assertions a
         JOIN graph_edge_provenance gp ON gp.edge_id=a.edge_id
         JOIN chunks c ON c.row_id=a.chunk_row JOIN sources s USING(source_id)
         JOIN passages p ON p.rowid=c.row_id WHERE a.subject_key=?1
         ORDER BY a.edge_id LIMIT ?2",
    )?;
    let mut rows = statement.query(params![requested, MAX_RELATIONS as i64])?;
    let mut found = BTreeSet::new();
    while let Some(row) = rows.next()? {
        let subject = row.get::<_, String>(0)?;
        if published_edge(row, "", &subject)?.is_some() {
            found.insert(subject);
        }
        if found.len() == 9 {
            break;
        }
    }
    for relation in &grant.reviewed_relationships {
        if normalize(&relation.subject) == requested && reviewed_edge(db, "", relation)?.is_some() {
            found.insert(relation.subject.clone());
        }
    }
    for edge in scoped_edges(db, "", requested, MAX_RELATIONS + 1)? {
        found.insert(edge.subject);
    }
    Ok(found.into_iter().collect())
}

pub(super) fn clarification(kind: &str, choices: &BTreeSet<String>) -> Value {
    json!({"schema_version":1,"status":"clarification","reason":kind,
        "choices":choices.iter().take(8).collect::<Vec<_>>()})
}

pub(super) fn fallback() -> Value {
    json!({"schema_version":1,"status":"ordinary_search"})
}

impl Service {
    pub(in super::super) fn relationship_question(
        &self,
        principal: &Principal,
        mut request: Query,
    ) -> ApiResult<Value> {
        let limit = request.limit.unwrap_or(12);
        if !(1..=20).contains(&limit) || store::validate_query(&request.question, limit).is_err() {
            return Err(INVALID_LOOKUP);
        }
        let reply = request.clarification.take();
        if reply.as_deref().is_some_and(|reply| !scope_tag(reply)) {
            return Err(INVALID_LOOKUP);
        }
        let plan = match interpret(&request.question) {
            None => fallback(),
            Some(intent) => {
                let aliases = if reply.is_none() {
                    self.plan_relationship_aliases(principal, &intent, limit)?
                } else {
                    fallback()
                };
                if aliases["status"] == "ordinary_search" {
                    self.plan_relationship_question(principal, intent, limit, reply.as_deref())?
                } else {
                    aliases
                }
            }
        };
        if plan["status"] == "ordinary_search" {
            let evidence = self.query(principal, request)?;
            return Ok(json!({"plan":plan,"result":evidence}));
        }
        Ok(plan)
    }

    fn plan_relationship_aliases(
        &self,
        principal: &Principal,
        intent: &Intent,
        limit: usize,
    ) -> ApiResult<Value> {
        // Route only a fully stated setting on this path. The normal planner
        // still handles choices when the user omits the setting.
        let ([root], Some(scope)) = (intent.roots.as_slice(), intent.scope.as_ref()) else {
            return Ok(fallback());
        };
        if !scope_tag(scope) {
            return Ok(fallback());
        }
        let predicates = intent
            .predicates
            .iter()
            .map(|item| (*item).to_owned())
            .collect();
        let request = RelationshipRequest {
            root: root.clone(),
            root_alias: None,
            root_alias_version: None,
            min_steps: intent.predicates.len(),
            max_steps: intent.predicates.len(),
            terminal_predicates: vec![intent.predicates.last().unwrap().to_string()],
            path_predicates: Some(predicates),
            scope: vec![scope.clone()],
            target: intent.target.clone(),
            via: None,
            limit: Some(limit),
        };
        let result = self.relationship_matching(
            principal,
            request,
            LookupMode::NaturalAlias,
            |depth, step| matches_intermediate(intent, depth, step),
        )?;
        match result["status"].as_str() {
            Some("ambiguous") => Ok(clarification("relationship", &BTreeSet::new())),
            Some(
                "value" | "explicit_negative" | "documented_absence" | "conflict" | "limit_reached",
            ) => Ok(result),
            _ => Ok(fallback()),
        }
    }

    fn plan_relationship_question(
        &self,
        principal: &Principal,
        intent: Intent,
        limit: usize,
        reply: Option<&str>,
    ) -> ApiResult<Value> {
        let _read = self.read_during_sync()?;
        self.ready()?;
        let (policy, digest) = self.policy()?;
        let scope = policy.scope(principal)?;
        let mut collections = Vec::new();
        for name in &scope {
            let grant = &policy.collections[name];
            if grant.graph_publication.is_some() {
                let db = self.open_collection(name, false).map_err(unavailable)?;
                if self
                    .graph_publication_current(&db, grant)
                    .map_err(unavailable)?
                {
                    collections.push((name, grant, db));
                }
            }
        }
        let mut candidates = BTreeSet::new();
        let mut facts = Vec::new();
        let mut facts_complete = true;
        for (name, grant, db) in &collections {
            for root in &intent.roots {
                candidates.extend(roots(db, grant, root).map_err(unavailable)?);
                if intent.predicates.len() == 1 {
                    let Some(current) = self
                        .current_facts(db, name, grant, root)
                        .map_err(unavailable)?
                    else {
                        facts_complete = false;
                        continue;
                    };
                    for fact in current {
                        if fact.predicate == intent.predicates[0] {
                            candidates.insert(fact.subject.clone());
                            facts.push(fact);
                        }
                    }
                }
            }
        }
        let chosen = if candidates.len() > 1 || intent.roots.len() > 1 {
            reply.and_then(|reply| {
                let mut matching = candidates
                    .iter()
                    .filter(|name| normalize(name) == normalize(reply));
                let chosen = matching.next()?;
                matching.next().is_none().then_some(chosen.clone())
            })
        } else {
            candidates.iter().next().cloned()
        };
        let result = if !facts_complete {
            json!({"schema_version":1,"status":"limit_reached"})
        } else if candidates.is_empty() {
            fallback()
        } else if let Some(root) = chosen {
            let request = RelationshipRequest {
                root_alias: None,
                root_alias_version: None,
                root,
                min_steps: intent.predicates.len(),
                max_steps: intent.predicates.len(),
                terminal_predicates: vec![
                    intent.predicates.last().expect("one predicate").to_string(),
                ],
                path_predicates: None,
                scope: Vec::new(),
                target: intent.target.clone(),
                via: None,
                limit: Some(limit),
            };
            let mut edges = Vec::new();
            let mut remaining = MAX_RELATIONS;
            let mut complete = true;
            for (name, grant, db) in &collections {
                if !read_edges(db, name, grant, &request, &mut edges, &mut remaining)
                    .map_err(unavailable)?
                {
                    complete = false;
                    break;
                }
            }
            if complete {
                let mut result = planned_value(&edges, &facts, request.clone(), &intent);
                if result["reason"] == "setting"
                    && intent.roots.len() == 1
                    && candidates.len() == 1
                    && let Some(reply) = reply
                    && result["choices"].as_array().is_some_and(|choices| {
                        choices.iter().any(|choice| {
                            choice
                                .as_str()
                                .is_some_and(|setting| normalize(setting) == normalize(reply))
                        })
                    })
                {
                    let explicit = Intent {
                        scope: Some(normalize(reply)),
                        ..intent
                    };
                    result = planned_value(&edges, &facts, request, &explicit);
                }
                result
            } else {
                json!({"schema_version":1,"status":"limit_reached"})
            }
        } else {
            clarification("entity", &candidates)
        };
        principal.ensure_current()?;
        if self.policy()?.1 != digest {
            return Err(ACCESS_CHANGED);
        }
        self.ready()?;
        Ok(result)
    }
}

fn planned_value(
    edges: &[Edge],
    facts: &[fact::Fact],
    mut request: RelationshipRequest,
    intent: &Intent,
) -> Value {
    let mut result = formatted(
        edges,
        facts,
        &mut request,
        Some(intent.scope.as_deref()),
        LookupMode::NaturalAlias,
        |depth, step| {
            if intent.predicates.get(depth).copied() != Some(step.predicate) {
                return false;
            }
            matches_intermediate(intent, depth, step)
        },
    );
    if result["status"] == "ambiguous" {
        return clarification("relationship", &BTreeSet::new());
    }
    if !matches!(
        result["status"].as_str(),
        Some("ordinary_search" | "clarification")
    ) {
        result["interpretation"] = json!({"root":request.root,"predicates":intent.predicates,"scope":request.scope,"target":request.target});
    }
    result
}

fn matches_intermediate(intent: &Intent, depth: usize, step: &Relation<'_>) -> bool {
    intent.intermediate.get(depth).is_none_or(|part| {
        let named = normalize(step.object);
        named == *part
            || (part.split_whitespace().count() == 1
                && named.split_whitespace().any(|word| word == part.as_str()))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn setting_choices_come_from_complete_matching_paths() {
        let edge = |subject: &str, predicate: &str, object: &str, scope: &[&str]| Edge {
            collection: "dispatch".into(),
            subject: subject.into(),
            predicate: predicate.into(),
            object: object.into(),
            quote: format!("{subject} {predicate} {object}"),
            citation: format!("cite-{object}"),
            revision: "current".into(),
            context: Vec::new(),
            scope: scope.iter().map(|tag| (*tag).into()).collect(),
            review: None,
        };
        let edges = [
            edge("Quarry Desk", "requires", "Transfer Pump", &[]),
            edge("Transfer Pump", "uses", "Aster Valve", &[]),
            edge(
                "Aster Valve",
                "owned_by",
                "Pine Service Crew",
                &["winter standby"],
            ),
            edge(
                "Transfer Pump",
                "uses",
                "Pressure Gauge",
                &["unrelated setting"],
            ),
            edge(
                "Transfer Pump",
                "depends_on",
                "Spare Valve",
                &["other setting"],
            ),
        ];
        let ask = |question| {
            let intent = interpret(question).unwrap();
            let request = RelationshipRequest {
                root_alias: None,
                root_alias_version: None,
                root: "Quarry Desk".into(),
                terminal_predicates: vec!["owned_by".into()],
                path_predicates: None,
                min_steps: intent.predicates.len(),
                max_steps: intent.predicates.len(),
                scope: Vec::new(),
                target: None,
                via: None,
                limit: Some(12),
            };
            planned_value(&edges, &[], request, &intent)
        };
        let clarification = ask("Who owns the valve used by the pump required by Quarry Desk?");
        assert_eq!(clarification["status"], "clarification");
        assert_eq!(clarification["choices"], json!(["winter standby"]));
        let answer = ask(
            "Who owns the valve used by the pump required by Quarry Desk during winter standby?",
        );
        assert_eq!(answer["value"], "Pine Service Crew");
        assert_eq!(answer["steps"].as_array().unwrap().len(), 3);
    }
}
