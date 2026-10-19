//! The generic RDF verbalizer (the SEMMA+ seam's `None` branch): any entity of a
//! graph's own RDF -> one short, embeddable sentence. Design and measurements:
//! hub `plans/2026-10-14-agents-find-stuff/semantic-rdf.md` §3.
//!
//! Rules (the design under test):
//! - an entity is a named node that has a name-like or description-like literal, or a type in the
//!   schema allowlist (classes, properties, Data Cube structure, DCAT datasets, SKOS schemes);
//! - entities typed with a denied class (qb:Observation and every subclass the store declares) are skipped;
//! - labels, descriptions and literals are picked by a language preference list;
//! - types and relation targets render through their labels (label chain -> humanized local name ->
//!   dropped); no opaque identifier ever enters the text (the UUID guard);
//! - relations are bounded (predicates, values per predicate, quads scanned per entity).

use oxigraph::model::{NamedNodeRef, NamedOrBlankNodeRef, Term, TermRef};
use oxigraph::store::Store;
use serde::Serialize;
use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap, HashSet};

pub(crate) const RDF_TYPE: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#type";

/// Type-like predicates: rdf:type, and Wikidata's instance-of (P31) which stands in for it.
pub(crate) const TYPE_PREDS: &[&str] = &[
    "http://www.w3.org/1999/02/22-rdf-syntax-ns#type",
    "http://www.wikidata.org/prop/direct/P31",
];

/// Name-like predicates, in priority order.
pub(crate) const LABEL_PREDS: &[&str] = &[
    "http://www.w3.org/2004/02/skos/core#prefLabel",
    "http://www.w3.org/2000/01/rdf-schema#label",
    "http://schema.org/name",
    "https://schema.org/name",
    "http://purl.org/dc/terms/title",
    "http://purl.org/dc/elements/1.1/title",
    "http://xmlns.com/foaf/0.1/name",
];
pub(crate) const ALT_LABEL_PREDS: &[&str] = &[
    "http://www.w3.org/2004/02/skos/core#altLabel",
    "http://schema.org/alternateName",
    "https://schema.org/alternateName",
];
pub(crate) const DESC_PREDS: &[&str] = &[
    "http://www.w3.org/2004/02/skos/core#definition",
    "http://www.w3.org/2000/01/rdf-schema#comment",
    "http://schema.org/description",
    "https://schema.org/description",
    "http://purl.org/dc/terms/description",
    "http://purl.org/dc/elements/1.1/description",
    "http://www.w3.org/2004/02/skos/core#scopeNote",
];
const NOTATION: &str = "http://www.w3.org/2004/02/skos/core#notation";
const DIRECT_CLAIM: &str = "http://wikiba.se/ontology#directClaim";

/// Predicates whose values are never useful text (links, identity, housekeeping).
const SKIP_PREDS: &[&str] = &[
    "http://www.w3.org/2002/07/owl#sameAs",
    "http://www.w3.org/2000/01/rdf-schema#seeAlso",
    "http://www.w3.org/2000/01/rdf-schema#isDefinedBy",
    "http://www.w3.org/ns/dcat#downloadURL",
    "http://www.w3.org/ns/dcat#accessURL",
    "http://www.w3.org/ns/dcat#landingPage",
    "http://www.w3.org/ns/dcat#mediaType",
    "http://www.w3.org/ns/dcat#byteSize",
    "http://www.w3.org/ns/dcat#dataset",
    "http://xmlns.com/foaf/0.1/homepage",
    "http://xmlns.com/foaf/0.1/depiction",
    "http://schema.org/url",
    "http://schema.org/image",
    "http://purl.org/dc/terms/identifier",
    "http://purl.org/dc/terms/issued",
    "http://purl.org/dc/terms/modified",
    "http://www.w3.org/2002/07/owl#versionInfo",
    "http://www.w3.org/ns/prov#wasGeneratedBy",
    "http://www.w3.org/ns/prov#generatedAtTime",
];
/// Types too generic to say anything.
const GENERIC_TYPES: &[&str] = &[
    "http://www.w3.org/2002/07/owl#NamedIndividual",
    "http://www.w3.org/2002/07/owl#Thing",
    "http://www.w3.org/2000/01/rdf-schema#Resource",
];
/// Schema-level types indexed even without a label (the label falls back to the local name).
pub(crate) const ALWAYS_TYPES: &[&str] = &[
    "http://www.w3.org/2002/07/owl#Class",
    "http://www.w3.org/2000/01/rdf-schema#Class",
    "http://www.w3.org/2002/07/owl#ObjectProperty",
    "http://www.w3.org/2002/07/owl#DatatypeProperty",
    "http://www.w3.org/1999/02/22-rdf-syntax-ns#Property",
    "http://purl.org/linked-data/cube#DataSet",
    "http://purl.org/linked-data/cube#Slice",
    "http://purl.org/linked-data/cube#DataStructureDefinition",
    "http://purl.org/linked-data/cube#DimensionProperty",
    "http://purl.org/linked-data/cube#MeasureProperty",
    "http://purl.org/linked-data/cube#AttributeProperty",
    "http://www.w3.org/ns/dcat#Dataset",
    "http://www.w3.org/ns/dcat#Catalog",
    "http://www.w3.org/2004/02/skos/core#ConceptScheme",
    "http://rdfs.org/ns/void#Dataset",
];
/// Classes whose instances are skipped (with every declared subclass).
pub(crate) const DENY_TYPES: &[&str] = &["http://purl.org/linked-data/cube#Observation"];
/// Incoming predicates that make the subject a part of a parent (results roll up to it).
const PART_OF_INCOMING: &[&str] = &[
    "http://www.w3.org/ns/dcat#distribution",
    "http://purl.org/linked-data/cube#slice",
    "http://purl.org/dc/terms/hasPart",
    "http://rdfs.org/ns/void#subset",
];

