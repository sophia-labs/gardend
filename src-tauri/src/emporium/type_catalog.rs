//! Typed objects (plans/2026-10-20-typed-objects, D1–D6): what one Emporium
//! class derives besides its SHACL shape ([`super::shacl_emit`]), and how a
//! graph's own types resolve.
//!
//! A graph's types are a Chamber domain ontology (`propose_domain_ontology`),
//! versioned and immutable per version; the built-in goldens answer the same
//! reads (their label falls back to the class name, their presentation is
//! empty). From one [`VocabularyContract`] class this module derives:
//!
//! - the **JSON Schema** an agent's submit tool is shaped by
//!   ([`contract_class_to_json_schema`]): flat, keyed by predicate local name,
//!   `additionalProperties: false`, nested classes inline (depth at most
//!   [`MAX_NESTING_DEPTH`]). JSON Schema is a face of the type, never its
//!   source (`openapi_emit` refuses the reverse mapping);
//! - the **catalogue entry** UIs and Sophia read ([`class_catalog_entry`]);
//! - the **typed projection** of an item's JSON ([`project_item`]): the
//!   `a <Class>` and field triples a run record adds, with nested values as
//!   child nodes under the item, and every value that does not fit named.
//!
//! The definition-time gate ([`validate_type_contract`]) refuses what these
//! faces cannot derive honestly: a nested class that is not in the
//! vocabulary, a cycle, nesting deeper than three, two fields with one key,
//! and a presentation that names a field the class does not have.

use oxigraph::store::Store;
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, BTreeSet};

use crate::emporium::chamber_ontology::{
    list_chamber_ontologies, resolve_chamber_ontology_version,
};
use crate::emporium::contract::{
    get_vocabulary, ClassSpec, Datatype, PredicateSpec, VocabularyContract,
};
use crate::emporium::planner::generic_value_strict;
use crate::emporium::shacl_emit::class_to_shacl;
use crate::emporium::shacl_validator::validate_against_shapes;
use crate::emporium::terms::{term_for, Term, Triple};

/// The seven nucleus DisplayKinds a presentation may name (Shrubbery
/// `packages/nucleus/src/kinds`).
pub(crate) const DISPLAY_KINDS: [&str; 7] = [
    "identity",
    "state",
    "metric",
    "prose",
    "reference",
    "testimony",
    "affordance",
];
/// Nested objects below an item: at most three levels (item → a → b → c).
pub(crate) const MAX_NESTING_DEPTH: usize = 3;
/// The longest item schema a workflow step takes (`workflow_plan_shapes.ttl`,
/// graph-plan.ts `GRAPH_PLAN_LIMITS`).
pub(crate) const ITEM_SCHEMA_MAX_CHARS: usize = 4000;
/// The vocabulary a graph's types go in unless a person names another.
pub(crate) const DEFAULT_TYPES_VOCAB: &str = "workspace";
/// Where published typed objects of a domain vocabulary live (D5, later).
pub(crate) const TYPES_WRITE_TARGET: &str = "projection:types";

const RDF_TYPE: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#type";

/// The recommended namespace of a graph's own vocabulary:
/// `urn:sophia:type:<graphId>:<vocab>#`.
pub(crate) fn type_namespace(graph_id: &str, vocab: &str) -> String {
    format!("urn:sophia:type:{graph_id}:{vocab}#")
}

/// The local name of a predicate CURIE (`t:who` → `who`) or IRI.
pub(crate) fn predicate_key(curie: &str) -> &str {
    if curie.contains("://") || curie.starts_with("urn:") {
        curie
            .rsplit(|c| c == '#' || c == '/' || c == ':')
            .find(|part| !part.is_empty())
            .unwrap_or(curie)
    } else {
        curie
            .split_once(':')
            .map(|(_, local)| local)
            .unwrap_or(curie)
    }
}

/// The IRI of a class: its primary-namespace `rdf:type`, else its first
/// expandable one. `None` for a class with no type (a registry-only class).
pub(crate) fn class_iri(contract: &VocabularyContract, class_name: &str) -> Option<String> {
    let class = contract.classes.get(class_name)?;
    let primary = contract.primary_namespace();
    let expanded: Vec<String> = class
        .rdf_types
        .iter()
        .filter_map(|t| contract.expand(t).ok())
        .collect();
    expanded
        .iter()
        .find(|uri| !primary.is_empty() && uri.starts_with(primary))
        .cloned()
        .or_else(|| expanded.first().cloned())
}

/// Whether `derive SHACL` gives this class a NodeShape (a primary-namespace
/// type), which a nested `sh:node` needs to point at.
fn class_has_shape(contract: &VocabularyContract, class_name: &str) -> bool {
    let primary = contract.primary_namespace();
    contract.classes.get(class_name).is_some_and(|class| {
        class
            .rdf_types
            .iter()
            .filter_map(|t| contract.expand(t).ok())
            .any(|uri| !primary.is_empty() && uri.starts_with(primary))
    })
}

fn non_empty(text: Option<&str>) -> Option<&str> {
    text.map(str::trim).filter(|text| !text.is_empty())
}

/// How people name the class: its label, else its name.
pub(crate) fn class_label(contract: &VocabularyContract, class_name: &str) -> String {
    contract
        .classes
        .get(class_name)
        .and_then(|class| non_empty(class.label.as_deref()))
        .unwrap_or(class_name)
        .to_string()
}

/// One field of a class: its predicate CURIE, its key (the predicate's local
/// name, or the CURIE when two predicates share a local name), its spec.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Field<'a> {
    pub(crate) curie: &'a str,
    pub(crate) key: &'a str,
    pub(crate) spec: &'a PredicateSpec,
}

impl Field<'_> {
    pub(crate) fn label(&self) -> &str {
        non_empty(self.spec.label.as_deref()).unwrap_or(self.key)
    }
}

