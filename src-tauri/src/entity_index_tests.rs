use super::*;

/// A scratch directory removed when dropped.
struct TempDir(PathBuf);
impl TempDir {
    fn new() -> Self {
        let dir =
            std::env::temp_dir().join(format!("garden-entity-index-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }
    fn path(&self) -> &Path {
        &self.0
    }
}
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn row(iri: &str, label: &str, kind: &str, parent: Option<&str>, text: &str) -> EntityRow {
    EntityRow {
        iri: iri.into(),
        label: label.into(),
        type_label: kind.into(),
        kind: kind.into(),
        graph: "urn:g".into(),
        parent: parent.map(str::to_string),
        text: text.into(),
        hash: "h".into(),
        license: None,
    }
}

fn unit(v: &[f32]) -> Vec<f32> {
    let n = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    v.iter().map(|x| x / n).collect()
}

/// Rows and vectors written to disk and loaded back, as a refresh leaves them.
fn index_from(rows: Vec<EntityRow>, vectors: Vec<Vec<f32>>) -> (TempDir, Arc<LoadedEntityIndex>) {
    let dir = TempDir::new();
    let dims = vectors[0].len();
    let scale = write_index_files(dir.path(), &rows, &vectors, dims).expect("write");
    let manifest = EntityIndexManifest {
        schema_version: ENTITY_INDEX_SCHEMA_VERSION,
        graph_id: "g".into(),
        model_id: DEFAULT_ENTITY_MODEL.into(),
        dimensions: dims,
        query_prefix: "query: ".into(),
        passage_prefix: "passage: ".into(),
        verbalizer: VERBALIZER_VERSION.into(),
        langs: vec!["es".into()],
        count: rows.len(),
        vector_scale: scale,
        built_at: format!("t-{}", rows.len()),
        build_seconds: 0.0,
        verbalize_seconds: 0.0,
        embed_seconds: 0.0,
        embedded: rows.len(),
        reused: 0,
        threads: 1,
        kinds: BTreeMap::new(),
        graphs: BTreeMap::new(),
        stats: VerbalizeStats::default(),
    };
    write_atomic(
        &entity_index_dir(dir.path()).join("manifest.json"),
        &serde_json::to_vec(&manifest).unwrap(),
    )
    .unwrap();
    let index = load_entity_index(dir.path()).expect("load");
    (dir, index)
}

fn no_filter() -> FindFilter {
    FindFilter {
        kinds: vec![],
        types: vec![],
        graphs: vec![],
    }
}

#[test]
fn a_part_answers_as_its_parent_and_says_which_part() {
    let rows = vec![
        row(
            "urn:ds",
            "Precios de combustibles",
            "dataset",
            None,
            "Precios de combustibles — dataset. Precios máximos.",
        ),
        row(
            "urn:ds/r1",
            "Datos de precios",
            "distribution",
            Some("urn:ds"),
            "Datos de precios — distribution. CSV.",
        ),
        row(
            "urn:other",
            "Velocidad del viento",
            "dataset",
            None,
            "Velocidad del viento — dataset. Boya.",
        ),
    ];
    // the query vector sits on the distribution
    let vectors = vec![
        unit(&[0.6, 0.8, 0.0]),
        unit(&[0.0, 1.0, 0.1]),
        unit(&[1.0, 0.0, 0.0]),
    ];
    let (_dir, index) = index_from(rows, vectors);
    let q = unit(&[0.0, 1.0, 0.0]);
    let hits = index.rank(Some(&q), "", &no_filter(), FindMode::Vector, 5);
    assert_eq!(index.rows[hits[0].row].iri, "urn:ds");
    assert_eq!(
        hits[0].via.map(|v| index.rows[v].iri.as_str()),
        Some("urn:ds/r1")
    );
    // the dataset itself is not listed twice
    assert_eq!(
        hits.iter()
            .filter(|h| index.rows[h.row].iri == "urn:ds")
            .count(),
        1
    );
    let shown = index.hit_json(&hits[0], "urn:g");
    assert_eq!(shown["via"]["label"], "Datos de precios");
    assert_eq!(shown["via"]["iri"], "urn:ds/r1");
    // asking for distributions gets the distribution itself
    let parts = FindFilter {
        kinds: vec!["distribution".into()],
        ..no_filter()
    };
    let direct = index.rank(Some(&q), "", &parts, FindMode::Vector, 5);
    assert_eq!(index.rows[direct[0].row].iri, "urn:ds/r1");
    assert!(direct[0].via.is_none());
    assert_eq!(shown["says"], "Precios máximos.");
    assert!(shown.get("graph").is_none());
}

#[test]
fn words_rescue_what_meaning_misses_and_filters_hold() {
    let rows = vec![
        row(
            "urn:a",
            "Universidad Nacional de Córdoba",
            "entity",
            None,
            "Universidad Nacional de Córdoba — university. inception: 1613-06-19",
        ),
        row(
            "urn:b",
            "Universidad de la República",
            "entity",
            None,
            "Universidad de la República — university. inception: 1849-07-18",
        ),
        row("urn:c", "Córdoba", "concept", None, "Córdoba — city"),
    ];
    let vectors = vec![
        unit(&[0.7, 0.7, 0.1]),
        unit(&[0.75, 0.66, 0.0]),
        unit(&[0.0, 0.2, 1.0]),
    ];
    let (_dir, index) = index_from(rows, vectors);
    // meaning alone prefers urn:b; the year in the query is a word only urn:a has
    let q = unit(&[0.76, 0.65, 0.0]);
    let vector_only = index.rank(
        Some(&q),
        "universidad fundada en 1613",
        &no_filter(),
        FindMode::Vector,
        3,
    );
    assert_eq!(index.rows[vector_only[0].row].iri, "urn:b");
    let fused = index.rank(
        Some(&q),
        "universidad fundada en 1613",
        &no_filter(),
        FindMode::Fused,
        3,
    );
    assert_eq!(index.rows[fused[0].row].iri, "urn:a");
    // accents fold both ways
    let lexical = index.rank(None, "cordoba", &no_filter(), FindMode::Lexical, 3);
    assert!(lexical.iter().any(|h| index.rows[h.row].iri == "urn:c"));
    // a kind filter applies before ranking
    let concepts = FindFilter {
        kinds: vec!["concept".into()],
        ..no_filter()
    };
    let only = index.rank(Some(&q), "universidad", &concepts, FindMode::Fused, 3);
    assert!(only.iter().all(|h| index.rows[h.row].kind == "concept"));
}

#[test]
fn fusion_is_convex_over_normalized_scores() {
    let fused = fuse(&[(1, 0.9), (2, 0.5)], &[(2, 10.0), (3, 2.0)], 0.7);
    let score = |id: u32| fused.iter().find(|x| x.0 == id).map(|x| x.1).unwrap();
    assert!((score(1) - 0.7).abs() < 1e-6);
    assert!((score(2) - 0.3).abs() < 1e-6);
    assert!(score(3).abs() < 1e-6);
}

#[test]
fn i8_scan_keeps_the_exact_order_on_unit_vectors() {
    let mut rng = 7u64;
    let mut next = || {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        ((rng >> 40) as f32 / (1u64 << 24) as f32) - 0.5
    };
    let vectors: Vec<Vec<f32>> = (0..200)
        .map(|_| unit(&(0..32).map(|_| next()).collect::<Vec<_>>()))
        .collect();
    let rows: Vec<EntityRow> = (0..200)
        .map(|i| row(&format!("urn:{i}"), "x", "entity", None, "x"))
        .collect();
    let query = vectors[17].clone();
    let (_dir, index) = index_from(rows, vectors);
    let hits = index.rank(Some(&query), "", &no_filter(), FindMode::Vector, 1);
    assert_eq!(index.rows[hits[0].row].iri, "urn:17");
}

#[test]
fn lexical_tokens_fold_and_drop_stopwords() {
    assert_eq!(
        lexical_tokens("¿Dónde está el CONSUMO de energía?"),
        vec!["esta", "consumo", "energia"]
    );
}

#[test]
fn config_round_trips_and_disabled_is_inert() {
    let dir = TempDir::new();
    assert!(!entity_index_enabled(dir.path()));
    let config = EntityIndexConfig {
        enabled: true,
        model: String::new(),
        langs: vec!["es".into(), "en".into()],
        documents: false,
    };
    write_entity_index_config(dir.path(), &config).unwrap();
    assert!(entity_index_enabled(dir.path()));
    assert_eq!(
        read_entity_index_config(dir.path()).unwrap().model_id(),
        DEFAULT_ENTITY_MODEL
    );
    write_entity_index_config(
        dir.path(),
        &EntityIndexConfig {
            enabled: false,
            ..config.clone()
        },
    )
    .unwrap();
    assert!(!entity_index_enabled(dir.path()));
    let bad = EntityIndexConfig {
        model: "bge-small".into(),
        ..config
    };
    assert!(write_entity_index_config(dir.path(), &bad).is_err());
}

/// End to end with the real multilingual model: needs ONNX Runtime and the
/// model files (downloaded once into fastembed's cache). Run with
/// `cargo test --features headless --lib entity_index -- --ignored`.
#[test]
#[ignore = "needs ONNX Runtime and the e5-small model files"]
fn refresh_then_find_with_the_real_model() {
    use oxigraph::io::{RdfFormat, RdfParser};
    let store = Store::new().unwrap();
    store
        .load_from_slice(
            RdfParser::from_format(RdfFormat::TriG),
            r#"@prefix dcat: <http://www.w3.org/ns/dcat#> . @prefix dct: <http://purl.org/dc/terms/> .
            GRAPH <urn:sophia:garden:cat:dcat> {
              <urn:catalog> a dcat:Catalog ; dct:rights "Licencia de Datos Abiertos – Uruguay" .
              <urn:ds:fuel> a dcat:Dataset ; dct:title "Precios de combustibles"@es ;
                  dct:description "Precios máximos de nafta y gasoil aprobados por el Poder Ejecutivo."@es ;
                  dct:license <https://creativecommons.org/licenses/by/4.0/> .
              <urn:ds:wind> a dcat:Dataset ; dct:title "Velocidad del viento 2019"@es ;
                  dct:description "Mediciones de velocidad y dirección del viento en la boya."@es .
              <urn:ds:school> a dcat:Dataset ; dct:title "Matrícula de educación media"@es ;
                  dct:description "Estudiantes inscriptos en liceos públicos por departamento."@es .
            }"#
            .as_bytes(),
        )
        .unwrap();
    let dir = TempDir::new();
    write_entity_index_config(
        dir.path(),
        &EntityIndexConfig {
            enabled: true,
            model: String::new(),
            langs: vec!["es".into(), "en".into()],
            documents: false,
        },
    )
    .unwrap();
    let report = refresh_entity_index(
        "cat",
        dir.path(),
        &store,
        &RefreshOptions {
            threads: Some(2),
            batch: None,
        },
        &mut |_, _, _| Ok(()),
    )
    .expect("refresh");
    // the three datasets and the catalog (a schema-typed thing, named by its local name)
    assert_eq!((report.entities, report.embedded, report.reused), (4, 4, 0));
    // a second refresh embeds nothing
    let again = refresh_entity_index(
        "cat",
        dir.path(),
        &store,
        &RefreshOptions {
            threads: Some(2),
            batch: None,
        },
        &mut |_, _, _| Ok(()),
    )
    .expect("refresh again");
    assert_eq!((again.embedded, again.reused), (0, 4));
    let index = load_entity_index(dir.path()).unwrap();
    for (query, expected) in [
        ("how much does petrol cost at the pump", "urn:ds:fuel"),
        ("wind measurements", "urn:ds:wind"),
        ("students enrolled in high school", "urn:ds:school"),
    ] {
        let q = with_entity_embedder(DEFAULT_ENTITY_MODEL, None, |e| e.embed_query(query)).unwrap();
        let hits = index.rank(Some(&q), query, &no_filter(), FindMode::Fused, 3);
        assert_eq!(index.rows[hits[0].row].iri, expected, "{query}");
    }
    let response = crate::entity_find::find_response(
        "cat",
        "nafta",
        &index,
        &index.rank(None, "nafta", &no_filter(), FindMode::Lexical, 3),
        1,
    );
    assert_eq!(
        response["provenance"][0]["rights"],
        "Licencia de Datos Abiertos – Uruguay"
    );
    assert_eq!(
        response["hits"][0]["license"],
        "https://creativecommons.org/licenses/by/4.0/"
    );
}

#[test]
fn provenance_is_what_a_graph_says_of_itself_and_licences_stay_with_their_datasets() {
    use oxigraph::io::{RdfFormat, RdfParser};
    let store = Store::new().unwrap();
    store
        .load_from_slice(
            RdfParser::from_format(RdfFormat::TriG),
            r#"@prefix dcat: <http://www.w3.org/ns/dcat#> . @prefix dct: <http://purl.org/dc/terms/> .
            GRAPH <urn:g:dcat> {
              <urn:ds:a> a dcat:Dataset ; dct:license <https://creativecommons.org/licenses/by-nc/4.0/> .
              <urn:ds:b> a dcat:Dataset ; dct:rights "Licencia de Datos Abiertos – Uruguay" .
            }
            GRAPH <urn:g:prov> { <urn:g:prov> dct:rights "harvested by Sophia Labs" . }
            GRAPH <urn:g:cat> { <urn:cat> a dcat:Catalog ; dct:license <urn:odc-uy> . <urn:ds:c> dct:license <urn:x> . }"#
                .as_bytes(),
        )
        .unwrap();
    // one dataset's licence is not its graph's
    let dcat = graph_provenance(&store, "urn:g:dcat");
    assert_eq!((dcat.rights, dcat.license), (None, None));
    assert_eq!(
        graph_provenance(&store, "urn:g:prov").rights.as_deref(),
        Some("harvested by Sophia Labs")
    );
    assert_eq!(
        graph_provenance(&store, "urn:g:cat").license.as_deref(),
        Some("urn:odc-uy")
    );
    assert_eq!(
        entity_license(&store, "urn:ds:a").as_deref(),
        Some("https://creativecommons.org/licenses/by-nc/4.0/")
    );
    assert_eq!(
        entity_license(&store, "urn:ds:b").as_deref(),
        Some("Licencia de Datos Abiertos – Uruguay")
    );
    assert_eq!(entity_license(&store, "urn:nothing"), None);
}
