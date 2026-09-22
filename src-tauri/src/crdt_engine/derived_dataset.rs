//! Verify explicitly declared compatibility edits against immutable dataset custody.
use super::*;

fn quoted_id(value: &str) -> String {
    value.bytes().map(|b| if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
        (b as char).to_string()
    } else {format!("%{b:02X}")}).collect()
}

pub(super) fn verify_derived_dataset(raw: &[u8], semantic: &[u8], members: &Members,
    manifest: &Value, user: &str, graph: &str) -> Result<(), String> {
    let rule=&manifest["compatibilityTransform"];
    require(rule["rule"]=="vera-20260920-standard-compatibility-v1"
        && rule["originalCustodyUnchanged"]==true, "derived dataset compatibility declaration")?;
    let main=format!("urn:mnemosyne:user:{user}:graph:{graph}");
    let wire_prefix=format!("{main}:wire:");
    let workspace=parsed_doc(member(members,"crdt/workspace.yjs")?,"derived workspace")?;
    let saved=roots(&workspace,"wires")?;
    let mut mapping=BTreeMap::new();let mut targets=BTreeSet::new();
    for (old,new) in object(rule,"identifierMap")? {
        let new=new.as_str().ok_or("derived identifier value")?;source_id(new)?;
        require(targets.insert(new.to_string()),"derived identifier collision")?;
        for kind in ["doc","folder","artifact","wire"] {
            for spelling in [old.to_string(),quoted_id(old)] {
                mapping.insert(format!("{main}:{kind}:{spelling}"),format!("{main}:{kind}:{new}"));
            }
        }
    }
    let rewrite=|node:NamedNode| -> Result<NamedNode,String> {
        let value=node.as_str();
        let (base,fragment)=value.split_once('#').map(|(a,b)|(a,format!("#{b}"))).unwrap_or((value,String::new()));
        if let Some(mapped)=mapping.get(base) {NamedNode::new(format!("{mapped}{fragment}")).map_err(|e|e.to_string())}
        else {Ok(node)}
    };
    let fingerprint=|quad:&oxigraph::model::Quad|archive_sha256(format!("{quad} .\n").as_bytes());
    let mut expected=std::collections::HashSet::new();
    let mut fields=BTreeMap::<(String,String),Vec<Term>>::new();
    for quad in RdfParser::from_format(RdfFormat::NQuads).for_slice(raw) {
        let mut quad=quad.map_err(|e|e.to_string())?;
        if let NamedOrBlankNode::NamedNode(node)=quad.subject {quad.subject=rewrite(node)?.into();}
        quad.predicate=rewrite(quad.predicate)?;
        if let Term::NamedNode(node)=quad.object {quad.object=rewrite(node)?.into();}
        require(expected.insert(fingerprint(&quad)),"derived dataset input duplicate")?;
        if let NamedOrBlankNode::NamedNode(node)=&quad.subject {
            if node.as_str().starts_with(&wire_prefix) && matches!(&quad.graph_name,GraphName::NamedNode(g) if g.as_str()==main) {
                fields.entry((node.as_str().to_string(),quad.predicate.as_str().to_string())).or_default().push(quad.object);
            }
        }
    }
    for row in array(rule,"wireMetadataDefaults")? {
        let subject=string(row,"subject")?;let predicate=string(row,"predicate")?;
        let id=subject.strip_prefix(&wire_prefix).ok_or("derived wire scope")?;
        source_id(id)?;require(!saved.contains_key(id),"derived metadata cannot modify current wire")?;
        require(row["referenceOnly"]==true && row["currentEntityExistenceAsserted"]==false,"derived metadata authority")?;
        let statement=format!("<{subject}> <{predicate}> {} <{main}> .\n",string(row,"value")?);
        let quads=RdfParser::from_format(RdfFormat::NQuads).for_slice(statement.as_bytes())
            .collect::<Result<Vec<_>,_>>().map_err(|e|e.to_string())?;
        require(quads.len()==1,"derived metadata statement count")?;
        let quad=&quads[0];let key=(subject.to_string(),predicate.to_string());
        require(quad.subject==NamedNode::new(subject).map_err(|e|e.to_string())?.into()
            && quad.predicate.as_str()==predicate && matches!(&quad.graph_name,GraphName::NamedNode(g) if g.as_str()==main),"derived metadata statement scope")?;
        if row["rule"]=="latest-wire-snapshot-timestamp-v1" {
            require(predicate==format!("{}snapshotAt",crate::runtime_config::WIRE_NS),"derived timestamp field")?;
            let prior=fields.get(&key).ok_or("derived timestamp source absent")?;
            let mut actual:Vec<_>=prior.iter().map(ToString::to_string).collect();actual.sort();
            let declared=array(row,"sourceValues")?.iter().map(|v|v.as_str().map(str::to_string).ok_or("derived timestamp testimony"))
                .collect::<Result<Vec<_>,_>>()?;
            require(actual==declared && prior.len()>1 && row["removedQuadCount"]==json!(prior.len()-1),"derived timestamp testimony mismatch")?;
            let times=prior.iter().map(|v|match v {Term::Literal(lit)=>chrono::DateTime::parse_from_rfc3339(lit.value()).map_err(|e|e.to_string()),_=>Err("derived timestamp type".into())}).collect::<Result<Vec<_>,_>>()?;
            let newest=times.iter().enumerate().max_by_key(|(_,v)|*v).unwrap().0;
            require(prior[newest]==quad.object,"derived timestamp is not newest")?;
            for value in prior {if value!=&quad.object {let mut removed=quad.clone();removed.object=value.clone();require(expected.remove(&fingerprint(&removed)),"derived timestamp missing source")?;}}
        } else {
            require(row["rule"]=="legacy-wire-metadata-default-v1" && row["sourceValue"].is_null()
                && !fields.contains_key(&key),"derived metadata must fill an absent field")?;
            let ns=crate::runtime_config::WIRE_NS;
            let permitted=if predicate==crate::runtime_config::RDF_TYPE {quad.object==NamedNode::new(format!("{ns}Wire")).unwrap().into()}
                else if predicate==format!("{ns}predicate") {quad.object==NamedNode::new("http://www.w3.org/2000/01/rdf-schema#seeAlso").unwrap().into()}
                else if predicate==format!("{ns}bidirectional") {quad.object==oxigraph::model::Literal::from(false).into()}
                else if predicate==format!("{ns}targetGraph") {matches!(&quad.object,Term::Literal(lit) if source_id(lit.value()).is_ok())}
                else if [format!("{ns}sourceDocument"),format!("{ns}targetDocument")].contains(&predicate.to_string()) {
                    matches!(&quad.object,Term::NamedNode(node) if node.as_str().starts_with(&format!("urn:mnemosyne:user:{user}:graph:")) && node.as_str().contains(":doc:"))
                } else {false};
            require(permitted,"unsupported derived metadata default")?;
            require(expected.insert(fingerprint(quad)),"duplicate derived metadata default")?;
        }
        fields.insert(key,vec![quad.object.clone()]);
    }
    for quad in RdfParser::from_format(RdfFormat::NQuads).for_slice(semantic) {
        let quad=quad.map_err(|e|e.to_string())?;
        require(expected.remove(&fingerprint(&quad)),"undeclared or duplicate derived dataset statement")?;
    }
    require(expected.is_empty(),"derived dataset omitted source statements")
}