/// A class's fields in presentation order: `presentation.fieldOrder` first,
/// then the rest by key. Deterministic: the JSON Schema, the catalogue and
/// every face read the same order.
pub(crate) fn ordered_fields(class: &ClassSpec) -> Vec<Field<'_>> {
    let mut used: BTreeSet<&str> = BTreeSet::new();
    let mut fields: Vec<Field<'_>> = class
        .predicates
        .iter()
        .map(|(curie, spec)| {
            let local = predicate_key(curie);
            let key = if used.insert(local) {
                local
            } else {
                curie.as_str()
            };
            Field {
                curie: curie.as_str(),
                key,
                spec,
            }
        })
        .collect();
    fields.sort_by(|a, b| a.key.cmp(b.key));
    let order: Vec<&str> = class
        .presentation
        .as_ref()
        .map(|p| p.field_order.iter().map(String::as_str).collect())
        .unwrap_or_default();
    let rank = |key: &str| order.iter().position(|k| *k == key).unwrap_or(usize::MAX);
    fields.sort_by_key(|field| rank(field.key));
    fields
}

// ── the definition-time gate ────────────────────────────────────────────────

/// What `propose_domain_ontology` refuses beyond its namespace and SHACL
/// gates, so every derived face is honest: nesting (a nested class must be a
/// class of this vocabulary with a shape, no cycle, depth at most three), keys
/// (two fields of one class may not share a local name), and presentation
/// (every key it names is a field of the class; kinds are DisplayKinds; roles
/// name text fields). Built-in goldens are never run through it.
pub(crate) fn validate_type_contract(contract: &VocabularyContract) -> Result<(), String> {
    for (class_name, class) in &contract.classes {
        let mut keys: BTreeMap<&str, &str> = BTreeMap::new();
        for (curie, pred) in &class.predicates {
            let key = predicate_key(curie);
            if let Some(other) = keys.insert(key, curie) {
                return Err(format!(
                    "{class_name}: {other} and {curie} share the field key {key:?}; \
                     give each field its own local name"
                ));
            }
            match (pred.datatype, pred.class.as_deref()) {
                (Datatype::object, None) => {
                    return Err(format!(
                        "{class_name}.{key} is a nested object but names no class \
                         (datatype \"object\" needs \"class\": another class of this vocabulary)"
                    ))
                }
                (Datatype::object, Some(nested)) => {
                    if !contract.classes.contains_key(nested) {
                        return Err(format!(
                            "{class_name}.{key} nests {nested}, which is not a class of \
                             vocabulary {}",
                            contract.name
                        ));
                    }
                    if !class_has_shape(contract, nested) {
                        return Err(format!(
                            "{class_name}.{key} nests {nested}, which has no rdf:type in the \
                             vocabulary's own namespace, so it has no shape to conform to"
                        ));
                    }
                }
                (_, Some(_)) => {
                    return Err(format!(
                        "{class_name}.{key} names a class but its datatype is {}; \
                         only datatype \"object\" nests a class",
                        pred.datatype.token()
                    ))
                }
                _ => {}
            }
        }
        validate_presentation(class_name, class)?;
    }
    for class_name in contract.classes.keys() {
        let depth = nesting_depth(contract, class_name, &mut vec![class_name.clone()])?;
        if depth > MAX_NESTING_DEPTH {
            return Err(format!(
                "{class_name} nests {depth} levels deep; at most {MAX_NESTING_DEPTH} levels of \
                 nested objects are allowed (link deeper structure by IRI instead)"
            ));
        }
    }
    Ok(())
}

/// How many levels of nested objects lie below `class_name`; a cycle is an
/// error naming its path.
fn nesting_depth(
    contract: &VocabularyContract,
    class_name: &str,
    path: &mut Vec<String>,
) -> Result<usize, String> {
    let Some(class) = contract.classes.get(class_name) else {
        return Ok(0);
    };
    let mut deepest = 0usize;
    for (curie, pred) in &class.predicates {
        if pred.datatype != Datatype::object {
            continue;
        }
        let Some(nested) = pred.class.as_deref() else {
            continue;
        };
        let step = format!("{}.{}", class_name, predicate_key(curie));
        if path.iter().any(|seen| seen == nested) {
            let mut cycle: Vec<String> = path.clone();
            cycle.push(nested.to_string());
            return Err(format!(
                "nested classes form a cycle ({}, through {step}); a value cannot contain \
                 itself: link it by IRI (datatype \"uri\") instead",
                cycle.join(" → ")
            ));
        }
        path.push(nested.to_string());
        let below = nesting_depth(contract, nested, path)?;
        path.pop();
        deepest = deepest.max(below + 1);
    }
    Ok(deepest)
}

fn validate_presentation(class_name: &str, class: &ClassSpec) -> Result<(), String> {
    let Some(presentation) = &class.presentation else {
        return Ok(());
    };
    let fields = ordered_fields(class);
    let field = |key: &str| fields.iter().find(|f| f.key == key).copied();
    let named = |what: &str, key: &str| -> Result<Field<'_>, String> {
        field(key).ok_or_else(|| {
            format!(
                "{class_name}'s presentation.{what} names {key:?}, which is not a field of \
                 {class_name} (its fields: {})",
                fields.iter().map(|f| f.key).collect::<Vec<_>>().join(", ")
            )
        })
    };
    if let Some(title) = &presentation.title {
        named("title", title)?;
    }
    if let Some(subtitle) = &presentation.subtitle {
        named("subtitle", subtitle)?;
    }
    let mut seen = BTreeSet::new();
    for key in &presentation.field_order {
        named("fieldOrder", key)?;
        if !seen.insert(key) {
            return Err(format!(
                "{class_name}'s presentation.fieldOrder lists {key:?} twice"
            ));
        }
    }
    for (key, kind) in &presentation.kinds {
        named("kinds", key)?;
        if !DISPLAY_KINDS.contains(&kind.as_str()) {
            return Err(format!(
                "{class_name}'s presentation.kinds gives {key:?} the kind {kind:?}; use one of {}",
                DISPLAY_KINDS.join(", ")
            ));
        }
    }
    for key in presentation.units.keys() {
        named("units", key)?;
    }
    let roles = &presentation.roles;
    for (role, key) in [
        ("quote", &roles.quote),
        ("note", &roles.note),
        ("document", &roles.document),
        ("block", &roles.block),
    ] {
        if let Some(key) = key {
            let field = named(&format!("roles.{role}"), key)?;
            if field.spec.datatype != Datatype::string || field.spec.multi {
                return Err(format!(
                    "{class_name}'s presentation.roles.{role} names {key:?}, which must be one \
                     text field (datatype \"string\", not multi) for Publish to read"
                ));
            }
        }
    }
    Ok(())
}

