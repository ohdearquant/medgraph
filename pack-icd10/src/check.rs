//! `icd10.check(codes, sex?, age_years?)` — the rule checker over the ICD-10-CM
//! graph. It reads the khive graph and applies rules R1-R7 (unknown code,
//! non-billable repair, ancestor redundancy, excludes1 conflict, sex, age,
//! missing 7th character), answering each code with drop / replace / keep.

use std::collections::{HashMap, HashSet};

use khive_runtime::{EdgeListFilter, KhiveRuntime, NamespaceToken, RuntimeError};
use khive_types::EdgeRelation;
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

use crate::neighborhood::find_by_code;
use crate::refs::normalize_code;

/// Sex-specific code-range prefixes: a small, auditable table, not the
/// exhaustive CMS sex-edit file.
const FEMALE_PREFIXES: &[&str] = &[
    "O", "N70", "N71", "N72", "N73", "N75", "N76", "N80", "N81", "N83", "N84", "N85", "N86", "N87",
    "N88", "N89", "N90", "N91", "N92", "N93", "N94", "N95", "N96", "N97", "N98", "Z34", "Z3A",
    "Z33", "Z32", "Z39", "C53", "C54", "C55", "C56", "C57", "C58", "D25", "D26", "D27", "D28",
];
const MALE_PREFIXES: &[&str] = &[
    "N40", "N41", "N42", "N43", "N44", "N45", "N46", "N47", "N48", "N49", "N50", "N51", "N52",
    "N53", "C60", "C61", "C62", "C63", "D29", "Z12.5",
];

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CheckParams {
    codes: Vec<String>,
    sex: Option<String>,
    age_years: Option<f64>,
}

pub async fn handle_check(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    params: Value,
) -> Result<Value, RuntimeError> {
    let params: CheckParams = serde_json::from_value(params)
        .map_err(|e| RuntimeError::InvalidInput(format!("invalid icd10.check arguments: {e}")))?;

    let sex_first = params
        .sex
        .as_deref()
        .map(|s| s.trim().to_lowercase())
        .and_then(|s| s.chars().next());

    let mut verdicts: Vec<Value> = Vec::new();
    let mut kept_ids: Vec<(String, Uuid)> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();

    for raw in &params.codes {
        let code = normalize_code(raw);
        if !seen.insert(code.clone()) {
            continue;
        }
        let Some(entity) = find_by_code(runtime, token, &code).await? else {
            verdicts.push(verdict(&code, "drop", "R1 unknown", "", None));
            continue;
        };
        let billable = prop_bool(&entity, "billable");
        let requires_7th = prop_bool(&entity, "requires_7th");

        if !billable {
            if requires_7th {
                // Same repair R2 offers a non-billable category/code: if there
                // is exactly one billable "unspecified" child, replace rather
                // than drop outright, keeping the R7 label so the caller
                // still sees "this needed a 7th character" rather than a
                // generic "not billable".
                match unique_unspecified_child(runtime, token, entity.id).await? {
                    Some((child_code, child_id)) => {
                        verdicts.push(verdict(
                            &code,
                            "replace",
                            "R7 missing_7th_character",
                            &format!("-> {child_code}"),
                            Some(child_code.clone()),
                        ));
                        kept_ids.push((child_code, child_id));
                    }
                    None => {
                        verdicts.push(verdict(
                            &code,
                            "drop",
                            "R7 missing_7th_character",
                            "append a 7th character to designate the encounter",
                            None,
                        ));
                    }
                }
                continue;
            }
            match unique_unspecified_child(runtime, token, entity.id).await? {
                Some((child_code, child_id)) => {
                    verdicts.push(verdict(
                        &code,
                        "replace",
                        "R2 not_billable",
                        &format!("-> {child_code}"),
                        Some(child_code.clone()),
                    ));
                    kept_ids.push((child_code, child_id));
                }
                None => {
                    verdicts.push(verdict(
                        &code,
                        "drop",
                        "R2 not_billable",
                        "pick a billable child",
                        None,
                    ));
                }
            }
            continue;
        }

        if let Some(reason) = sex_conflict(&code, sex_first) {
            verdicts.push(verdict(&code, "drop", "R5 sex", reason, None));
            continue;
        }
        if let Some(reason) = age_conflict(&code, params.age_years) {
            verdicts.push(verdict(&code, "drop", "R6 age", reason, None));
            continue;
        }
        kept_ids.push((code, entity.id));
    }

    // Ancestors of every surviving code (for R3 and R4's family sets).
    let mut anc_map: HashMap<Uuid, HashSet<Uuid>> = HashMap::new();
    for (_, id) in &kept_ids {
        if let std::collections::hash_map::Entry::Vacant(v) = anc_map.entry(*id) {
            let a = ancestors(runtime, token, *id).await?;
            v.insert(a);
        }
    }

    // R3: drop a code that is an ancestor of another surviving code.
    let mut after_r3: Vec<(String, Uuid)> = Vec::new();
    for (code, id) in &kept_ids {
        let is_ancestor = kept_ids.iter().any(|(_, other_id)| {
            other_id != id
                && anc_map
                    .get(other_id)
                    .map(|s| s.contains(id))
                    .unwrap_or(false)
        });
        if is_ancestor {
            verdicts.push(verdict(
                code,
                "drop",
                "R3 ancestor",
                "a more specific predicted code exists",
                None,
            ));
        } else {
            after_r3.push((code.clone(), *id));
        }
    }

    // Excludes1 neighbor set per id touched by R4 (self + ancestors).
    let mut excl_map: HashMap<Uuid, HashSet<Uuid>> = HashMap::new();
    let mut touched: HashSet<Uuid> = HashSet::new();
    for (_, id) in &after_r3 {
        touched.insert(*id);
        touched.extend(anc_map.get(id).cloned().unwrap_or_default());
    }
    for id in &touched {
        excl_map.insert(*id, excludes1_neighbors(runtime, token, *id).await?);
    }

    // R4: excludes1 conflicts among survivors — the later code loses.
    let mut final_list: Vec<(String, Uuid)> = Vec::new();
    let mut pairs: Vec<(String, String)> = Vec::new();
    for (code, id) in &after_r3 {
        let family_this: HashSet<Uuid> = std::iter::once(*id)
            .chain(anc_map.get(id).cloned().unwrap_or_default())
            .collect();
        let mut conflict: Option<String> = None;
        for (fcode, fid) in &final_list {
            let family_f: HashSet<Uuid> = std::iter::once(*fid)
                .chain(anc_map.get(fid).cloned().unwrap_or_default())
                .collect();
            let hit = family_this.iter().any(|x| {
                excl_map
                    .get(x)
                    .map(|n| family_f.iter().any(|y| n.contains(y)))
                    .unwrap_or(false)
            });
            if hit {
                conflict = Some(fcode.clone());
                break;
            }
        }
        match conflict {
            Some(prev) => {
                pairs.push((prev.clone(), code.clone()));
                verdicts.push(verdict(
                    code,
                    "drop",
                    "R4 excludes1",
                    &format!("conflicts with {prev}"),
                    None,
                ));
            }
            None => {
                final_list.push((code.clone(), *id));
                verdicts.push(verdict(code, "keep", "ok", "", None));
            }
        }
    }

    Ok(json!({
        "kept": final_list.iter().map(|(c, _)| c.clone()).collect::<Vec<_>>(),
        "verdicts": verdicts,
        "excludes1_pairs": pairs,
    }))
}