#[derive(Clone, Debug)]
pub(crate) struct VerbalizeCfg {
    pub(crate) langs: Vec<String>,
    pub(crate) max_alt_labels: usize,
    pub(crate) max_descriptions: usize,
    pub(crate) description_chars: usize,
    pub(crate) max_types: usize,
    pub(crate) max_relations: usize,
    pub(crate) max_values: usize,
    pub(crate) max_quads_scanned: usize,
    pub(crate) max_chars: usize,
    /// The Sophia graph whose reserved projection graphs (documents, memory,
    /// salience…) are left out: the block index covers those.
    pub(crate) graph_id: String,
    pub(crate) normalize_names: bool,
}

impl Default for VerbalizeCfg {
    fn default() -> Self {
        Self {
            langs: vec!["en".into(), "es".into()],
            max_alt_labels: 2,
            max_descriptions: 2,
            description_chars: 320,
            max_types: 3,
            max_relations: 8,
            max_values: 3,
            max_quads_scanned: 400,
            max_chars: 700,
            graph_id: String::new(),
            normalize_names: true,
        }
    }
}

#[derive(Serialize, Clone, Debug)]
pub(crate) struct EntityRecord {
    pub(crate) iri: String,
    pub(crate) label: String,
    pub(crate) label_lang: String,
    pub(crate) types: Vec<String>,
    pub(crate) kind: String,
    pub(crate) graphs: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) parent: Option<String>,
    pub(crate) text: String,
}

#[derive(Default, Debug, Serialize, serde::Deserialize, Clone)]
#[serde(rename_all = "camelCase", default)]
pub(crate) struct VerbalizeStats {
    pub(crate) candidates: usize,
    pub(crate) kept: usize,
    pub(crate) skipped_denied_type: usize,
    pub(crate) skipped_no_label: usize,
    pub(crate) skipped_excluded_graph: usize,
    pub(crate) truncated_scans: usize,
    pub(crate) label_fallback_local_name: usize,
}

/// Shared, read-only context computed once per run.
pub(crate) struct Ctx<'a> {
    pub(crate) store: &'a Store,
    pub(crate) cfg: VerbalizeCfg,
    pub(crate) deny: HashSet<String>,
    pub(crate) always: HashSet<String>,
    pub(crate) dataset_types: HashSet<String>,
    /// wdt:Pxx -> wd:Pxx (Wikidata direct-claim predicates take their label from the property entity)
    pub(crate) direct_claim: HashMap<String, String>,
    /// Labels are cached per thread for one run only: a later run (after the
    /// graph changed) must never see an earlier run's labels.
    generation: u64,
}

static GENERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

thread_local! {
    static LABEL_CACHE: RefCell<(u64, HashMap<String, Option<(String, String)>>)> = RefCell::new((0, HashMap::new()));
}