// ── the JSON Schema face ────────────────────────────────────────────────────

/// The JSON Schema of one class (D2): an object keyed by predicate local
/// name, `additionalProperties: false`, `required` from the contract, `multi`
/// as an array, nested classes inline. Datatypes: `string` → string; `uri` →
/// string ("an absolute IRI"); `integer`/`long` → integer; `float`/`double` →
/// number; `boolean` → boolean; `dateTime` → string ("ISO 8601 date-time");
/// `object` → the nested class's object schema. `title` is the class label
/// (else its name); descriptions come from the field labels. Pure and deterministic; no `format` (providers' structured output
/// refuses it). An unknown class is `{"type":"object"}`.
pub(crate) fn contract_class_to_json_schema(
    contract: &VocabularyContract,
    class_name: &str,
) -> Value {
    class_object_schema(contract, class_name, 0, None)
}

fn class_object_schema(
    contract: &VocabularyContract,
    class_name: &str,
    depth: usize,
    description_override: Option<String>,
) -> Value {
    let Some(class) = contract.classes.get(class_name) else {
        return json!({ "type": "object" });
    };
    let mut properties = Map::new();
    let mut required: Vec<Value> = Vec::new();
    for field in ordered_fields(class) {
        let description = field_description(field);
        let item = match field.spec.datatype {
            Datatype::object => match field.spec.class.as_deref() {
                // Past the depth the gate allows (never for a proposed
                // contract): an opaque object rather than an unbounded walk.
                Some(nested) if depth < MAX_NESTING_DEPTH => class_object_schema(
                    contract,
                    nested,
                    depth + 1,
                    if field.spec.multi {
                        None
                    } else {
                        description.clone()
                    },
                ),
                _ => json!({ "type": "object" }),
            },
            scalar => {
                let mut schema = Map::new();
                schema.insert("type".into(), json!(json_type(scalar)));
                if !field.spec.multi {
                    if let Some(description) = &description {
                        schema.insert("description".into(), json!(description));
                    }
                }
                Value::Object(schema)
            }
        };
        let schema = if field.spec.multi {
            let mut array = Map::new();
            array.insert("type".into(), json!("array"));
            array.insert("items".into(), item);
            if let Some(description) = &description {
                array.insert("description".into(), json!(description));
            }
            Value::Object(array)
        } else {
            item
        };
        properties.insert(field.key.to_string(), schema);
        if field.spec.required {
            required.push(json!(field.key));
        }
    }
    let mut schema = Map::new();
    schema.insert("type".into(), json!("object"));
    // Always titled (the label, else the class name): Choreograph's step
    // prompt says "Each item you hand back is a <title>".
    schema.insert("title".into(), json!(class_label(contract, class_name)));
    let description = description_override.or_else(|| {
        non_empty(class.description.as_deref())
            .or_else(|| non_empty(class.comment.as_deref()))
            .map(str::to_string)
    });
    if let Some(description) = description {
        schema.insert("description".into(), json!(description));
    }
    schema.insert("properties".into(), Value::Object(properties));
    if !required.is_empty() {
        schema.insert("required".into(), Value::Array(required));
    }
    schema.insert("additionalProperties".into(), json!(false));
    Value::Object(schema)
}

fn json_type(datatype: Datatype) -> &'static str {
    match datatype {
        Datatype::string | Datatype::uri | Datatype::dateTime => "string",
        Datatype::integer | Datatype::long => "integer",
        Datatype::float | Datatype::double => "number",
        Datatype::boolean => "boolean",
        Datatype::object => "object",
    }
}

/// `"<label>: <description> (<hint>)"`, each part when it is there; `None`
/// when there is nothing to say.
fn field_description(field: Field<'_>) -> Option<String> {
    let label = non_empty(field.spec.label.as_deref());
    let description = non_empty(field.spec.description.as_deref());
    let base = match (label, description) {
        (Some(label), Some(description)) => format!("{label}: {description}"),
        (Some(label), None) => label.to_string(),
        (None, Some(description)) => description.to_string(),
        (None, None) => String::new(),
    };
    let hint = match field.spec.datatype {
        Datatype::uri => Some("an absolute IRI"),
        Datatype::dateTime => Some("ISO 8601 date-time"),
        _ => None,
    };
    match (base.is_empty(), hint) {
        (true, None) => None,
        (true, Some(hint)) => {
            let mut chars = hint.chars();
            let first = chars.next().map(|c| c.to_ascii_uppercase());
            Some(first.into_iter().chain(chars).collect())
        }
        (false, None) => Some(base),
        (false, Some(hint)) => Some(format!("{base} ({hint})")),
    }
}

// ── the catalogue face ──────────────────────────────────────────────────────

