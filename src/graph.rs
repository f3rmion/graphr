//! Whole-snapshot adjacency held in memory for traversals that would otherwise
//! issue one SQLite query per visited node. Built once per sealed image and
//! shared read-only; node ids stay the image's SQLite ids.

use std::collections::VecDeque;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) enum Relation {
    Calls,
    TestCalls,
    Imports,
    /// Type to the trait or base type it implements or extends.
    Implements,
    /// Supertype member to the same-name member that overrides it.
    Dispatch,
}

impl Relation {
    pub(crate) fn parse(kind: &str) -> Option<Self> {
        match kind {
            "CALLS" => Some(Self::Calls),
            "TEST_CALLS" => Some(Self::TestCalls),
            "IMPORTS" => Some(Self::Imports),
            _ => None,
        }
    }

    /// Label for a hop walked along the edge direction.
    pub(crate) const fn forward(self) -> &'static str {
        match self {
            Self::Calls => "call ->",
            Self::TestCalls => "test-call ->",
            Self::Imports => "import ->",
            Self::Implements => "implements ->",
            Self::Dispatch => "dispatch ->",
        }
    }

    /// Label for a hop walked against the edge direction.
    pub(crate) const fn backward(self) -> &'static str {
        match self {
            Self::Calls => "caller <-",
            Self::TestCalls => "test <-",
            Self::Imports => "importer <-",
            Self::Implements => "impl <-",
            Self::Dispatch => "dispatch <-",
        }
    }
}

/// One traversal step: the node reached and how it was reached from its parent.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Step {
    pub id: i64,
    pub label: &'static str,
}

/// Compressed adjacency: neighbors of dense node `i` are
/// `neighbors[offsets[i]..offsets[i + 1]]`, sorted by (neighbor, relation).
#[derive(Debug)]
struct Adjacency {
    offsets: Vec<u32>,
    neighbors: Vec<(u32, Relation)>,
}

impl Adjacency {
    fn new(node_count: usize, mut pairs: Vec<(u32, u32, Relation)>) -> Self {
        pairs.sort_unstable();
        pairs.dedup();
        let mut offsets = vec![0_u32; node_count + 1];
        for &(from, _, _) in &pairs {
            offsets[from as usize + 1] += 1;
        }
        for index in 0..node_count {
            offsets[index + 1] += offsets[index];
        }
        let neighbors = pairs.into_iter().map(|(_, to, kind)| (to, kind)).collect();
        Self { offsets, neighbors }
    }

    fn of(&self, node: u32) -> &[(u32, Relation)] {
        let node = node as usize;
        &self.neighbors[self.offsets[node] as usize..self.offsets[node + 1] as usize]
    }
}

#[derive(Debug)]
pub(crate) struct CodeGraph {
    ids: Vec<i64>,
    out: Adjacency,
    inc: Adjacency,
}

/// Reverse reachability from a root, in breadth-first order.
pub(crate) struct Impact {
    pub steps: Vec<Step>,
    pub complete: bool,
}

impl CodeGraph {
    /// `ids` lists every node; edges naming an unknown node are rejected.
    pub(crate) fn new(mut ids: Vec<i64>, edges: &[(i64, i64, Relation)]) -> Result<Self, String> {
        ids.sort_unstable();
        ids.dedup();
        u32::try_from(ids.len()).map_err(|_| "graph exceeds u32 nodes".to_owned())?;
        let index = |id: i64| {
            ids.binary_search(&id)
                .map(|index| index as u32)
                .map_err(|_| format!("edge names unknown node {id}"))
        };
        let mut forward = Vec::with_capacity(edges.len());
        let mut backward = Vec::with_capacity(edges.len());
        for &(from, to, relation) in edges {
            let (from, to) = (index(from)?, index(to)?);
            forward.push((from, to, relation));
            backward.push((to, from, relation));
        }
        Ok(Self {
            out: Adjacency::new(ids.len(), forward),
            inc: Adjacency::new(ids.len(), backward),
            ids,
        })
    }

    fn index(&self, id: i64) -> Option<u32> {
        self.ids.binary_search(&id).ok().map(|index| index as u32)
    }

    /// Nodes that reach `root` (callers, tests, importers, implementors, and
    /// supertype members dispatching to it), up to `depth` hops and `limit`
    /// nodes. `complete` is false when either bound hid a reachable node.
    pub(crate) fn impact(&self, root: i64, depth: u32, limit: usize) -> Option<Impact> {
        let root = self.index(root)?;
        let mut seen = vec![false; self.ids.len()];
        seen[root as usize] = true;
        let mut queue = VecDeque::from([(root, 0_u32)]);
        let mut steps = Vec::new();
        let mut complete = true;
        while let Some((node, level)) = queue.pop_front() {
            for &(next, relation) in self.inc.of(node) {
                if seen[next as usize] {
                    continue;
                }
                if level == depth || steps.len() == limit {
                    complete = false;
                    continue;
                }
                seen[next as usize] = true;
                steps.push(Step {
                    id: self.ids[next as usize],
                    label: relation.backward(),
                });
                queue.push_back((next, level + 1));
            }
        }
        Some(Impact { steps, complete })
    }