impl<'a> Ctx<'a> {
    pub(crate) fn new(store: &'a Store, cfg: VerbalizeCfg) -> Self {
        let deny = subclass_closure(store, DENY_TYPES);
        let always = subclass_closure(store, ALWAYS_TYPES);
        let dataset_types = subclass_closure(
            store,
            &[
                "http://purl.org/linked-data/cube#DataSet",
                "http://www.w3.org/ns/dcat#Dataset",
                "http://rdfs.org/ns/void#Dataset",
            ],
        );
        let mut direct_claim = HashMap::new();
        if let Ok(p) = NamedNodeRef::new(DIRECT_CLAIM) {
            for q in store.quads_for_pattern(None, Some(p), None, None).flatten() {
                if let (oxigraph::model::NamedOrBlankNode::NamedNode(s), Term::NamedNode(o)) =
                    (&q.subject, &q.object)
                {
                    direct_claim.insert(o.as_str().to_string(), s.as_str().to_string());
                }
            }
        }
        let generation = GENERATION.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Self { store, cfg, deny, always, dataset_types, direct_claim, generation }
    }

    fn graph_ok(&self, g: &oxigraph::model::GraphName) -> bool {
        match g {
            oxigraph::model::GraphName::NamedNode(n) => {
                self.cfg.graph_id.is_empty()
                    || !crate::rdf_authority::is_reserved_rdf_graph_iri(&self.cfg.graph_id, n.as_str())
            }
            _ => true,
        }
    }

    /// Candidate subjects: anything with a name/description literal, or an allowlisted schema type.
    pub(crate) fn candidates(&self) -> Vec<String> {
        let mut set: HashSet<String> = HashSet::new();
        let mut preds: Vec<&str> = LABEL_PREDS.to_vec();
        preds.extend_from_slice(DESC_PREDS);
        for p in preds {
            let Ok(p) = NamedNodeRef::new(p) else { continue };
            for q in self.store.quads_for_pattern(None, Some(p), None, None).flatten() {
                if let oxigraph::model::NamedOrBlankNode::NamedNode(s) = &q.subject {
                    if self.graph_ok(&q.graph_name) && !set.contains(s.as_str()) {
                        set.insert(s.as_str().to_string());
                    }
                }
            }
        }
        let ty = NamedNodeRef::new(RDF_TYPE).unwrap();
        for t in &self.always {
            let Ok(t) = NamedNodeRef::new(t) else { continue };
            for q in self.store.quads_for_pattern(None, Some(ty), Some(t.into()), None).flatten() {
                if let oxigraph::model::NamedOrBlankNode::NamedNode(s) = &q.subject {
                    if self.graph_ok(&q.graph_name) && !set.contains(s.as_str()) {
                        set.insert(s.as_str().to_string());
                    }
                }
            }
        }
        let mut v: Vec<String> = set.into_iter().collect();
        v.sort_unstable();
        v
    }

    /// Best label for any IRI: label chain in language order; Wikidata direct claims via their property.
    pub(crate) fn label_of(&self, iri: &str) -> Option<(String, String)> {
        if let Some(hit) = LABEL_CACHE.with(|c| {
            let c = c.borrow();
            if c.0 == self.generation { c.1.get(iri).cloned() } else { None }
        }) {
            return hit;
        }
        let mut found = self.label_lookup(iri);
        if found.is_none() {
            if let Some(prop) = self.direct_claim.get(iri) {
                found = self.label_lookup(prop);
            }
        }
        LABEL_CACHE.with(|c| {
            let mut c = c.borrow_mut();
            if c.0 != self.generation || c.1.len() > 400_000 {
                c.0 = self.generation;
                c.1.clear();
            }
            c.1.insert(iri.to_string(), found.clone());
        });
        found
    }

    fn label_lookup(&self, iri: &str) -> Option<(String, String)> {
        let Ok(s) = NamedNodeRef::new(iri) else { return None };
        for p in LABEL_PREDS {
            let p = NamedNodeRef::new(p).unwrap();
            let lits: Vec<(String, String)> = self
                .store
                .quads_for_pattern(Some(NamedOrBlankNodeRef::from(s)), Some(p), None, None)
                .flatten()
                .filter_map(|q| match q.object {
                    Term::Literal(l) => Some((
                        l.value().to_string(),
                        l.language().unwrap_or("").to_string(),
                    )),
                    _ => None,
                })
                .collect();
            if let Some(best) = pick_lang(&lits, &self.cfg.langs) {
                if word_like(&best.0) {
                    return Some(best);
                }
            }
        }
        None
    }

