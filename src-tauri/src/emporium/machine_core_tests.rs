//! Contract-level proof for the lightweight Sophia machine ontology.
//!
//! These tests deliberately exercise the real Emporium SHACL compiler and
//! validator. The topology Turtle is a logical Cloud-2 fixture; live
//! `MachineRun` testimony is event-derived and therefore tested separately.

use crate::emporium::{
    contract::get_vocabulary,
    shacl_validator::{compile_check_contract, validate_desired},
    terms::{Term, Triple},
};
use oxigraph::{
    io::{RdfFormat, RdfParser},
    model::{Literal, NamedNode},
    store::Store,
};

const MACH: &str = "http://mnemosyne.dev/machine#";
const PROV: &str = "http://www.w3.org/ns/prov#";
const RDF_TYPE: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#type";
const XSD: &str = "http://www.w3.org/2001/XMLSchema#";

fn uri(value: &str) -> Term {
    Term::Uri(NamedNode::new(value).expect("fixture IRI"))
}

fn string(value: &str) -> Term {
    Term::Lit(Literal::new_simple_literal(value))
}

fn date_time(value: &str) -> Term {
    Term::Lit(Literal::new_typed_literal(
        value,
        NamedNode::new(format!("{XSD}dateTime")).expect("xsd:dateTime"),
    ))
}

fn triple(subject: &str, predicate: &str, object: Term) -> Triple {
    (subject.to_string(), predicate.to_string(), object)
}

fn conforming_five_class_fixture() -> Vec<Triple> {
    let definition = "urn:sophia:machine-definition:garden-cell:0.1.0";
    let machine = "urn:sophia:machine:cloud-2-canary:garden-cell:lab";
    let run = "urn:sophia:machine-run:cloud-2-canary:run-01";
    let port = "urn:sophia:machine:cloud-2-canary:garden-cell:lab:port:graph-api";
    let binding = "urn:sophia:machine-binding:gateway-to-lab";

    vec![
        triple(
            definition,
            RDF_TYPE,
            uri(&format!("{MACH}MachineDefinition")),
        ),
        triple(
            definition,
            &format!("{MACH}definitionId"),
            string("garden-cell"),
        ),
        triple(definition, &format!("{MACH}version"), string("0.1.0")),
        triple(
            definition,
            &format!("{MACH}machineKind"),
            uri(&format!("{MACH}GardenCell")),
        ),
        triple(
            definition,
            &format!("{MACH}executionMode"),
            uri(&format!("{MACH}KubernetesPod")),
        ),
        triple(
            definition,
            &format!("{MACH}cardinality"),
            uri(&format!("{MACH}OnePerGraph")),
        ),
        triple(
            definition,
            &format!("{MACH}lifecycleStatus"),
            uri(&format!("{MACH}Active")),
        ),
        triple(machine, RDF_TYPE, uri(&format!("{MACH}Machine"))),
        triple(
            machine,
            &format!("{MACH}machineId"),
            string("garden-cell:lab"),
        ),
        triple(machine, &format!("{MACH}conformsTo"), uri(definition)),
        triple(
            machine,
            &format!("{MACH}environment"),
            uri("urn:sophia:environment:cloud-2-canary"),
        ),
        triple(
            machine,
            &format!("{MACH}lifecycleStatus"),
            uri(&format!("{MACH}OnDemand")),
        ),
        triple(run, RDF_TYPE, uri(&format!("{MACH}MachineRun"))),
        triple(run, RDF_TYPE, uri(&format!("{PROV}Activity"))),
        triple(run, &format!("{MACH}runId"), string("run-01")),
        triple(run, &format!("{MACH}actualizes"), uri(machine)),
        triple(
            run,
            &format!("{MACH}runState"),
            uri(&format!("{MACH}Running")),
        ),
        triple(
            run,
            &format!("{PROV}startedAtTime"),
            date_time("2026-07-13T12:00:00Z"),
        ),
        triple(port, RDF_TYPE, uri(&format!("{MACH}Port"))),
        triple(
            port,
            &format!("{MACH}portId"),
            string("garden-cell:lab:graph-api"),
        ),
        triple(port, &format!("{MACH}portOf"), uri(machine)),
        triple(port, &format!("{MACH}portName"), string("graph-api")),
        triple(
            port,
            &format!("{MACH}direction"),
            uri(&format!("{MACH}Inbound")),
        ),
        triple(
            port,
            &format!("{MACH}protocol"),
            uri(&format!("{MACH}HttpAndWebSocket")),
        ),
        triple(binding, RDF_TYPE, uri(&format!("{MACH}Binding"))),
        triple(
            binding,
            &format!("{MACH}bindingId"),
            string("gateway-to-lab"),
        ),
        triple(
            binding,
            &format!("{MACH}sourcePort"),
            uri("urn:sophia:machine:cloud-2-canary:gateway:port:cell-upstream"),
        ),
        triple(binding, &format!("{MACH}destinationPort"), uri(port)),
        triple(
            binding,
            &format!("{MACH}deliverySemantics"),
            uri(&format!("{MACH}SynchronousRequestResponse")),
        ),
        triple(
            binding,
            &format!("{MACH}failureBehavior"),
            uri(&format!("{MACH}SurfaceUpstreamFailure")),
        ),
        triple(
            binding,
            &format!("{MACH}lifecycleStatus"),
            uri(&format!("{MACH}Active")),
        ),
    ]
}

#[test]
fn machine_core_is_five_classes_and_not_a_public_pillar() {
    let contract = get_vocabulary("sophia-machine-core").expect("machine core is registered");
    assert_eq!(contract.version, "0.1.0");
    assert_eq!(contract.primary_namespace(), MACH);
    assert_eq!(contract.classes.len(), 5);
    assert_eq!(
        contract
            .classes
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        [
            "Binding",
            "Machine",
            "MachineDefinition",
            "MachineRun",
            "Port"
        ]
    );

    let alias = get_vocabulary("machine").expect("machine alias resolves");
    assert_eq!(alias.name, "sophia-machine-core");
}

#[test]
fn machine_core_shapes_compile_and_conforming_fixture_passes() {
    let contract = get_vocabulary("sophia-machine-core").expect("machine core");
    let shapes = compile_check_contract(contract).expect("machine-core SHACL compiles");
    assert_eq!(
        shapes.matches("a sh:NodeShape").count(),
        5,
        "one closed NodeShape per machine-core class"
    );
    validate_desired(&conforming_five_class_fixture(), contract)
        .expect("one conforming instance of every class passes the real validator");
}

#[test]
fn machine_run_without_logical_machine_link_is_rejected() {
    let contract = get_vocabulary("sophia-machine-core").expect("machine core");
    let actualizes = format!("{MACH}actualizes");
    let malformed = conforming_five_class_fixture()
        .into_iter()
        .filter(|(_, predicate, _)| predicate != &actualizes)
        .collect::<Vec<_>>();
    let error =
        validate_desired(&malformed, contract).expect_err("MachineRun requires mach:actualizes");
    assert!(
        error.starts_with("SHACL:"),
        "validator fails loudly: {error}"
    );
}

#[test]
fn cloud2_catalog_fixture_is_real_turtle() {
    let ttl = include_str!("vocabs/sophia-machine-core.cloud2-canary.ttl");
    let store = Store::new().expect("temporary Oxigraph store");
    store
        .load_from_slice(RdfParser::from_format(RdfFormat::Turtle), ttl.as_bytes())
        .expect("Cloud-2 machine catalog parses as Turtle");
    assert!(
        store.len().expect("catalog size") >= 100,
        "fixture contains definitions, logical machines, ports, and bindings"
    );
}
