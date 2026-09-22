//! `flow_board::columns` — the DDL column order per Flow table (unit G2).
//!
//! Source of truth: `tmp/mithras-flow/flow/src/db/schema.ts` (read at build
//! time, outside this worktree, per this unit's brief — the pinned suitor
//! tree, READ ONLY). `TABLE_ORDER` reproduces `schema.ts`'s `TABLES` const
//! (the 13 names, in `emptyModel()` order) plus the JSON-key spelling
//! (`workflowLinks` etc — Flow's own `serializeModel`/golden fixture use
//! camelCase JSON keys for the three-word tables even though the DDL/SQL
//! table names are snake_case) and the RDF class's two spellings.
//!
//! `ddl_columns` reproduces each `CREATE TABLE`'s column list verbatim, in
//! order, `id` always first. This is "the same column-order table G1
//! derived" this unit's brief asks to embed as data (G1's
//! `contracts/vocabulary-map.md` records the non-geometry, non-`id` subset
//! of this same ordering as each predicate's `sh:order`, one-indexed with
//! `id`=0 implicit and every geometry column skipped — cross-checked by hand
//! against every class table there at write time; the two never disagree).

/// One row per Flow DDL table, in `emptyModel()`/`TABLES` order
/// (`schema.ts:3-17`). `(json_key, ddl_table, kebab_class, pascal_class)`:
/// `json_key` is the top-level key Flow's `serializeModel` writes
/// (`"workflowLinks"`); `ddl_table` is the literal SQL table name
/// (`"workflow_links"`); `kebab_class`/`pascal_class` are the RDF class's two
/// spellings — `kebab_class` is the path segment `vocabulary-map.md`'s
/// `subject_rule` uses (`…:projection:flow:workflow-link:{localId}`),
/// `pascal_class` is the CURIE local name `flow.golden.json`'s `rdf_types`
/// uses (`"flow:WorkflowLink"`).
pub(crate) const TABLE_ORDER: [(&str, &str, &str, &str); 13] = [
    ("systems", "systems", "system", "System"),
    ("requirements", "requirements", "requirement", "Requirement"),
    ("tasks", "tasks", "task", "Task"),
    ("workflows", "workflows", "workflow", "Workflow"),
    (
        "workflowLinks",
        "workflow_links",
        "workflow-link",
        "WorkflowLink",
    ),
    ("outcomes", "outcomes", "outcome", "Outcome"),
    ("trades", "trades", "trade", "Trade"),
    ("tradeLinks", "trade_links", "trade-link", "TradeLink"),
    ("constraints", "constraints", "constraint", "Constraint"),
    (
        "constraintLinks",
        "constraint_links",
        "constraint-link",
        "ConstraintLink",
    ),
    ("sources", "sources", "source", "Source"),
    ("edges", "edges", "edge", "Edge"),
    // NOTE: table "deps" carries the class "Dependency" (not "Dep") —
    // vocabulary-map.md § flow:Dependency, subject_rule
    // "…:projection:flow:dependency:{localId}".
    ("deps", "deps", "dependency", "Dependency"),
];

/// Full DDL column list per table (`ddl_table` name), exact `CREATE TABLE`
/// order, `id` always first. Includes the geometry columns (needed to know
/// WHERE to re-join them on export — interfaces.md §E) — callers that only
/// want resource facts should filter with [`is_geometry_column`] (see
/// [`resource_columns`]).
pub(crate) fn ddl_columns(ddl_table: &str) -> &'static [&'static str] {
    match ddl_table {
        "systems" => &[
            "id",
            "parent_id",
            "name",
            "description",
            "display_mode",
            "color",
            "x",
            "y",
            "width",
            "height",
        ],
        "requirements" => &[
            "id",
            "system_id",
            "text",
            "source",
            "status",
            "sort_order",
            "start_date",
            "due_date",
            "duration_days",
        ],
        "tasks" => &[
            "id",
            "owner_type",
            "owner_id",
            "text",
            "done",
            "sort_order",
            "start_date",
            "due_date",
            "duration_days",
        ],
        "workflows" => &["id", "name", "description", "x", "y"],
        "workflow_links" => &["id", "workflow_id", "system_id", "role", "waypoints"],
        "outcomes" => &["id", "workflow_id", "text", "achieved", "sort_order"],
        "trades" => &["id", "name", "description", "winner_id", "x", "y"],
        "trade_links" => &["id", "trade_id", "system_id", "waypoints"],
        "constraints" => &["id", "name", "description", "x", "y"],
        "constraint_links" => &["id", "constraint_id", "target_id", "note", "waypoints"],
        "sources" => &["id", "owner_id", "text", "sort_order"],
        "edges" => &["id", "source_id", "target_id", "label", "waypoints"],
        "deps" => &["id", "predecessor_id", "successor_id"],
        _ => &[],
    }
}

/// The five geometry column names — uniform across every table that has them
/// (interfaces.md §C). Never a resource fact; always a scene register.
pub(crate) fn is_geometry_column(name: &str) -> bool {
    matches!(name, "x" | "y" | "width" | "height" | "waypoints")
}

/// [`ddl_columns`] minus geometry — the columns `resource.rows` actually
/// holds, in DDL order.
pub(crate) fn resource_columns(ddl_table: &str) -> impl Iterator<Item = &'static str> {
    ddl_columns(ddl_table)
        .iter()
        .copied()
        .filter(|c| !is_geometry_column(c))
}

pub(crate) fn kebab_class(ddl_table: &str) -> Option<&'static str> {
    TABLE_ORDER
        .iter()
        .find(|(_, t, _, _)| *t == ddl_table)
        .map(|(_, _, k, _)| *k)
}

pub(crate) fn pascal_class(ddl_table: &str) -> Option<&'static str> {
    TABLE_ORDER
        .iter()
        .find(|(_, t, _, _)| *t == ddl_table)
        .map(|(_, _, _, p)| *p)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_table_has_id_first_and_no_duplicate_columns() {
        for (_, ddl_table, _, _) in TABLE_ORDER {
            let cols = ddl_columns(ddl_table);
            assert_eq!(cols.first(), Some(&"id"), "{ddl_table}: id must be first");
            let mut sorted = cols.to_vec();
            sorted.sort_unstable();
            sorted.dedup();
            assert_eq!(
                sorted.len(),
                cols.len(),
                "{ddl_table}: no duplicate DDL columns"
            );
        }
    }

    #[test]
    fn thirteen_tables_thirteen_distinct_json_keys_and_ddl_names() {
        assert_eq!(TABLE_ORDER.len(), 13);
        let mut json_keys: Vec<&str> = TABLE_ORDER.iter().map(|(j, _, _, _)| *j).collect();
        let mut ddl_names: Vec<&str> = TABLE_ORDER.iter().map(|(_, t, _, _)| *t).collect();
        json_keys.sort_unstable();
        json_keys.dedup();
        ddl_names.sort_unstable();
        ddl_names.dedup();
        assert_eq!(json_keys.len(), 13);
        assert_eq!(ddl_names.len(), 13);
    }
}