/// Shared with `icd10.exclude`'s E2: true male/female-prefix
/// conflict check, same table `check.rs` has always used. Reused rather than
/// copied so R5 and E2 can never drift apart.
pub(crate) fn sex_conflict(code: &str, sex_first: Option<char>) -> Option<&'static str> {
    if sex_first == Some('m') && starts_with_any(code, FEMALE_PREFIXES) {
        Some("female-only code, male patient")
    } else if sex_first == Some('f') && starts_with_any(code, MALE_PREFIXES) {
        Some("male-only code, female patient")
    } else {
        None
    }
}

/// Shared with `icd10.exclude`'s E3: same perinatal/newborn rule
/// R6 has always used.
pub(crate) fn age_conflict(code: &str, age_years: Option<f64>) -> Option<&'static str> {
    let age = age_years?;
    if age >= 1.0 && (code.starts_with('P') || code.starts_with("Z38")) {
        Some("perinatal/newborn code, patient over 1 year")
    } else {
        None
    }
}

fn verdict(
    code: &str,
    action: &str,
    rule: &str,
    detail: &str,
    replacement: Option<String>,
) -> Value {
    json!({
        "code": code,
        "action": action,
        "rule": rule,
        "detail": detail,
        "replacement": replacement,
    })
}

pub(crate) fn prop_bool(e: &khive_storage::Entity, key: &str) -> bool {
    e.properties
        .as_ref()
        .and_then(|p| p.get(key))
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

fn starts_with_any(code: &str, prefixes: &[&str]) -> bool {
    prefixes.iter().any(|p| code.starts_with(p))
}

pub(crate) async fn ancestors(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    id: Uuid,
) -> Result<HashSet<Uuid>, RuntimeError> {
    let mut out = HashSet::new();
    let mut current = id;
    for _ in 0..12 {
        let incoming = crate::memo::list_edges(
            runtime,
            token,
            EdgeListFilter {
                target_id: Some(current),
                relations: vec![EdgeRelation::Contains],
                ..Default::default()
            },
            5,
            0,
        )
        .await?;
        let Some(parent) = incoming.first().map(|e| e.source_id) else {
            break;
        };
        if !out.insert(parent) {
            break;
        }
        current = parent;
    }
    Ok(out)
}

pub(crate) async fn unique_unspecified_child(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    id: Uuid,
) -> Result<Option<(String, Uuid)>, RuntimeError> {
    let children_edges = crate::memo::list_edges(
        runtime,
        token,
        EdgeListFilter {
            source_id: Some(id),
            relations: vec![EdgeRelation::Contains],
            ..Default::default()
        },
        1000,
        0,
    )
    .await?;
    let child_ids: Vec<Uuid> = children_edges.iter().map(|e| e.target_id).collect();
    if child_ids.is_empty() {
        return Ok(None);
    }
    let entities = crate::memo::entities_by_ids(runtime, token, &child_ids).await?;
    let mut billable_unspecified: Vec<(String, Uuid)> = Vec::new();
    for e in &entities {
        let billable = prop_bool(e, "billable");
        let desc = e
            .properties
            .as_ref()
            .and_then(|p| p.get("desc"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_lowercase();
        if billable && desc.contains("unspecified") {
            billable_unspecified.push((e.name.clone(), e.id));
        }
    }
    if billable_unspecified.len() == 1 {
        Ok(billable_unspecified.into_iter().next())
    } else {
        Ok(None)
    }
}

pub(crate) async fn excludes1_neighbors(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    id: Uuid,
) -> Result<HashSet<Uuid>, RuntimeError> {
    let mut out = HashSet::new();
    for filter in [
        EdgeListFilter {
            source_id: Some(id),
            relations: vec![EdgeRelation::CompetesWith],
            ..Default::default()
        },
        EdgeListFilter {
            target_id: Some(id),
            relations: vec![EdgeRelation::CompetesWith],
            ..Default::default()
        },
    ] {
        let edges = crate::memo::list_edges(runtime, token, filter, 1000, 0).await?;
        for e in edges {
            let has_role = e
                .metadata
                .as_ref()
                .and_then(|m| m.get("icd"))
                .and_then(|v| v.as_array())
                .map(|arr| arr.iter().any(|x| x.as_str() == Some("excludes1")))
                .unwrap_or(false);
            if has_role {
                out.insert(if e.source_id == id {
                    e.target_id
                } else {
                    e.source_id
                });
            }
        }
    }
    Ok(out)
}

/// `excludes1_neighbors`'s ids, rendered as codes — shared by
/// `icd10.resolve`'s candidate `excludes1` field so it is read off the same
/// edge query `icd10.check`'s R4 and `icd10.exclude`'s E4 use.
pub(crate) async fn excludes1_codes(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    id: Uuid,
) -> Result<Vec<String>, RuntimeError> {
    let ids = excludes1_neighbors(runtime, token, id).await?;
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let ids: Vec<Uuid> = ids.into_iter().collect();
    let entities = crate::memo::entities_by_ids(runtime, token, &ids).await?;
    Ok(entities.into_iter().map(|e| e.name).collect())
}

/// The ICD-10-CM chapter range (e.g. `"K00-K95"`) an entity falls under.
/// `icd_code` entities carry their chapter range directly
/// (`ingest.rs`'s `code.chapter_range`); `icd_category` entities do not
/// (only `icd_code` gets a `chapter` property), so this walks `ancestors()`
/// to the `icd_chapter` ancestor and reads its `range` property. Shared by
/// `icd10.resolve`'s "chapters the index-arm hits reached" and
/// `icd10.exclude`'s E6.
pub(crate) async fn chapter_range_of(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    id: Uuid,
) -> Result<Option<String>, RuntimeError> {
    let entity = crate::memo::entity_by_id(runtime, token, id).await?;
    let Some(entity) = entity else {
        return Ok(None);
    };
    if entity.entity_type.as_deref() == Some("icd_code") {
        if let Some(chapter) = entity
            .properties
            .as_ref()
            .and_then(|p| p.get("chapter"))
            .and_then(Value::as_str)
        {
            return Ok(Some(chapter.to_string()));
        }
    }
    let ancestor_ids = ancestors(runtime, token, id).await?;
    if ancestor_ids.is_empty() {
        return Ok(None);
    }
    let ids: Vec<Uuid> = ancestor_ids.into_iter().collect();
    let ancestor_entities = crate::memo::entities_by_ids(runtime, token, &ids).await?;
    for e in ancestor_entities {
        if e.entity_type.as_deref() == Some("icd_chapter") {
            if let Some(range) = e
                .properties
                .as_ref()
                .and_then(|p| p.get("range"))
                .and_then(Value::as_str)
            {
                return Ok(Some(range.to_string()));
            }
        }
    }
    Ok(None)
}