/// One class's catalogue entry (`classCatalog[i]`, §4 "Type catalogue"):
/// `{iri, name, vocab, version, label, plural, description, fields:[{key, iri,
/// label, description, datatype, required, multi, class?}], presentation,
/// jsonSchema}`. `label` falls back to the class name, a field's to its key;
/// a built-in's `presentation` is `{}`. `fields[].class` is the nested class's
/// IRI, present only for `datatype: "object"`.
pub(crate) fn class_catalog_entry(
    contract: &VocabularyContract,
    class_name: &str,
    version: &str,
) -> Value {
    let Some(class) = contract.classes.get(class_name) else {
        return Value::Null;
    };
    let fields: Vec<Value> = ordered_fields(class)
        .into_iter()
        .map(|field| {
            let mut entry = Map::new();
            entry.insert("key".into(), json!(field.key));
            entry.insert("iri".into(), json!(contract.expand(field.curie).ok()));
            entry.insert("label".into(), json!(field.label()));
            entry.insert(
                "description".into(),
                json!(non_empty(field.spec.description.as_deref())
                    .or_else(|| non_empty(field.spec.comment.as_deref()))
                    .or_else(|| non_empty(field.spec.source.as_deref()))),
            );
            entry.insert("datatype".into(), json!(field.spec.datatype.token()));
            entry.insert("required".into(), json!(field.spec.required));
            entry.insert("multi".into(), json!(field.spec.multi));
            if field.spec.datatype == Datatype::object {
                let nested = field.spec.class.as_deref().unwrap_or_default();
                entry.insert("class".into(), json!(class_iri(contract, nested)));
            }
            Value::Object(entry)
        })
        .collect();
    json!({
        "iri": class_iri(contract, class_name),
        "name": class_name,
        "vocab": contract.name,
        "version": version,
        "label": class_label(contract, class_name),
        "plural": non_empty(class.plural.as_deref()),
        "description": non_empty(class.description.as_deref())
            .or_else(|| non_empty(class.comment.as_deref())),
        "fields": fields,
        "presentation": class
            .presentation
            .as_ref()
            .map(|p| serde_json::to_value(p).unwrap_or_else(|_| json!({})))
            .unwrap_or_else(|| json!({})),
        "jsonSchema": contract_class_to_json_schema(contract, class_name),
    })
}

/// Every class's catalogue entry, in class-name order.
pub(crate) fn class_catalog(contract: &VocabularyContract, version: &str) -> Vec<Value> {
    contract
        .classes
        .keys()
        .map(|name| class_catalog_entry(contract, name, version))
        .collect()
}

/// For each class whose JSON Schema is longer than a workflow step takes, a
/// warning (the class is still accepted: not every class is an item class).
pub(crate) fn schema_size_warnings(contract: &VocabularyContract) -> Vec<String> {
    contract
        .classes
        .keys()
        .filter_map(|name| {
            let size = contract_class_to_json_schema(contract, name)
                .to_string()
                .len();
            (size > ITEM_SCHEMA_MAX_CHARS).then(|| {
                format!(
                    "{name}'s JSON Schema is {size} characters; a workflow step's item schema \
                     takes at most {ITEM_SCHEMA_MAX_CHARS}, so {name} cannot be a step's itemClass"
                )
            })
        })
        .collect()
}

// ── resolution: which class an IRI or `vocab:Class` names in this graph ──────

/// Where a class comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TypeSource {
    Builtin,
    Domain,
}

impl TypeSource {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            TypeSource::Builtin => "builtin",
            TypeSource::Domain => "domain",
        }
    }
}

/// A class found in this graph's types: its contract (owned), its name, IRI
/// and the `<vocab>@<version>` it was read from.
#[derive(Debug, Clone)]
pub(crate) struct ResolvedClass {
    pub(crate) vocab: String,
    pub(crate) version: String,
    pub(crate) source: TypeSource,
    pub(crate) contract: VocabularyContract,
    pub(crate) class_name: String,
    pub(crate) iri: String,
}

impl ResolvedClass {
    /// `"<vocab>@<version>"`, the pin a workflow step carries.
    pub(crate) fn pin(&self) -> String {
        format!("{}@{}", self.vocab, self.version)
    }

    pub(crate) fn class(&self) -> &ClassSpec {
        &self.contract.classes[&self.class_name]
    }

    pub(crate) fn label(&self) -> String {
        class_label(&self.contract, &self.class_name)
    }

    pub(crate) fn plural(&self) -> Option<String> {
        non_empty(self.class().plural.as_deref()).map(str::to_string)
    }

    pub(crate) fn json_schema(&self) -> Value {
        contract_class_to_json_schema(&self.contract, &self.class_name)
    }

    /// `{iri, name, vocab, version, label, plural, fields}` (fields as the
    /// catalogue gives them): what `workflow_plan_read` shows on a step.
    pub(crate) fn summary(&self) -> Value {
        let entry = class_catalog_entry(&self.contract, &self.class_name, &self.version);
        json!({
            "iri": self.iri,
            "name": self.class_name,
            "vocab": self.vocab,
            "version": self.version,
            "source": self.source.as_str(),
            "label": entry["label"],
            "plural": entry["plural"],
            "fields": entry["fields"],
        })
    }
}

fn is_iri_reference(reference: &str) -> bool {
    reference.contains("://") || reference.starts_with("urn:")
}

fn find_class_by_iri(contract: &VocabularyContract, iri: &str) -> Option<String> {
    contract
        .classes
        .keys()
        .find(|name| class_iri(contract, name).as_deref() == Some(iri))
        .cloned()
}

/// The served built-in contracts, by name.
fn builtin_contracts() -> Vec<&'static VocabularyContract> {
    crate::emporium::vocabs::all_contracts()
        .iter()
        .filter_map(|c| get_vocabulary(c.name))
        .collect()
}