    /// Label, or a humanized local name if it reads like words; never an opaque id.
    fn name_of(&self, iri: &str) -> Option<String> {
        if let Some((l, _)) = self.label_of(iri) {
            return Some(if self.cfg.normalize_names { text_name(&l) } else { l });
        }
        let h = humanize(local_name(iri));
        word_like(&h).then_some(h)
    }

    pub(crate) fn verbalize(&self, iri: &str, stats: &mut VerbalizeStats) -> Option<EntityRecord> {
        let s = NamedNodeRef::new(iri).ok()?;
        let mut labels: Vec<(String, String)> = Vec::new();
        let mut alts: Vec<(String, String)> = Vec::new();
        let mut descs: Vec<(String, String)> = Vec::new();
        let mut notations: Vec<String> = Vec::new();
        let mut types: Vec<String> = Vec::new();
        let mut rels: BTreeMap<String, Vec<Term>> = BTreeMap::new();
        let mut graphs: Vec<String> = Vec::new();
        let mut scanned = 0usize;
        let mut any_ok_graph = false;
        for q in self
            .store
            .quads_for_pattern(Some(NamedOrBlankNodeRef::from(s)), None, None, None)
            .flatten()
        {
            scanned += 1;
            if scanned > self.cfg.max_quads_scanned {
                stats.truncated_scans += 1;
                break;
            }
            if !self.graph_ok(&q.graph_name) {
                continue;
            }
            any_ok_graph = true;
            let g = match &q.graph_name {
                oxigraph::model::GraphName::NamedNode(n) => n.as_str().to_string(),
                oxigraph::model::GraphName::DefaultGraph => "default".to_string(),
                oxigraph::model::GraphName::BlankNode(_) => "blank".to_string(),
            };
            if !graphs.contains(&g) && graphs.len() < 4 {
                graphs.push(g);
            }
            let p = q.predicate.as_str();
            if TYPE_PREDS.contains(&p) {
                if let Term::NamedNode(t) = &q.object {
                    if !types.iter().any(|x| x == t.as_str()) {
                        types.push(t.as_str().to_string());
                    }
                }
                continue;
            }
            if let Term::Literal(l) = &q.object {
                let lv = (l.value().to_string(), l.language().unwrap_or("").to_string());
                if LABEL_PREDS.contains(&p) {
                    labels.push(lv);
                    continue;
                }
                if ALT_LABEL_PREDS.contains(&p) {
                    alts.push(lv);
                    continue;
                }
                if DESC_PREDS.contains(&p) {
                    descs.push(lv);
                    continue;
                }
                if p == NOTATION {
                    notations.push(lv.0);
                    continue;
                }
            }
            if SKIP_PREDS.contains(&p) {
                continue;
            }
            rels.entry(p.to_string()).or_default().push(q.object.clone());
        }
        if !any_ok_graph {
            stats.skipped_excluded_graph += 1;
            return None;
        }
        if types.iter().any(|t| self.deny.contains(t)) {
            stats.skipped_denied_type += 1;
            return None;
        }
        // types, specific first: broad umbrella types (skos:Concept, prov:Entity) go last
        const BROAD: &[&str] = &[
            "http://www.w3.org/2004/02/skos/core#Concept",
            "http://www.w3.org/ns/prov#Entity",
        ];
        types.sort_by_key(|t| BROAD.contains(&t.as_str()));
        // Label: the chain, else (for schema types and described things) the local name, else skip.
        let label = match pick_lang(&labels, &self.cfg.langs).filter(|(l, _)| word_like(l)) {
            Some(l) => l,
            None => {
                let schema = types.iter().any(|t| self.always.contains(t));
                // a code-like label ("0101") or local name ("2025") of a typed instance reads as "<type> <code>"
                let code_label = pick_lang(&labels, &self.cfg.langs).map(|l| l.0).filter(|l| l.chars().count() <= 12);
                let h = code_label.clone().unwrap_or_else(|| humanize(local_name(iri)));
                let schema = schema || (code_label.is_some() && !types.is_empty());
                let typed = types
                    .iter()
                    .find(|t| !GENERIC_TYPES.contains(&t.as_str()))
                    .and_then(|t| self.name_of(t))
                    .filter(|_| !h.is_empty() && h.chars().count() <= 12 && !word_like(&h));
                if (schema || !descs.is_empty()) && word_like(&h) {
                    stats.label_fallback_local_name += 1;
                    (h, String::new())
                } else if let (true, Some(tn)) = (schema || !descs.is_empty(), typed) {
                    stats.label_fallback_local_name += 1;
                    (format!("{tn} {h}"), String::new())
                } else {
                    stats.skipped_no_label += 1;
                    return None;
                }
            }
        };
        let mut parts: Vec<String> = Vec::new();
        // label (alternates in other languages / aliases)
        let mut alt_out: Vec<String> = Vec::new();
        for lang in &self.cfg.langs {
            if let Some((l, _)) = labels.iter().find(|(l, g)| g == lang && *l != label.0) {
                if word_like(l) && !alt_out.contains(l) {
                    alt_out.push(l.clone());
                }
            }
        }
        for (l, _) in order_by_lang(&alts, &self.cfg.langs) {
            if alt_out.len() >= self.cfg.max_alt_labels {
                break;
            }
            if word_like(&l) && l != label.0 && !alt_out.contains(&l) {
                alt_out.push(l);
            }
        }
        alt_out.truncate(self.cfg.max_alt_labels);
        let nm = |l: &str| if self.cfg.normalize_names { text_name(l) } else { l.to_string() };
        let alt_out: Vec<String> = alt_out.iter().map(|a| nm(a)).collect();
        let mut head = clean(&nm(&label.0), 160);
        if !alt_out.is_empty() {
            head.push_str(&format!(" ({})", alt_out.join("; ")));
        }
        if let Some(n) = notations.first() {
            head.push_str(&format!(" [code {}]", clean(n, 24)));
        }
        let mut type_names: Vec<String> = Vec::new();
        for t in types.iter().filter(|t| !GENERIC_TYPES.contains(&t.as_str())) {
            if type_names.len() >= self.cfg.max_types {
                break;
            }
            if let Some(n) = self.name_of(t) {
                if !type_names.contains(&n) {
                    type_names.push(n);
                }
            }
        }
        if !type_names.is_empty() {
            head.push_str(&format!(" — {}", type_names.join(", ")));
        }
        parts.push(head);
        // descriptions, one per preferred language
        let mut ndesc = 0;
        let mut seen_desc_langs: Vec<String> = Vec::new();
        for (d, lang) in order_by_lang(&descs, &self.cfg.langs) {
            if ndesc >= self.cfg.max_descriptions {
                break;
            }
            if seen_desc_langs.contains(&lang) {
                continue;
            }
            let d = clean(&d, self.cfg.description_chars);
            if d.is_empty() || d == label.0 {
                continue;
            }
            seen_desc_langs.push(lang);
            parts.push(d);
            ndesc += 1;
        }
        // Data Cube: a dataset says its dimensions and measures.
        if types.iter().any(|t| self.dataset_types.contains(t)) {
            if let Some(s) = self.cube_structure(iri) {
                parts.push(s);
            }
        }
        // relations: IRI-valued first (they carry meaning through labels), then short literals.
        let mut rendered: Vec<(bool, String)> = Vec::new();
        for (p, objs) in &rels {
            let Some(pname) = self.name_of(p) else { continue };
            let mut vals: Vec<String> = Vec::new();
            let mut more = 0usize;
            let mut has_iri = false;
            let lit_objs: Vec<(String, String)> = objs
                .iter()
                .filter_map(|o| match o {
                    Term::Literal(l) => Some((l.value().to_string(), l.language().unwrap_or("").to_string())),
                    _ => None,
                })
                .collect();
            let tagged = lit_objs.iter().any(|(_, g)| !g.is_empty());
            for o in objs {
                let v = match o {
                    Term::NamedNode(n) => {
                        has_iri = true;
                        self.name_of(n.as_str())
                    }
                    Term::Literal(l) => {
                        let lang = l.language().unwrap_or("");
                        if tagged && !lang.is_empty() && !self.cfg.langs.iter().any(|x| x == lang) {
                            None
                        } else {
                            literal_text(l.value(), l.datatype().as_str())
                        }
                    }
                    _ => None,
                };
                if let Some(v) = v {
                    if vals.contains(&v) {
                        continue;
                    }
                    if vals.len() < self.cfg.max_values {
                        vals.push(v);
                    } else {
                        more += 1;
                    }
                }
            }
            if vals.is_empty() {
                continue;
            }
            let mut s = format!("{pname}: {}", vals.join(", "));
            if more > 0 {
                s.push_str(&format!(" (+{more} more)"));
            }
            rendered.push((has_iri, s));
        }
        rendered.sort_by_key(|(iri, _)| !*iri);
        let rel_parts: Vec<String> = rendered
            .into_iter()
            .take(self.cfg.max_relations)
            .map(|(_, s)| s)
            .collect();
        // parent via incoming part-of
        let parent = self.parent_of(iri);
        let mut tail = rel_parts.join("; ");
        if let Some((_, plabel)) = &parent {
            if !tail.is_empty() {
                tail.push_str("; ");
            }
            tail.push_str(&format!("part of: {plabel}"));
        }
        if !tail.is_empty() {
            parts.push(tail);
        }
        // parts join with ". ", unless a part already ends a sentence (an
        // abbreviation such as "F.C." keeps its period)
        let mut text = String::new();
        for part in parts.iter().map(|p| p.trim()).filter(|p| !p.is_empty()) {
            if !text.is_empty() {
                text.push_str(if text.ends_with(['.', '…', '!', '?']) { " " } else { ". " });
            }
            text.push_str(part);
        }
        if text.chars().count() > self.cfg.max_chars {
            text = truncate_words(&text, self.cfg.max_chars);
        }
        let kind = self.kind_of(&types);
        stats.kept += 1;
        Some(EntityRecord {
            iri: iri.to_string(),
            label: label.0,
            label_lang: label.1,
            types: type_names,
            kind,
            graphs,
            parent: parent.map(|(p, _)| p),
            text,
        })
    }

