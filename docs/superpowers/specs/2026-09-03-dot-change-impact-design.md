# Opt-in DOT change-impact view

## Goal

Let a human export one immutable Graphr change snapshot as a deterministic,
bounded Graphviz DOT callgraph. The graph highlights changed symbols and the
existing affected caller flows without adding a renderer, UI, process
execution, file-writing API, or dependency.

## Non-goals

- Comparing base and head call edges.
- Rendering SVG, PNG, or an interactive graph.
- Returning a whole-repository graph.
- Adding imports, inheritance, or other non-call relationships.
- Making deleted or unmapped source look statically analyzed.

## MCP interface

Add an optional `format` enum to `changes`:

```text
changes({
  "snapshot_id": "<digest>",
  "depth": 6,
  "max_nodes": 50,
  "format": "dot"
})
```

`format` is `review` by default, preserving the current paged review. `dot`
returns only one complete DOT document plus the existing structured snapshot
provenance. DOT mode rejects `cursor`; a partial DOT fragment is never returned.

In DOT mode, `depth` limits the visible caller depth and `max_nodes` limits the
whole rendered graph rather than a page. The existing parameter bounds remain:
depth 0 through 6 and 1 through 50 nodes.

## Graph contents and layout

The renderer reuses the changed roots, risk scores, and affected flows already
computed by `Store::changes`; it does not parse Graphr's textual review output
or run new graph queries.

The DOT document uses `rankdir=LR`, and every edge points in actual call
direction from caller to callee. It contains:

- changed symbols first, ordered by the existing risk order;
- the highest-criticality affected flow paths that fit the remaining bounds;
- shared nodes and edges once, so converging paths merge visually;
- changed nodes with an orange fill and thicker border;
- derived callable impact roots for changed types/files in pale yellow and
  explicitly labeled `affected`, never `changed`;
- test nodes as ellipses, with calls originating in tests dashed; unchanged
  tests are blue while changed tests retain the orange changed fill;
- unchanged caller nodes as neutral boxes;
- dependency-boundary nodes in gray;
- labels with symbol name and `path:line`, plus risk on changed nodes.

For each discovered flow, the renderer follows the existing parent map to each
admitted changed or derived impact root and emits the depth-limited path suffix.
Only call paths that lead to an admitted changed or impact root are emitted.
There are no file clusters, radial layout, downstream-only callees, or legend
subgraph in the first version.

## Bounds and completeness

DOT output is at most 8 KiB and is always a syntactically complete
`digraph graphr_changes`. Nodes and whole path suffixes are admitted in
deterministic priority order until the node or byte budget is reached. Labels
are deterministically shortened when necessary. Changed roots take priority
over context; if even all changed roots do not fit, the existing risk order
decides which are shown. Context nodes are never emitted without a connecting
path to an admitted changed or impact root.

The graph label reports the snapshot ID and the existing analysis accounting.
It distinguishes discovered flows omitted by rendering from an incomplete flow
discovery whose total is unknown, and reports changed-root, deleted-path, and
unmapped-range omissions. Omitted data is never represented by a fake call
edge. A snapshot with no changes returns a valid empty `digraph` whose label
includes the structured no-change reason.

## Data flow

1. MCP deserialization validates the requested format and existing bounds.
2. `Engine::changes` resolves the immutable snapshot as it does today.
3. The cached review calculation runs the existing change and affected-flow
   analysis once and renders both the current compact graph text and DOT from
   the same structured results.
4. Review mode follows the current independent pagination unchanged. DOT mode
   returns the cached complete DOT string and refuses a cursor.
5. The MCP result retains the current structured provenance; Graphr does not
   create a file or invoke Graphviz.

The review cache remains keyed by snapshot ID, depth, and max-nodes because one
cached value contains both renderings. No new cache format or migration is
needed.

## Validation and failure behavior

DOT quoting escapes backslashes, quotes, and line breaks before any repository
text enters a label. UTF-8 remains UTF-8. Truncation occurs only at character
boundaries and cannot remove DOT framing.

Invalid formats are rejected by the MCP schema/deserializer. A cursor combined
with DOT mode is an invalid-parameters error. Corrupt graph data, cancellation,
snapshot lookup, and provenance failures keep the current terminal behavior;
DOT mode does not fall back to a live worktree or older snapshot.

Deleted and unmapped source appears only in graph-level completeness metadata
because it has no trustworthy node in the indexed head graph.

## Tests

- Exact DOT for a branched caller graph, including merged paths and call
  direction.
- Changed, test, ordinary, and dependency-boundary node styling.
- Risk ordering, depth/node bounds, byte truncation, and explicit omissions.
- DOT injection characters and multibyte label truncation.
- Valid empty DOT for every no-change reason.
- MCP schema/default behavior and rejection of DOT cursors.
- Existing review output and pagination remain byte-for-byte covered by current
  tests.
- Run `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`,
  `cargo test`, and `cargo build --locked --release`.