/// The class an IRI names among this graph's types: a built-in first
/// (embedded wins), then an active domain ontology, then (when
/// `include_superseded`) any domain version that declared it.
pub(crate) fn resolve_class_iri(
    store: &Store,
    graph_id: &str,
    iri: &str,
    include_superseded: bool,
) -> Result<Option<ResolvedClass>, String> {
    for contract in builtin_contracts() {
        if let Some(class_name) = find_class_by_iri(contract, iri) {
            return Ok(Some(ResolvedClass {
                vocab: contract.name.clone(),
                version: contract.version.clone(),
                source: TypeSource::Builtin,
                contract: contract.clone(),
                class_name,
                iri: iri.to_string(),
            }));
        }
    }
    let records = list_chamber_ontologies(store, graph_id)?;
    let mut ordered: Vec<_> = records.iter().filter(|r| r.status == "active").collect();
    if include_superseded {
        let mut older: Vec<_> = records.iter().filter(|r| r.status != "active").collect();
        // Newest superseded version first (whole versions compare as numbers).
        older.sort_by_key(|r| std::cmp::Reverse(r.version.parse::<u64>().unwrap_or(0)));
        ordered.extend(older);
    }
    for record in ordered {
        let Ok(contract) = serde_json::from_str::<VocabularyContract>(&record.contract_json) else {
            continue;
        };
        if let Some(class_name) = find_class_by_iri(&contract, iri) {
            return Ok(Some(ResolvedClass {
                vocab: record.name.clone(),
                version: record.version.clone(),
                source: TypeSource::Domain,
                contract,
                class_name,
                iri: iri.to_string(),
            }));
        }
    }
    Ok(None)
}

/// A reference a workflow step gives (`itemClass`), resolved: the class, or
/// the IRI it names when this graph has no such type.
#[derive(Debug, Clone)]
pub(crate) enum ClassReference {
    Known(Box<ResolvedClass>),
    Unknown { iri: String },
}

/// Resolve `itemClass`: an absolute IRI, or `<vocab>:<Class>` (a built-in or
/// domain vocabulary of this graph, embedded first). An unknown `vocab:Class`
/// still names an IRI (the vocabulary's namespace, or the graph's recommended
/// one `urn:sophia:type:<graphId>:<vocab>#`) so the step records what was
/// asked and the report can say it is not a type here. `Err` only for a
/// reference that is neither form, or a store fault.
pub(crate) fn resolve_class_reference(
    store: &Store,
    graph_id: &str,
    reference: &str,
) -> Result<ClassReference, String> {
    let reference = reference.trim();
    if is_iri_reference(reference) {
        oxigraph::model::NamedNode::new(reference)
            .map_err(|error| format!("itemClass {reference:?} is not an IRI: {error}"))?;
        return Ok(
            match resolve_class_iri(store, graph_id, reference, false)? {
                Some(found) => ClassReference::Known(Box::new(found)),
                None => ClassReference::Unknown {
                    iri: reference.to_string(),
                },
            },
        );
    }
    let Some((vocab, class_name)) = reference.split_once(':') else {
        return Err(format!(
            "itemClass {reference:?} is neither an IRI nor <vocab>:<Class> (e.g. workspace:DreamReport)"
        ));
    };
    let class_ok = class_name
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic())
        && class_name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    if vocab.is_empty() || !class_ok {
        return Err(format!(
            "itemClass {reference:?} is neither an IRI nor <vocab>:<Class> (e.g. workspace:DreamReport)"
        ));
    }
    if let Some(contract) = get_vocabulary(vocab) {
        return Ok(match contract.classes.get(class_name) {
            Some(_) => ClassReference::Known(Box::new(ResolvedClass {
                vocab: contract.name.clone(),
                version: contract.version.clone(),
                source: TypeSource::Builtin,
                contract: contract.clone(),
                class_name: class_name.to_string(),
                iri: class_iri(contract, class_name)
                    .unwrap_or_else(|| format!("{}{class_name}", contract.primary_namespace())),
            })),
            None => ClassReference::Unknown {
                iri: format!("{}{class_name}", contract.primary_namespace()),
            },
        });
    }
    if let Some(resolved) = resolve_chamber_ontology_version(store, graph_id, vocab, "latest")? {
        let contract = resolved.contract;
        let namespace = contract.primary_namespace().to_string();
        return Ok(match class_iri(&contract, class_name) {
            Some(iri) => ClassReference::Known(Box::new(ResolvedClass {
                vocab: contract.name.clone(),
                version: resolved.version,
                source: TypeSource::Domain,
                contract,
                class_name: class_name.to_string(),
                iri,
            })),
            None => ClassReference::Unknown {
                iri: format!("{namespace}{class_name}"),
            },
        });
    }
    Ok(ClassReference::Unknown {
        iri: format!("{}{class_name}", type_namespace(graph_id, vocab)),
    })
}

/// The class a step pinned (`wf:itemClass <iri>` + `wf:itemVocab
/// "<vocab>@<version>"`), at exactly that version (superseded versions stay
/// readable). Without a pin, the class the IRI names now.
pub(crate) fn resolve_pinned_class(
    store: &Store,
    graph_id: &str,
    iri: &str,
    pin: Option<&str>,
) -> Result<Option<ResolvedClass>, String> {
    let Some((vocab, version)) = pin.and_then(|pin| pin.rsplit_once('@')) else {
        return resolve_class_iri(store, graph_id, iri, false);
    };
    if let Some(contract) = get_vocabulary(vocab) {
        return Ok(
            find_class_by_iri(contract, iri).map(|class_name| ResolvedClass {
                vocab: contract.name.clone(),
                version: contract.version.clone(),
                source: TypeSource::Builtin,
                contract: contract.clone(),
                class_name,
                iri: iri.to_string(),
            }),
        );
    }
    let Some(resolved) = resolve_chamber_ontology_version(store, graph_id, vocab, version)? else {
        return Ok(None);
    };
    Ok(
        find_class_by_iri(&resolved.contract, iri).map(|class_name| ResolvedClass {
            vocab: vocab.to_string(),
            version: resolved.version.clone(),
            source: TypeSource::Domain,
            contract: resolved.contract,
            class_name,
            iri: iri.to_string(),
        }),
    )
}

// ── the compact definition form ─────────────────────────────────────────────

fn slug(text: &str) -> String {
    let mut out = String::new();
    let mut previous: Option<char> = None;
    for c in text.chars() {
        if c.is_ascii_uppercase()
            && previous.is_some_and(|p| p.is_ascii_lowercase() || p.is_ascii_digit())
        {
            out.push('-');
        }
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.ends_with('-') {
            out.push('-');
        }
        previous = Some(c);
    }
    out.trim_matches('-').to_string()
}

