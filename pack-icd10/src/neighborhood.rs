//! `icd10.neighborhood(code)` — parent/children/siblings/excludes/index terms.

use std::collections::HashMap;

use khive_runtime::{EdgeListFilter, KhiveRuntime, NamespaceToken, RuntimeError};
use khive_storage::{Entity, EntityFilter};
use khive_types::EdgeRelation;
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct NeighborhoodParams {
    code: String,
}

pub async fn handle_neighborhood(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    params: Value,
) -> Result<Value, RuntimeError> {
    let params: NeighborhoodParams = serde_json::from_value(params).map_err(|e| {
        RuntimeError::InvalidInput(format!("invalid icd10.neighborhood arguments: {e}"))
    })?;
    let code = params.code.trim().to_string();

    let self_entity = find_by_code(runtime, token, &code).await?.ok_or_else(|| {
        RuntimeError::InvalidInput(format!("no icd_code or icd_category entity named {code:?}"))
    })?;
    let this_id = self_entity.id;

    let incoming = crate::memo::list_edges(
        runtime,
        token,
        EdgeListFilter {
            target_id: Some(this_id),
            relations: vec![EdgeRelation::Contains],
            ..Default::default()
        },
        10,
        0,
    )
    .await?;
    let parent_id = incoming.first().map(|e| e.source_id);

    let outgoing = crate::memo::list_edges(
        runtime,
        token,
        EdgeListFilter {
            source_id: Some(this_id),
            relations: vec![EdgeRelation::Contains],
            ..Default::default()
        },
        1000,
        0,
    )
    .await?;
    let child_ids: Vec<Uuid> = outgoing.iter().map(|e| e.target_id).collect();

    let mut sibling_ids: Vec<Uuid> = Vec::new();
    if let Some(pid) = parent_id {
        let sibling_edges = crate::memo::list_edges(
            runtime,
            token,
            EdgeListFilter {
                source_id: Some(pid),
                relations: vec![EdgeRelation::Contains],
                ..Default::default()
            },
            1000,
            0,
        )
        .await?;
        sibling_ids = sibling_edges
            .iter()
            .map(|e| e.target_id)
            .filter(|&id| id != this_id)
            .collect();
    }

    let excludes1 = partners_by_role(
        runtime,
        token,
        this_id,
        EdgeRelation::CompetesWith,
        "excludes1",
    )
    .await?;
    let excludes2 = partners_by_role(
        runtime,
        token,
        this_id,
        EdgeRelation::ComposedWith,
        "excludes2",
    )
    .await?;
    let code_also = partners_by_role(
        runtime,
        token,
        this_id,
        EdgeRelation::ComposedWith,
        "code_also",
    )
    .await?;
    let code_first = partners_by_role(
        runtime,
        token,
        this_id,
        EdgeRelation::Precedes,
        "code_first",
    )
    .await?;
    let use_additional = partners_by_role(
        runtime,
        token,
        this_id,
        EdgeRelation::Precedes,
        "use_additional",
    )
    .await?;

    let idx_edges = crate::memo::list_edges(
        runtime,
        token,
        EdgeListFilter {
            target_id: Some(this_id),
            relations: vec![EdgeRelation::VariantOf],
            ..Default::default()
        },
        25,
        0,
    )
    .await?;
    let term_ids: Vec<Uuid> = idx_edges.iter().map(|e| e.source_id).take(25).collect();

    let mut all_ids: Vec<Uuid> = vec![this_id];
    all_ids.extend(parent_id);
    all_ids.extend(child_ids.iter().copied());
    all_ids.extend(sibling_ids.iter().copied());
    all_ids.extend(excludes1.iter().map(|(id, _)| *id));
    all_ids.extend(excludes2.iter().map(|(id, _)| *id));
    all_ids.extend(code_also.iter().map(|(id, _)| *id));
    all_ids.extend(code_first.iter().map(|(id, _)| *id));
    all_ids.extend(use_additional.iter().map(|(id, _)| *id));
    all_ids.extend(term_ids.iter().copied());
    all_ids.sort();
    all_ids.dedup();

    let entities = crate::memo::entities_by_ids(runtime, token, &all_ids).await?;
    let by_id: HashMap<Uuid, Entity> = entities.into_iter().map(|e| (e.id, e)).collect();

    let render = |id: &Uuid| -> Value {
        by_id
            .get(id)
            .map(entity_summary)
            .unwrap_or_else(|| json!({"id": id}))
    };

    let siblings_json: Vec<Value> = sibling_ids
        .iter()
        .map(|id| {
            let mut v = render(id);
            if let (Some(a), Some(b)) = (by_id.get(&this_id), by_id.get(id)) {
                v["differs_on"] = json!(axis_diff(a, b));
            }
            v
        })
        .collect();

    Ok(json!({
        "code": code,
        "record": entity_summary(&self_entity),
        "parent": parent_id.map(|id| render(&id)),
        "children": child_ids.iter().map(render).collect::<Vec<_>>(),
        "siblings": siblings_json,
        "excludes1": zip_partners(&excludes1, &by_id),
        "excludes2": zip_partners(&excludes2, &by_id),
        "code_also": zip_partners(&code_also, &by_id),
        "code_first": zip_partners(&code_first, &by_id),
        "use_additional": zip_partners(&use_additional, &by_id),
        "index_terms": term_ids.iter().map(render).collect::<Vec<_>>(),
    }))
}