    fn kind_of(&self, types: &[String]) -> String {
        let has = |list: &[&str]| types.iter().any(|t| list.contains(&t.as_str()));
        if types.iter().any(|t| self.dataset_types.contains(t)) {
            "dataset".into()
        } else if has(&["http://www.w3.org/ns/dcat#Distribution"]) {
            "distribution".into()
        } else if has(&["http://www.w3.org/2002/07/owl#Class", "http://www.w3.org/2000/01/rdf-schema#Class"]) {
            "class".into()
        } else if has(&[
            "http://www.w3.org/2002/07/owl#ObjectProperty",
            "http://www.w3.org/2002/07/owl#DatatypeProperty",
            "http://www.w3.org/1999/02/22-rdf-syntax-ns#Property",
            "http://purl.org/linked-data/cube#DimensionProperty",
            "http://purl.org/linked-data/cube#MeasureProperty",
            "http://purl.org/linked-data/cube#AttributeProperty",
        ]) {
            "property".into()
        } else if has(&["http://www.w3.org/2004/02/skos/core#Concept"]) {
            "concept".into()
        } else if has(&["http://www.w3.org/2004/02/skos/core#ConceptScheme"]) {
            "scheme".into()
        } else {
            "entity".into()
        }
    }

    fn parent_of(&self, iri: &str) -> Option<(String, String)> {
        let o = NamedNodeRef::new(iri).ok()?;
        for p in PART_OF_INCOMING {
            let p = NamedNodeRef::new(p).unwrap();
            if let Some(q) = self
                .store
                .quads_for_pattern(None, Some(p), Some(TermRef::from(o)), None)
                .flatten()
                .next()
            {
                if let oxigraph::model::NamedOrBlankNode::NamedNode(s) = q.subject {
                    let label = self.name_of(s.as_str()).unwrap_or_default();
                    return Some((s.as_str().to_string(), label));
                }
            }
        }
        None
    }