    /// Shortest route from `from` to `to` within `max_hops`, following edge
    /// direction unless `undirected`. Ties resolve to the lowest node ids, so
    /// the route is deterministic. The first step carries an empty label.
    pub(crate) fn path(
        &self,
        from: i64,
        to: i64,
        max_hops: u32,
        undirected: bool,
    ) -> Option<Vec<Step>> {
        let (from, to) = (self.index(from)?, self.index(to)?);
        let mut parent = vec![None::<(u32, &'static str)>; self.ids.len()];
        let mut seen = vec![false; self.ids.len()];
        seen[from as usize] = true;
        let mut frontier = vec![from];
        let mut hops = 0;
        while !seen[to as usize] && !frontier.is_empty() && hops < max_hops {
            hops += 1;
            let mut next = Vec::new();
            for &node in &frontier {
                let forward = self
                    .out
                    .of(node)
                    .iter()
                    .map(|&(target, relation)| (target, relation.forward()));
                let backward = self
                    .inc
                    .of(node)
                    .iter()
                    .filter(|_| undirected)
                    .map(|&(source, relation)| (source, relation.backward()));
                for (target, label) in forward.chain(backward) {
                    if !seen[target as usize] {
                        seen[target as usize] = true;
                        parent[target as usize] = Some((node, label));
                        next.push(target);
                    }
                }
            }
            next.sort_unstable();
            frontier = next;
        }
        if !seen[to as usize] {
            return None;
        }
        let mut route = Vec::new();
        let mut node = to;
        while let Some((previous, label)) = parent[node as usize] {
            route.push(Step {
                id: self.ids[node as usize],
                label,
            });
            node = previous;
        }
        route.push(Step {
            id: self.ids[from as usize],
            label: "",
        });
        route.reverse();
        Some(route)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn graph() -> CodeGraph {
        // 1 test -> 2 read -> 3 Base::get ~> 4 Impl::get ; 5 Impl implements 6 Base;
        // 7 file imports 8 file; 9 isolated.
        CodeGraph::new(
            (1..=9).rev().collect(),
            &[
                (1, 2, Relation::TestCalls),
                (2, 3, Relation::Calls),
                (3, 4, Relation::Dispatch),
                (5, 6, Relation::Implements),
                (7, 8, Relation::Imports),
                (2, 3, Relation::Calls),
            ],
        )
        .unwrap()
    }

    fn ids(steps: &[Step]) -> Vec<(i64, &'static str)> {
        steps.iter().map(|step| (step.id, step.label)).collect()
    }

    #[test]
    fn impact_walks_callers_through_dispatch_within_bounds() {
        let graph = graph();
        let full = graph.impact(4, 6, 50).unwrap();
        assert_eq!(
            ids(&full.steps),
            [(3, "dispatch <-"), (2, "caller <-"), (1, "test <-")]
        );
        assert!(full.complete);
        let shallow = graph.impact(4, 2, 50).unwrap();
        assert_eq!(shallow.steps.len(), 2);
        assert!(!shallow.complete, "depth bound must be reported");
        let narrow = graph.impact(4, 6, 1).unwrap();
        assert_eq!(narrow.steps.len(), 1);
        assert!(!narrow.complete, "node bound must be reported");
        assert_eq!(
            ids(&graph.impact(6, 6, 50).unwrap().steps),
            [(5, "impl <-")]
        );
        assert!(graph.impact(42, 6, 50).is_none());
    }

    #[test]
    fn path_follows_direction_unless_undirected_and_respects_hops() {
        let graph = graph();
        assert_eq!(
            ids(&graph.path(1, 4, 8, false).unwrap()),
            [
                (1, ""),
                (2, "test-call ->"),
                (3, "call ->"),
                (4, "dispatch ->")
            ]
        );
        assert!(graph.path(4, 1, 8, false).is_none());
        assert_eq!(
            ids(&graph.path(4, 2, 8, true).unwrap()),
            [(4, ""), (3, "dispatch <-"), (2, "caller <-")]
        );
        assert!(graph.path(1, 4, 2, false).is_none());
        assert_eq!(ids(&graph.path(9, 9, 0, false).unwrap()), [(9, "")]);
        assert!(graph.path(1, 9, 8, true).is_none());
    }

    #[test]
    fn path_ties_resolve_to_lowest_ids() {
        // 1 -> {3, 2} -> 4: both routes have two hops; the lower id wins.
        let graph = CodeGraph::new(
            vec![1, 2, 3, 4],
            &[
                (1, 3, Relation::Calls),
                (1, 2, Relation::Calls),
                (3, 4, Relation::Calls),
                (2, 4, Relation::Calls),
            ],
        )
        .unwrap();
        assert_eq!(
            ids(&graph.path(1, 4, 8, false).unwrap()),
            [(1, ""), (2, "call ->"), (4, "call ->")]
        );
    }

    #[test]
    fn rejects_edges_to_unknown_nodes() {
        assert!(CodeGraph::new(vec![1], &[(1, 2, Relation::Calls)]).is_err());
    }
}
