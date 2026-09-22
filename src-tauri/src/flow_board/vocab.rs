//! `flow_board::vocab` — the `resource -> flow:` predicate map (unit G2, item
//! 6), matching `build/contracts/vocabulary-map.md` (G1) exactly: one row per
//! materialized predicate, keyed by its DDL source column, in the same order
//! `vocabulary-map.md` lists them (== `sh:order`, ascending). Geometry
//! columns never appear here (interfaces.md §C excludes them from
//! `:projection:flow` entirely); `display_mode`/`parent_id`/`sort_order` do
//! (they are resource facts, not geometry).

use crate::emporium::contract::Datatype;

/// One materialized predicate: its CURIE local name (under `flow:`, i.e.
/// [`super::FLOW_NS`]), the DDL column it reads from, and its declared
/// datatype.
pub(crate) struct PredicateSpec {
    pub(crate) local: &'static str,
    pub(crate) column: &'static str,
    pub(crate) datatype: Datatype,
}

macro_rules! pred {
    ($local:expr, $column:expr, $dt:ident) => {
        PredicateSpec {
            local: $local,
            column: $column,
            datatype: Datatype::$dt,
        }
    };
}

/// Predicates for one DDL table. Every `uri`-datatype predicate's *object* is
/// resolved dynamically at triple-build time — the referenced row's own
/// class, looked up by id across the whole board (Flow mints ids via a
/// global `uuid()`, never reused across tables) — never a single fixed
/// target class, because several of these are declared `sh:or` unions in
/// `flow-shapes-v1.ttl` (e.g. `ownedBy`, `target`, `predecessor`).
pub(crate) fn predicates(ddl_table: &str) -> &'static [PredicateSpec] {
    match ddl_table {
        "systems" => &[
            pred!("parent", "parent_id", uri),
            pred!("name", "name", string),
            pred!("description", "description", string),
            pred!("displayMode", "display_mode", string),
            pred!("color", "color", string),
        ],
        "requirements" => &[
            pred!("ownedBy", "system_id", uri),
            pred!("text", "text", string),
            pred!("requirementSource", "source", string),
            pred!("status", "status", string),
            pred!("sortOrder", "sort_order", integer),
            pred!("startDate", "start_date", string),
            pred!("dueDate", "due_date", string),
            pred!("durationDays", "duration_days", integer),
        ],
        "tasks" => &[
            pred!("ownerType", "owner_type", string),
            pred!("ownedBy", "owner_id", uri),
            pred!("text", "text", string),
            pred!("done", "done", boolean),
            pred!("sortOrder", "sort_order", integer),
            pred!("startDate", "start_date", string),
            pred!("dueDate", "due_date", string),
            pred!("durationDays", "duration_days", integer),
        ],
        "workflows" => &[
            pred!("name", "name", string),
            pred!("description", "description", string),
        ],
        "workflow_links" => &[
            pred!("workflow", "workflow_id", uri),
            pred!("system", "system_id", uri),
            pred!("role", "role", string),
        ],
        "outcomes" => &[
            pred!("workflow", "workflow_id", uri),
            pred!("text", "text", string),
            pred!("achieved", "achieved", boolean),
            pred!("sortOrder", "sort_order", integer),
        ],
        "trades" => &[
            pred!("name", "name", string),
            pred!("tradeRationale", "description", string),
            pred!("winner", "winner_id", uri),
        ],
        "trade_links" => &[
            pred!("trade", "trade_id", uri),
            pred!("system", "system_id", uri),
        ],
        "constraints" => &[
            pred!("name", "name", string),
            pred!("description", "description", string),
        ],
        "constraint_links" => &[
            pred!("constraint", "constraint_id", uri),
            pred!("target", "target_id", uri),
            pred!("note", "note", string),
        ],
        "sources" => &[
            pred!("ownedBy", "owner_id", uri),
            pred!("sourceCitation", "text", string),
            pred!("sortOrder", "sort_order", integer),
        ],
        "edges" => &[
            pred!("sourceNode", "source_id", uri),
            pred!("targetNode", "target_id", uri),
            pred!("label", "label", string),
        ],
        "deps" => &[
            pred!("predecessor", "predecessor_id", uri),
            pred!("successor", "successor_id", uri),
        ],
        _ => &[],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::flow_board::columns::{resource_columns, TABLE_ORDER};

    /// Every predicate's `column` is a real, non-geometry DDL column of its
    /// table — catches a typo before it ever reaches a live doc.
    #[test]
    fn every_predicate_column_is_a_real_resource_column() {
        for (_, ddl_table, _, _) in TABLE_ORDER {
            let cols: Vec<&str> = resource_columns(ddl_table).collect();
            for p in predicates(ddl_table) {
                assert!(
                    cols.contains(&p.column),
                    "{ddl_table}.{} is not a resource column of {ddl_table} ({cols:?})",
                    p.column
                );
            }
        }
    }
}