    /// qb:structure -> qb:component -> qb:dimension / qb:measure labels.
    fn cube_structure(&self, iri: &str) -> Option<String> {
        let s = NamedNodeRef::new(iri).ok()?;
        let structure = NamedNodeRef::new("http://purl.org/linked-data/cube#structure").unwrap();
        let component = NamedNodeRef::new("http://purl.org/linked-data/cube#component").unwrap();
        let dsd = self
            .store
            .quads_for_pattern(Some(s.into()), Some(structure), None, None)
            .flatten()
            .next()?
            .object;
        let dsd = match dsd {
            Term::NamedNode(n) => oxigraph::model::NamedOrBlankNode::NamedNode(n),
            Term::BlankNode(b) => oxigraph::model::NamedOrBlankNode::BlankNode(b),
            _ => return None,
        };
        let mut dims = Vec::new();
        let mut meas = Vec::new();
        for c in self
            .store
            .quads_for_pattern(Some(dsd.as_ref()), Some(component), None, None)
            .flatten()
            .take(64)
        {
            let c = match c.object {
                Term::NamedNode(n) => oxigraph::model::NamedOrBlankNode::NamedNode(n),
                Term::BlankNode(b) => oxigraph::model::NamedOrBlankNode::BlankNode(b),
                _ => continue,
            };
            for q in self.store.quads_for_pattern(Some(c.as_ref()), None, None, None).flatten() {
                let target = match q.predicate.as_str() {
                    "http://purl.org/linked-data/cube#dimension" => &mut dims,
                    "http://purl.org/linked-data/cube#measure" => &mut meas,
                    _ => continue,
                };
                if let Term::NamedNode(p) = q.object {
                    if let Some(n) = self.name_of(p.as_str()) {
                        if !target.contains(&n) && target.len() < 8 {
                            target.push(n);
                        }
                    }
                }
            }
        }
        if dims.is_empty() && meas.is_empty() {
            return None;
        }
        Some(format!("dimensions: {}; measures: {}", dims.join(", "), meas.join(", ")))
    }
}

