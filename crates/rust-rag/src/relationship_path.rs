//! Bounded directed path selection over reader-visible, source-verified relations.

use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Eq, PartialEq)]
pub(crate) struct Citation<'a> {
    pub(crate) id: &'a str,
    pub(crate) revision: &'a str,
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) struct Relation<'a> {
    pub(crate) collection: &'a str,
    pub(crate) subject: &'a str,
    pub(crate) predicate: &'a str,
    pub(crate) object: &'a str,
    pub(crate) citation: Citation<'a>,
    pub(crate) binding_citations: &'a [Citation<'a>],
    pub(crate) required_scope: &'a [&'a str],
}

pub(crate) struct Lookup<'a> {
    pub(crate) root: &'a str,
    pub(crate) terminal_predicates: &'a [&'a str],
    pub(crate) min_steps: usize,
    pub(crate) max_steps: usize,
    pub(crate) scope: &'a BTreeSet<&'a str>,
    pub(crate) citation_limit: usize,
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) enum Outcome<'a> {
    Value(Vec<&'a Relation<'a>>),
    NoSupportedValue,
    Ambiguous,
    OutsideRequestedScope,
    LimitReached,
}

fn citation_count(path: &[&Relation<'_>]) -> usize {
    path.iter()
        .flat_map(|edge| std::iter::once(&edge.citation).chain(edge.binding_citations))
        .map(|citation| citation.id)
        .collect::<BTreeSet<_>>()
        .len()
}

/// The admission check must use the reader's current signed policy and exact source span.
pub(crate) fn resolve<'a>(
    relations: &'a [Relation<'a>],
    request: &Lookup<'_>,
    readable: impl Fn(&str, &Citation<'_>) -> bool,
) -> Outcome<'a> {
    if request.root.is_empty()
        || request.terminal_predicates.is_empty()
        || request.min_steps == 0
        || request.min_steps > request.max_steps
        || request.max_steps > 3
        || request.citation_limit == 0
    {
        return Outcome::NoSupportedValue;
    }
    let mut adjacency = BTreeMap::<&str, Vec<&Relation<'_>>>::new();
    for edge in relations {
        if readable(edge.collection, &edge.citation)
            && edge
                .binding_citations
                .iter()
                .all(|citation| readable(edge.collection, citation))
        {
            adjacency.entry(edge.subject).or_default().push(edge);
        }
    }
    let mut pending = vec![(request.root, Vec::new(), false)];
    let mut selected = None;
    let mut outside_scope = false;
    let mut examined = 0;
    while let Some((node, path, wrong_scope)) = pending.pop() {
        let Some(edges) = adjacency.get(node) else {
            continue;
        };
        for edge in edges {
            examined += 1;
            if examined > 1_000 {
                return Outcome::LimitReached;
            }
            if edge.object == request.root
                || edge.object == node
                || path
                    .iter()
                    .any(|earlier: &&Relation<'_>| earlier.object == edge.object)
                || path
                    .first()
                    .is_some_and(|first| first.collection != edge.collection)
            {
                continue;
            }
            let mut next = path.clone();
            next.push(*edge);
            if citation_count(&next) > request.citation_limit {
                continue;
            }
            let wrong_scope = wrong_scope
                || edge
                    .required_scope
                    .iter()
                    .any(|value| !request.scope.contains(value));
            if next.len() >= request.min_steps
                && request.terminal_predicates.contains(&edge.predicate)
            {
                let scope_proven = request.scope.iter().all(|value| {
                    next.iter().any(|step| step.required_scope.contains(value))
                });
                if wrong_scope || !scope_proven {
                    outside_scope = true;
                } else if selected.as_ref().is_some_and(|previous| previous != &next) {
                    return Outcome::Ambiguous;
                } else {
                    selected = Some(next.clone());
                }
            }
            if next.len() < request.max_steps {
                pending.push((edge.object, next, wrong_scope));
            }
        }
    }
    match selected {
        Some(path) => Outcome::Value(path),
        None if outside_scope => Outcome::OutsideRequestedScope,
        None => Outcome::NoSupportedValue,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn citation(id: &str) -> Citation<'_> {
        Citation {
            id,
            revision: "current",
        }
    }

    #[test]
    fn scope_identity_budget_and_ambiguity_are_checked_on_the_visible_graph() {
        let first = Relation {
            collection: "operations",
            subject: "entry",
            predicate: "uses",
            object: "router",
            citation: citation("entry-citation"),
            binding_citations: &[],
            required_scope: &[],
        };
        let second = Relation {
            collection: "operations",
            subject: "router",
            predicate: "owned_by",
            object: "crew",
            citation: citation("owner-citation"),
            binding_citations: &[citation("identity-citation")],
            required_scope: &["live", "north"],
        };
        let hidden = Relation {
            collection: "other",
            subject: "router",
            predicate: "owned_by",
            object: "other crew",
            citation: citation("other-citation"),
            binding_citations: &[],
            required_scope: &[],
        };
        let competing = Relation {
            collection: "operations",
            subject: "router",
            predicate: "owned_by",
            object: "other crew",
            citation: citation("other-citation"),
            binding_citations: &[citation("unavailable-citation")],
            required_scope: &["live", "north"],
        };
        let stale = Relation {
            collection: "operations",
            subject: "router",
            predicate: "owned_by",
            object: "previous crew",
            citation: Citation {
                id: "previous-citation",
                revision: "stale",
            },
            binding_citations: &[],
            required_scope: &[],
        };
        let edges = [first, second, hidden, competing, stale];
        let scope = BTreeSet::from(["live", "north"]);
        let wrong_scope = BTreeSet::from(["live"]);
        let request = Lookup {
            root: "entry",
            terminal_predicates: &["owned_by"],
            min_steps: 2,
            max_steps: 2,
            scope: &scope,
            citation_limit: 3,
        };
        let reader = |collection: &str, citation: &Citation<'_>| {
            collection == "operations"
                && citation.id != "unavailable-citation"
                && citation.revision == "current"
        };
        assert_eq!(
            resolve(&edges[..2], &request, reader),
            Outcome::Value(vec![&edges[0], &edges[1]])
        );
        assert_eq!(
            resolve(&edges, &request, reader),
            resolve(&edges[..2], &request, reader)
        );
        assert_eq!(resolve(&edges, &request, |_, _| true), Outcome::Ambiguous);
        let scoped = Lookup {
            scope: &wrong_scope,
            ..request
        };
        assert_eq!(
            resolve(&edges, &scoped, reader),
            Outcome::OutsideRequestedScope
        );
        assert_eq!(
            resolve(&edges, &scoped, |_, citation| reader("other", citation)),
            Outcome::NoSupportedValue
        );
        assert_eq!(
            resolve(
                &edges[..2],
                &Lookup {
                    citation_limit: 2,
                    ..request
                },
                reader
            ),
            Outcome::NoSupportedValue
        );
        assert_eq!(
            resolve(&edges[..1], &request, reader),
            Outcome::NoSupportedValue
        );
    }
}