fn take_text(object: &Map<String, Value>, key: &str, at: &str) -> Result<Option<String>, String> {
    match object.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) if text.trim().is_empty() => Ok(None),
        Some(Value::String(text)) => Ok(Some(text.trim().to_string())),
        Some(other) => Err(format!("{at}: {key} must be text, not {other}")),
    }
}

fn take_bool(object: &Map<String, Value>, key: &str, at: &str) -> Result<bool, String> {
    match object.get(key) {
        None | Some(Value::Null) => Ok(false),
        Some(Value::Bool(flag)) => Ok(*flag),
        Some(other) => Err(format!("{at}: {key} must be true or false, not {other}")),
    }
}

fn identifier(text: &str) -> bool {
    text.chars().next().is_some_and(|c| c.is_ascii_alphabetic())
        && text.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        && text.len() <= 64
}

/// One compact class `{name, label?, plural?, description?, fields:[{key,
/// label?, datatype, required?, multi?, description?, class?}],
/// presentation?}` as the contract's class JSON, with every Emporium detail
/// filled in: `rdf_types` in the graph's namespace, `subject_rule`
/// `{graph_subject}:projection:types:<class-slug>:{localId}`, and the
/// signature of a current-state, halt-enforced, materialized class. A
/// `presentation` without `fieldOrder` takes the fields' order.
fn compact_class(prefix: &str, value: &Value, index: usize) -> Result<(String, Value), String> {
    let at = format!("classes[{index}]");
    let object = value
        .as_object()
        .ok_or_else(|| format!("{at} must be an object"))?;
    const CLASS_KEYS: [&str; 6] = [
        "name",
        "label",
        "plural",
        "description",
        "fields",
        "presentation",
    ];
    if let Some(unknown) = object.keys().find(|k| !CLASS_KEYS.contains(&k.as_str())) {
        return Err(format!(
            "{at} has an unknown field {unknown:?}; a class takes {}",
            CLASS_KEYS.join(", ")
        ));
    }
    let name = take_text(object, "name", &at)?
        .ok_or_else(|| format!("{at} needs name (e.g. \"DreamReport\")"))?;
    if !identifier(&name) {
        return Err(format!(
            "{at}: name {name:?} is not a class name (a letter, then letters, digits or _)"
        ));
    }
    let at = format!("class {name}");
    let fields = object
        .get("fields")
        .and_then(Value::as_array)
        .filter(|fields| !fields.is_empty())
        .ok_or_else(|| format!("{at} needs fields: a non-empty list"))?;
    let mut predicates = Map::new();
    let mut order: Vec<Value> = Vec::new();
    for (i, field) in fields.iter().enumerate() {
        let fat = format!("{at} fields[{i}]");
        let field = field
            .as_object()
            .ok_or_else(|| format!("{fat} must be an object"))?;
        const FIELD_KEYS: [&str; 7] = [
            "key",
            "label",
            "datatype",
            "required",
            "multi",
            "description",
            "class",
        ];
        if let Some(unknown) = field.keys().find(|k| !FIELD_KEYS.contains(&k.as_str())) {
            return Err(format!(
                "{fat} has an unknown field {unknown:?}; a field takes {}",
                FIELD_KEYS.join(", ")
            ));
        }
        let key = take_text(field, "key", &fat)?.ok_or_else(|| format!("{fat} needs key"))?;
        if !identifier(&key) {
            return Err(format!(
                "{fat}: key {key:?} is not a field key (a letter, then letters, digits or _)"
            ));
        }
        let datatype = take_text(field, "datatype", &fat)?
            .ok_or_else(|| format!("{fat} ({key}) needs datatype"))?;
        const TOKENS: [&str; 9] = [
            "string", "uri", "integer", "long", "float", "double", "boolean", "dateTime", "object",
        ];
        if !TOKENS.contains(&datatype.as_str()) {
            return Err(format!(
                "{fat} ({key}): datatype {datatype:?} is not one of {}",
                TOKENS.join(", ")
            ));
        }
        let mut spec = Map::new();
        spec.insert("datatype".into(), json!(datatype));
        spec.insert(
            "required".into(),
            json!(take_bool(field, "required", &fat)?),
        );
        spec.insert("multi".into(), json!(take_bool(field, "multi", &fat)?));
        if let Some(label) = take_text(field, "label", &fat)? {
            spec.insert("label".into(), json!(label));
        }
        if let Some(description) = take_text(field, "description", &fat)? {
            spec.insert("description".into(), json!(description));
        }
        if let Some(class) = take_text(field, "class", &fat)? {
            spec.insert("class".into(), json!(class));
        }
        let curie = format!("{prefix}:{key}");
        if predicates.insert(curie, Value::Object(spec)).is_some() {
            return Err(format!("{at} has two fields with the key {key:?}"));
        }
        order.push(json!(key));
    }
    let mut presentation = match object.get("presentation") {
        None | Some(Value::Null) => Map::new(),
        Some(Value::Object(p)) => p.clone(),
        Some(other) => return Err(format!("{at}: presentation must be an object, not {other}")),
    };
    if !presentation.contains_key("fieldOrder") {
        presentation.insert("fieldOrder".into(), Value::Array(order));
    }
    let mut class = Map::new();
    if let Some(label) = take_text(object, "label", &at)? {
        class.insert("label".into(), json!(label));
    }
    if let Some(plural) = take_text(object, "plural", &at)? {
        class.insert("plural".into(), json!(plural));
    }
    if let Some(description) = take_text(object, "description", &at)? {
        class.insert("description".into(), json!(description));
    }
    class.insert("rdf_types".into(), json!([format!("{prefix}:{name}")]));
    class.insert(
        "subject_rule".into(),
        json!(format!(
            "{{graph_subject}}:projection:types:{}:{{localId}}",
            slug(&name)
        )),
    );
    class.insert("source_kind".into(), json!("current-state"));
    class.insert("identity_kind".into(), json!("logical-id"));
    class.insert("store_mode".into(), json!("materialize"));
    class.insert("store_target".into(), json!(TYPES_WRITE_TARGET));
    class.insert("enforcement".into(), json!("halt"));
    class.insert("reconciliation_strategy".into(), json!("codeBacked"));
    class.insert("dispatch_mode".into(), json!("current-state-materialize"));
    class.insert("predicates".into(), Value::Object(predicates));
    class.insert("presentation".into(), Value::Object(presentation));
    Ok((name, Value::Object(class)))
}

