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
#[cfg(test)]
fn resolve<'a>(
    relations: &'a [Relation<'a>],
    request: &Lookup<'_>,
    readable: impl Fn(&str, &Citation<'_>) -> bool,
) -> Outcome<'a> {
    resolve_matching(relations, request, readable, |_, _| true, false)
}

pub(crate) fn resolve_matching<'a>(
    relations: &'a [Relation<'a>],
    request: &Lookup<'_>,
    readable: impl Fn(&str, &Citation<'_>) -> bool,
    matches_step: impl Fn(usize, &Relation<'_>) -> bool,
    check_all_paths: bool,
) -> Outcome<'a> {
    let traversal = Lookup {
        citation_limit: if check_all_paths {
            usize::MAX
        } else {
            request.citation_limit
        },
        ..*request
    };
    let mut selected = None;
    let mut outside_scope = false;
    let mut ambiguous = false;
    let complete = visit_paths(relations, &traversal, readable, matches_step, |path| {
        let tags: BTreeSet<_> = path
            .iter()
            .flat_map(|edge| edge.required_scope.iter().copied())
            .collect();
        if tags != *request.scope {
            outside_scope = true;
        } else if selected.as_ref().is_some_and(|previous| previous != path) {
            ambiguous = true;
            return false;
        } else {
            selected = Some(path.to_vec());
        }
        true
    });
    if ambiguous {
        return Outcome::Ambiguous;
    }
    if !complete {
        return Outcome::LimitReached;
    }
    match selected {
        Some(path) if citation_count(&path) > request.citation_limit => Outcome::LimitReached,
        Some(path) => Outcome::Value(path),
        None if outside_scope => Outcome::OutsideRequestedScope,
        None => Outcome::NoSupportedValue,
    }
}

pub(crate) fn visit_paths<'a>(
    relations: &'a [Relation<'a>],
    request: &Lookup<'_>,
    readable: impl Fn(&str, &Citation<'_>) -> bool,
    matches_step: impl Fn(usize, &Relation<'_>) -> bool,
    mut visit: impl FnMut(&[&'a Relation<'a>]) -> bool,
) -> bool {
    if request.root.is_empty()
        || request.terminal_predicates.is_empty()
        || request.min_steps == 0
        || request.min_steps > request.max_steps
        || request.max_steps > 3
        || request.citation_limit == 0
    {
        return true;
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
    let mut pending = vec![(request.root, Vec::new())];
    let mut examined = 0;
    while let Some((node, path)) = pending.pop() {
        let Some(edges) = adjacency.get(node) else {
            continue;
        };
        for edge in edges {
            examined += 1;
            if examined > 1_000 {
                return false;
            }
            if !matches_step(path.len(), edge)
                || edge.object == request.root
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
            if next.len() >= request.min_steps
                && request.terminal_predicates.contains(&edge.predicate)
                && !visit(&next)
            {
                return true;
            }
            if next.len() < request.max_steps {
                pending.push((edge.object, next));
            }
        }
    }
    true
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
        let extra_context = [citation("second-context"), citation("third-context")];
        let [first, second, _, competing, _] = edges;
        let competing_paths = [
            first,
            second,
            Relation {
                binding_citations: &extra_context,
                ..competing
            },
        ];
        assert_eq!(
            resolve(&competing_paths, &request, reader),
            Outcome::Value(vec![&competing_paths[0], &competing_paths[1]])
        );
        assert_eq!(
            resolve_matching(&competing_paths, &request, reader, |_, _| true, true),
            Outcome::Ambiguous
        );
    }
}