pub(crate) fn subclass_closure(store: &Store, roots: &[&str]) -> HashSet<String> {
    let sub = NamedNodeRef::new("http://www.w3.org/2000/01/rdf-schema#subClassOf").unwrap();
    let mut out: HashSet<String> = roots.iter().map(|s| s.to_string()).collect();
    let mut frontier: Vec<String> = out.iter().cloned().collect();
    while let Some(c) = frontier.pop() {
        let Ok(cn) = NamedNodeRef::new(&c) else { continue };
        for q in store
            .quads_for_pattern(None, Some(sub), Some(TermRef::from(cn)), None)
            .flatten()
        {
            if let oxigraph::model::NamedOrBlankNode::NamedNode(s) = q.subject {
                if out.insert(s.as_str().to_string()) {
                    frontier.push(s.as_str().to_string());
                }
            }
        }
    }
    out
}

fn pick_lang(lits: &[(String, String)], langs: &[String]) -> Option<(String, String)> {
    for lang in langs {
        if let Some(x) = lits.iter().find(|(_, g)| g == lang) {
            return Some(x.clone());
        }
    }
    lits.iter()
        .find(|(_, g)| g.is_empty())
        .or_else(|| lits.first())
        .cloned()
}

fn order_by_lang(lits: &[(String, String)], langs: &[String]) -> Vec<(String, String)> {
    let rank = |g: &str| langs.iter().position(|x| x == g).unwrap_or(if g.is_empty() { langs.len() } else { langs.len() + 1 });
    let mut v = lits.to_vec();
    v.sort_by_key(|(_, g)| rank(g));
    v
}

pub(crate) fn local_name(iri: &str) -> &str {
    let t = iri.trim_end_matches(['/', '#']);
    t.rsplit(['#', '/', ':']).next().unwrap_or(t)
}

/// camelCase / snake_case / kebab-case -> words.
pub(crate) fn humanize(s: &str) -> String {
    let mut out = String::new();
    let chars: Vec<char> = s.chars().collect();
    for (i, &c) in chars.iter().enumerate() {
        if c == '_' || c == '-' || c == '.' || c == '+' {
            out.push(' ');
            continue;
        }
        if i > 0 && c.is_uppercase() && chars[i - 1].is_lowercase() {
            out.push(' ');
        }
        out.push(c);
    }
    let words: Vec<String> = out
        .split_whitespace()
        .map(|w| {
            if w.chars().all(|c| c.is_uppercase() || c.is_ascii_digit()) && w.len() > 1 {
                w.to_string()
            } else {
                w.to_lowercase()
            }
        })
        .collect();
    let joined = words.join(" ");
    // percent-decoding of the common case
    joined.replace("%20", " ")
}