/// The graph-local CURIE prefix of a graph's own vocabulary: `t-<graph-slug>`
/// (never a reserved system prefix).
pub(crate) fn types_prefix(graph_id: &str) -> String {
    let slug = slug(graph_id);
    if slug.is_empty() {
        "t".to_string()
    } else {
        format!("t-{slug}")
    }
}

/// The next version of a domain vocabulary: one more than the largest whole
/// version on record, `"1"` for the first.
pub(crate) fn next_types_version(
    store: &Store,
    graph_id: &str,
    vocab: &str,
) -> Result<String, String> {
    let records = list_chamber_ontologies(store, graph_id)?;
    let versions: Vec<&str> = records
        .iter()
        .filter(|r| r.name == vocab)
        .map(|r| r.version.as_str())
        .collect();
    let largest = versions
        .iter()
        .filter_map(|v| v.parse::<u64>().ok())
        .max()
        .unwrap_or(0)
        .max(versions.len() as u64);
    Ok((largest + 1).to_string())
}

/// Build the full contract JSON for a compact definition (`classes` without
/// `contract`): the prior active version's classes, with each class given here
/// replacing its namesake wholesale and each of `remove_classes` dropped. The
/// prior version must itself be a compact-built one (same namespace and
/// prefix); a vocabulary proposed as a full contract is extended with a full
/// contract.
#[allow(clippy::too_many_arguments)]
pub(crate) fn compact_types_contract(
    graph_id: &str,
    vocab: &str,
    version: &str,
    title: Option<&str>,
    description: Option<&str>,
    classes: &[Value],
    remove_classes: &[String],
    prior: Option<&Value>,
) -> Result<Value, String> {
    let vocab_ok = vocab.chars().next().is_some_and(|c| c.is_ascii_lowercase())
        && vocab
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && vocab.len() <= 63;
    if !vocab_ok {
        return Err(format!(
            "vocab {vocab:?} is not a vocabulary name (a lowercase letter, then lowercase \
             letters, digits or -)"
        ));
    }
    let prefix = types_prefix(graph_id);
    let namespace = type_namespace(graph_id, vocab);
    let mut merged = Map::new();
    let mut prior_title = None;
    let mut prior_description = None;
    if let Some(prior) = prior {
        let same = prior["primary_prefix"].as_str() == Some(prefix.as_str())
            && prior["namespaces"][&prefix].as_str() == Some(namespace.as_str());
        if !same {
            return Err(format!(
                "vocab {vocab} was proposed as a full contract (namespace {}); extend it by \
                 proposing a full contract, or name another vocab for these classes",
                prior["namespaces"][prior["primary_prefix"].as_str().unwrap_or_default()]
            ));
        }
        if let Some(classes) = prior["classes"].as_object() {
            merged = classes.clone();
        }
        prior_title = prior["title"].as_str().map(str::to_string);
        prior_description = prior["description"].as_str().map(str::to_string);
    }
    for remove in remove_classes {
        if merged.shift_remove(remove).is_none() {
            return Err(format!(
                "removeClasses names {remove}, which is not a class of {vocab}"
            ));
        }
    }
    for (index, class) in classes.iter().enumerate() {
        let (name, class) = compact_class(&prefix, class, index)?;
        merged.insert(name, class);
    }
    if merged.is_empty() {
        return Err(format!("vocab {vocab} would have no classes"));
    }
    let mut namespaces = Map::new();
    namespaces.insert(prefix.clone(), json!(namespace));
    namespaces.insert(
        "rdf".into(),
        json!("http://www.w3.org/1999/02/22-rdf-syntax-ns#"),
    );
    namespaces.insert("xsd".into(), json!("http://www.w3.org/2001/XMLSchema#"));
    let mut contract = Map::new();
    contract.insert("name".into(), json!(vocab));
    contract.insert("version".into(), json!(version));
    contract.insert(
        "title".into(),
        json!(title
            .map(str::to_string)
            .or(prior_title)
            .unwrap_or_else(|| format!("Types of {graph_id}"))),
    );
    contract.insert(
        "description".into(),
        json!(description
            .map(str::to_string)
            .or(prior_description)
            .unwrap_or_else(|| format!("The types graph {graph_id} defines for itself."))),
    );
    contract.insert("namespaces".into(), Value::Object(namespaces));
    contract.insert("primary_prefix".into(), json!(prefix));
    contract.insert("write_target".into(), json!(TYPES_WRITE_TARGET));
    contract.insert("classes".into(), Value::Object(merged));
    Ok(Value::Object(contract))
}

// ── the typed projection of an item ─────────────────────────────────────────