pub(crate) async fn find_by_code(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    code: &str,
) -> Result<Option<Entity>, RuntimeError> {
    for entity_type in ["icd_code", "icd_category"] {
        let filter = EntityFilter {
            entity_types: vec![entity_type.to_string()],
            name_exact: Some(code.to_string()),
            ..Default::default()
        };
        let mut hits = crate::memo::list_entities_filtered(runtime, token, filter, 1, 0).await?;
        if let Some(e) = hits.pop() {
            return Ok(Some(e));
        }
    }
    Ok(None)
}

pub(crate) async fn partners_by_role(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    this_id: Uuid,
    relation: EdgeRelation,
    role: &str,
) -> Result<Vec<(Uuid, Value)>, RuntimeError> {
    let outgoing = EdgeListFilter {
        source_id: Some(this_id),
        relations: vec![relation],
        ..Default::default()
    };
    let incoming = EdgeListFilter {
        target_id: Some(this_id),
        relations: vec![relation],
        ..Default::default()
    };
    // Code-first and use-additional are instructions carried by one code, and `ingest` stores
    // them in a fixed direction: a code-first note on X as referenced -> X, a use-additional
    // note on X as X -> referenced. Read from the other end, the same edge would hand X's
    // instruction to the code it names. Excludes and code-also are read from either end.
    let filters = match role {
        "code_first" => vec![incoming],
        "use_additional" => vec![outgoing],
        _ => vec![outgoing, incoming],
    };
    let mut out: Vec<(Uuid, Value)> = Vec::new();
    for filter in filters {
        let edges = crate::memo::list_edges(runtime, token, filter, 1000, 0).await?;
        for e in edges {
            let has_role = e
                .metadata
                .as_ref()
                .and_then(|m| m.get("icd"))
                .and_then(|v| v.as_array())
                .map(|arr| arr.iter().any(|x| x.as_str() == Some(role)))
                .unwrap_or(false);
            if !has_role {
                continue;
            }
            let partner = if e.source_id == this_id {
                e.target_id
            } else {
                e.source_id
            };
            out.push((partner, e.metadata.clone().unwrap_or(Value::Null)));
        }
    }
    out.sort_by_key(|(id, _)| *id);
    out.dedup_by_key(|(id, _)| *id);
    Ok(out)
}

/// `partners_by_role`'s partner ids, rendered as codes — shared by
/// `icd10.resolve`'s candidate `code_first`/`use_additional` fields.
pub(crate) async fn partner_codes(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    this_id: Uuid,
    relation: EdgeRelation,
    role: &str,
) -> Result<Vec<String>, RuntimeError> {
    let partners = partners_by_role(runtime, token, this_id, relation, role).await?;
    if partners.is_empty() {
        return Ok(Vec::new());
    }
    let ids: Vec<Uuid> = partners.into_iter().map(|(id, _)| id).collect();
    let entities = crate::memo::entities_by_ids(runtime, token, &ids).await?;
    Ok(entities.into_iter().map(|e| e.name).collect())
}

fn zip_partners(partners: &[(Uuid, Value)], by_id: &HashMap<Uuid, Entity>) -> Vec<Value> {
    partners
        .iter()
        .map(|(id, meta)| {
            let mut v = by_id
                .get(id)
                .map(entity_summary)
                .unwrap_or_else(|| json!({"id": id}));
            v["relation_metadata"] = meta.clone();
            v
        })
        .collect()
}

fn entity_summary(e: &Entity) -> Value {
    json!({
        "id": e.id,
        "name": e.name,
        "entity_type": e.entity_type,
        "properties": e.properties,
    })
}

fn axis_diff(a: &Entity, b: &Entity) -> &'static str {
    let get = |e: &Entity, k: &str| e.properties.as_ref().and_then(|v| v.get(k)).cloned();
    if get(a, "laterality") != get(b, "laterality") {
        return "laterality";
    }
    if get(a, "specificity") != get(b, "specificity") {
        return "specificity";
    }
    if get(a, "requires_7th") != get(b, "requires_7th") {
        return "7th_character";
    }
    "other"
}