/// The UUID guard: does this string read as words (not an id, hash, code or URL)?
pub(crate) fn word_like(s: &str) -> bool {
    let s = s.trim();
    if s.is_empty() || s.starts_with("http://") || s.starts_with("https://") || s.starts_with("urn:") {
        return false;
    }
    let letters = s.chars().filter(|c| c.is_alphabetic()).count();
    if letters < 2 {
        return false;
    }
    let total = s.chars().filter(|c| !c.is_whitespace()).count().max(1);
    // Q42 / P31 / L123 style ids
    let b = s.as_bytes();
    if b.len() > 1 && b[0].is_ascii_uppercase() && b[1..].iter().all(|c| c.is_ascii_digit()) {
        return false;
    }
    // long hex-ish runs (uuids, hashes)
    let mut run = 0usize;
    let mut run_digits = 0usize;
    for c in s.chars() {
        if c.is_ascii_hexdigit() || c == '-' {
            run += 1;
            if c.is_ascii_digit() {
                run_digits += 1;
            }
            if run >= 12 && run_digits >= 3 {
                return false;
            }
        } else {
            run = 0;
            run_digits = 0;
        }
    }
    letters * 10 >= total * 4
}

fn literal_text(v: &str, datatype: &str) -> Option<String> {
    let v = v.trim();
    if v.is_empty() || v.chars().count() > 120 {
        return None;
    }
    if v.starts_with("http://") || v.starts_with("https://") || v.starts_with("urn:") || v.starts_with("data:") {
        return None;
    }
    match datatype {
        "http://www.w3.org/2001/XMLSchema#dateTime" | "http://www.w3.org/2001/XMLSchema#date" => {
            let d = v.trim_start_matches('+');
            Some(d.get(..10).unwrap_or(d).to_string())
        }
        "http://www.w3.org/2001/XMLSchema#boolean" => None,
        "http://www.w3.org/2001/XMLSchema#base64Binary" | "http://www.w3.org/2001/XMLSchema#hexBinary" => None,
        dt if dt.ends_with("wktLiteral") || dt.ends_with("geoJSONLiteral") => None,
        "http://www.w3.org/2001/XMLSchema#string" | "http://www.w3.org/1999/02/22-rdf-syntax-ns#langString" => {
            // an opaque code in a string literal says nothing
            if !word_like(v) {
                None
            } else {
                Some(clean(v, 120))
            }
        }
        _ => Some(clean(v, 40)),
    }
}

/// Text face of a name: drop list enumerators ("2) ", "1a. ") and un-shout ALL-CAPS code-list labels,
/// which subword tokenizers split badly ("YEGUARIZOS" -> "Yeguarizos"). The stored label keeps its form.
pub(crate) fn text_name(s: &str) -> String {
    let t = s.trim();
    let b = t.as_bytes();
    let mut i = 0;
    while i < b.len() && b[i].is_ascii_digit() {
        i += 1;
    }
    if i > 0 && i < b.len() && b[i].is_ascii_lowercase() {
        i += 1;
    }
    let t = if i > 0 && i < b.len() && (b[i] == b')' || b[i] == b'.') && t[i + 1..].trim_start().chars().next().map(|c| c.is_alphabetic()).unwrap_or(false) {
        t[i + 1..].trim_start()
    } else {
        t
    };
    let letters: Vec<char> = t.chars().filter(|c| c.is_alphabetic()).collect();
    let upper = letters.iter().filter(|c| c.is_uppercase()).count();
    // short single words stay as written: they are usually acronyms (ANCAP, MIDES, UTE)
    let words: Vec<&str> = t.split(|c: char| !c.is_alphabetic()).filter(|w| !w.is_empty()).collect();
    let shouting = words.len() >= 2 || words.iter().any(|w| w.chars().count() >= 6);
    if letters.len() >= 4 && upper * 10 >= letters.len() * 8 && shouting {
        let mut out = String::with_capacity(t.len());
        let mut first = true;
        for c in t.chars() {
            if first && c.is_alphabetic() {
                out.extend(c.to_uppercase());
                first = false;
            } else {
                out.extend(c.to_lowercase());
            }
        }
        out
    } else {
        t.to_string()
    }
}

pub(crate) fn clean(s: &str, max: usize) -> String {
    let s = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if s.chars().count() <= max {
        s
    } else {
        truncate_words(&s, max)
    }
}

fn truncate_words(s: &str, max: usize) -> String {
    let cut: String = s.chars().take(max).collect();
    match cut.rfind(' ') {
        Some(i) if i > max / 2 => format!("{}…", &cut[..i]),
        _ => format!("{cut}…"),
    }
}


#[cfg(test)]
#[path = "entity_verbalize_tests.rs"]
mod tests;