/// An item's JSON as the class's triples about `subject`: `a <Class>`, one
/// triple per field value (strictly coerced: a value the generic coercion
/// would quietly change is a violation instead), and, for each nested value,
/// a child node `<subject>/<key>` (`<subject>/<key>/<i>` for a list, `i` from
/// 0) typed with the nested class and linked by the field's predicate, then
/// the whole set checked against the class's derived shapes. `Err` lists every
/// violation, each naming its path (`place.city`, `witnesses[1].name`).
pub(crate) fn project_item(
    contract: &VocabularyContract,
    class_name: &str,
    subject: &str,
    item: &Value,
    target_graph: &str,
) -> Result<Vec<Triple>, Vec<String>> {
    let mut triples = Vec::new();
    let mut problems = Vec::new();
    let mut paths: BTreeMap<String, String> = BTreeMap::new();
    project_node(
        contract,
        class_name,
        subject,
        item,
        "",
        0,
        &mut triples,
        &mut problems,
        &mut paths,
    );
    if !problems.is_empty() {
        return Err(problems);
    }
    let shapes = class_to_shacl(contract, class_name);
    if let Err(violations) =
        validate_against_shapes(&shapes, &triples, target_graph, &contract.name)
    {
        return Err(violations
            .iter()
            .map(|violation| {
                let bare = |text: &str| {
                    text.trim()
                        .trim_start_matches('<')
                        .trim_end_matches('>')
                        .to_string()
                };
                // The node the shape judged (rudof names the SHAPE as the
                // focus of a closed-shape result), then the field it is about.
                let node = paths.get(&bare(&violation.focus_node)).cloned();
                let key = violation
                    .property_path
                    .as_deref()
                    .map(|path| predicate_key(&bare(path)).to_string());
                let at = match (node.as_deref(), key) {
                    (Some(""), Some(key)) | (None, Some(key)) => key,
                    (Some(node), Some(key)) => format!("{node}.{key}"),
                    (Some(node), None) if !node.is_empty() => node.to_string(),
                    _ => "the item".to_string(),
                };
                let message = if violation.message == "constraint violated" && node.is_none() {
                    "is not allowed by the class's shape".to_string()
                } else {
                    violation.message.clone()
                };
                format!("{at}: {message}")
            })
            .collect());
    }
    Ok(triples)
}

fn join_path(path: &str, key: &str) -> String {
    if path.is_empty() {
        key.to_string()
    } else {
        format!("{path}.{key}")
    }
}

#[allow(clippy::too_many_arguments)]
fn project_node(
    contract: &VocabularyContract,
    class_name: &str,
    subject: &str,
    value: &Value,
    path: &str,
    depth: usize,
    triples: &mut Vec<Triple>,
    problems: &mut Vec<String>,
    paths: &mut BTreeMap<String, String>,
) {
    let label = class_label(contract, class_name);
    let at = if path.is_empty() { "the item" } else { path };
    let (Some(class), Some(iri)) = (
        contract.classes.get(class_name),
        class_iri(contract, class_name),
    ) else {
        problems.push(format!(
            "{at}: {class_name} is not a class of {}",
            contract.name
        ));
        return;
    };
    let Some(object) = value.as_object() else {
        problems.push(format!("{at}: expected an object (a {label}), got {value}"));
        return;
    };
    paths.insert(subject.to_string(), path.to_string());
    triples.push((
        subject.to_string(),
        RDF_TYPE.to_string(),
        term_for(&crate::emporium::terms::Value::Uri(iri), Datatype::uri),
    ));
    let fields = ordered_fields(class);
    for key in object.keys() {
        if !fields.iter().any(|field| field.key == key) {
            problems.push(format!(
                "{} is not a field of {label} (its fields: {})",
                join_path(path, key),
                fields.iter().map(|f| f.key).collect::<Vec<_>>().join(", ")
            ));
        }
    }
    for field in fields {
        let field_path = join_path(path, field.key);
        let Ok(predicate) = contract.expand(field.curie) else {
            problems.push(format!(
                "{field_path}: the contract cannot expand {}",
                field.curie
            ));
            continue;
        };
        let values: Vec<(Option<usize>, &Value)> = match object.get(field.key) {
            None | Some(Value::Null) => Vec::new(),
            Some(Value::Array(list)) if field.spec.multi => list
                .iter()
                .enumerate()
                .filter(|(_, v)| !v.is_null())
                .map(|(i, v)| (Some(i), v))
                .collect(),
            Some(Value::Array(_)) => {
                problems.push(format!("{field_path}: expected one value, got a list"));
                continue;
            }
            Some(single) if field.spec.multi => vec![(Some(0), single)],
            Some(single) => vec![(None, single)],
        };
        if values.is_empty() {
            if field.spec.required {
                problems.push(format!("{field_path} is missing; a {label} needs it"));
            }
            continue;
        }
        for (index, value) in values {
            let value_path = match index {
                Some(i) if field.spec.multi => format!("{field_path}[{i}]"),
                _ => field_path.clone(),
            };
            if field.spec.datatype == Datatype::object {
                let Some(nested) = field.spec.class.as_deref() else {
                    problems.push(format!("{value_path}: the contract names no nested class"));
                    continue;
                };
                if depth >= MAX_NESTING_DEPTH {
                    problems.push(format!(
                        "{value_path}: nested deeper than {MAX_NESTING_DEPTH} levels"
                    ));
                    continue;
                }
                let child = match index {
                    Some(i) if field.spec.multi => format!("{subject}/{}/{i}", field.key),
                    _ => format!("{subject}/{}", field.key),
                };
                triples.push((
                    subject.to_string(),
                    predicate.clone(),
                    term_for(
                        &crate::emporium::terms::Value::Uri(child.clone()),
                        Datatype::uri,
                    ),
                ));
                project_node(
                    contract,
                    nested,
                    &child,
                    value,
                    &value_path,
                    depth + 1,
                    triples,
                    problems,
                    paths,
                );
                continue;
            }
            match generic_value_strict(value, field.spec.datatype) {
                Ok(Some(coerced)) => triples.push((
                    subject.to_string(),
                    predicate.clone(),
                    term_for(&coerced, field.spec.datatype),
                )),
                Ok(None) => {}
                Err(error) => problems.push(format!("{value_path}: {error}")),
            }
        }
    }
}

/// Turn a projection's emporium triples into N-Triples terms (the run record
/// writes oxigraph model triples).
pub(crate) fn term_to_oxigraph(term: &Term) -> oxigraph::model::Term {
    match term {
        Term::Uri(node) => oxigraph::model::Term::NamedNode(node.clone()),
        Term::Lit(literal) => oxigraph::model::Term::Literal(literal.clone()),
        Term::Placeholder(name) => {
            oxigraph::model::Term::NamedNode(oxigraph::model::NamedNode::new_unchecked(format!(
                "{}{name}",
                crate::emporium::terms::PLACEHOLDER_NS
            )))
        }
    }
}
