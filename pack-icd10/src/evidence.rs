//! `icd10.evidence(mentions, codes, sex?, age_years?)` — for each requested
//! code, the sourced paths from this visit's mentions to it, and a verdict
//! (`direct` | `medication` | `guideline` | `category_only` | `unsupported`).
//!
//! `sex`/`age_years` are accepted for signature parity with `icd10.resolve`/
//! `icd10.exclude` and are not applied here — sex/age filtering is
//! `icd10.exclude`'s E2/E3, not this verb's concern.
//!
//! Negated and family-history mentions never support anything: filtered out
//! before any arm runs (`usable_mentions`), so a caller cannot reach the
//! medication/direct/guideline arms through a mention the visit itself
//! disclaims.
//!
//! ## Category-or-below semantics
//!
//! A guideline premise or conclusion written as a category (e.g. `"I10"`,
//! `"N18"`) means "this category or any code below it" on BOTH sides:
//!
//! - Premise-present: satisfied when the premise's own resolved entity
//!   equals a supported code's entity, OR is an ANCESTOR of it (walked via
//!   `check::ancestors`, the same `contains`-hierarchy closure `icd10.check`
//!   uses for its own R3 ancestor rule) — a supported code two levels below
//!   a category premise still satisfies that premise.
//! - Conclusion-reaches: satisfied when the requested code's own entity
//!   equals the conclusion's resolved entity, OR the conclusion is an
//!   ancestor of it (same `ancestors()` closure, checked from the
//!   requested-code side). The `medication` arm applies the identical rule
//!   ("the code is that node or below it") when a DOID xref resolves to a
//!   category rather than the requested code itself.
//!
//! Both directions reuse one helper, `is_self_or_descendant`, rather than
//! two hand-written ancestor walks that could drift apart.

use std::collections::{HashMap, HashSet};

use khive_runtime::{EdgeListFilter, KhiveRuntime, NamespaceToken, RuntimeError};
use khive_types::EdgeRelation;
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

use crate::check::ancestors;
use crate::neighborhood::find_by_code;
use crate::refs::normalize_code;
use crate::resolve::{handle_resolve, immediate_parent};

#[derive(Debug, Clone, Deserialize)]
struct MentionIn {
    #[serde(rename = "type")]
    kind: String,
    name: String,
    #[serde(default)]
    #[allow(dead_code)] // carried through the trace/UI shape; not read by matching itself.
    quote: Option<String>,
    #[serde(default)]
    status: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct EvidenceParams {
    mentions: Vec<MentionIn>,
    codes: Vec<String>,
    #[serde(default)]
    #[allow(dead_code)]
    sex: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    age_years: Option<f64>,
}

/// A trace-schema path step: `{node: {kind, key, label}, relation, edge}`.
/// `relation`/`edge` are `None` on a path's first step (nothing precedes the
/// origin mention).
fn step(kind: &str, key: &str, label: &str, relation: Option<&str>, edge: Option<Value>) -> Value {
    json!({
        "node": {"kind": kind, "key": key, "label": label},
        "relation": relation,
        "edge": edge,
    })
}

fn is_excluded_from_evidence(m: &MentionIn) -> bool {
    let status = m.status.as_deref().unwrap_or("current");
    if status.eq_ignore_ascii_case("negated") || status.eq_ignore_ascii_case("family") {
        return true;
    }
    m.kind == "medication" && !status.eq_ignore_ascii_case("current")
}

pub async fn handle_evidence(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    params: Value,
) -> Result<Value, RuntimeError> {
    let params: EvidenceParams = serde_json::from_value(params).map_err(|e| {
        RuntimeError::InvalidInput(format!("invalid icd10.evidence arguments: {e}"))
    })?;

    // Negated/family-history mentions never support anything. Medication
    // evidence additionally requires a current mention; a past, planned, or
    // uncertain drug is not support for a present disease. This filter is
    // the single choke point every arm below reads through.
    let usable: Vec<&MentionIn> = params
        .mentions
        .iter()
        .filter(|m| !is_excluded_from_evidence(m))
        .collect();

    // ── Resolve every distinct condition/symptom/encounter mention text
    //    exactly ONCE and cache it in a map both arms read: calling
    //    `icd10.resolve` once per (code, mention) pair in the direct arm,
    //    plus again per (code, mention) in `category_only_path` (pass 2) for
    //    every code pass 1 left undecided, is O(codes * mentions) calls to
    //    the same handful of distinct mention texts — enough to run a real
    //    encounter (many requested codes, few distinct mentions) past 11
    //    minutes. ─────────────────────────────────────────────────────────
    let condition_like: Vec<&MentionIn> = usable
        .iter()
        .filter(|m| matches!(m.kind.as_str(), "condition" | "symptom" | "encounter"))
        .copied()
        .collect();
    let mut resolved_by_mention: HashMap<String, Vec<Value>> = HashMap::new();
    for m in &condition_like {
        if resolved_by_mention.contains_key(&m.name) {
            continue;
        }
        let resolved = handle_resolve(
            runtime,
            token,
            json!({"mentions": [{"text": m.name}], "text_mode": "all_terms"}),
        )
        .await?;
        let candidates = resolved["mentions"][0]["candidates"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        resolved_by_mention.insert(m.name.clone(), candidates);
    }

    // ── Pass 1: direct + medication verdicts for every requested code. ─────
    // The guideline arm (pass 2 below) needs to know which OTHER requested
    // codes are actually supported — not merely which ones happen to name a
    // real graph entity — so premise presence is judged against ACTUAL
    // support, never bare existence. Computing every code's direct/
    // medication result first, in one pass, is what makes that judgment
    // possible without re-deriving it ad hoc per rule.
    struct CodeState {
        raw: String,
        target_id: Option<Uuid>,
        verdict: Option<&'static str>, // Some("direct" | "medication") once pass 1 decides it
        paths: Vec<Value>,
        abstentions: Vec<Value>,
    }
    let mut states: Vec<CodeState> = Vec::with_capacity(params.codes.len());
    for raw_code in &params.codes {
        let normalized = normalize_code(raw_code);
        let target = find_by_code(runtime, token, &normalized).await?;
        let target_id = target.as_ref().map(|e| e.id);

        let mut direct_paths: Vec<Value> = Vec::new();
        let mut medication_paths: Vec<Value> = Vec::new();
        let mut abstentions: Vec<Value> = Vec::new();

        // ── direct: condition / symptom / encounter mentions through
        //    resolve's own Index/Tabular arms (all_terms), read from the
        //    per-mention resolve cache built once above. ───────────────────
        for m in &condition_like {
            let candidates = resolved_by_mention
                .get(&m.name)
                .cloned()
                .unwrap_or_default();
            let Some(hit) = candidates
                .iter()
                .find(|c| c["code"].as_str() == Some(&normalized))
            else {
                continue;
            };
            // A candidate can carry several `matched` entries (one per
            // arm/via that reached it); a `via: "family"` entry is the
            // resolver widening the candidate list from a sibling hit, never
            // an Index/Tabular path to THIS code, so it does not count as
            // direct evidence — the same rule `icd10.verify`'s `reachable`
            // uses. When every entry for this hit is family-only, this
            // mention contributes no direct path for this code; it falls
            // through to category_only/unsupported.
            let matched_arr = hit["matched"].as_array().cloned().unwrap_or_default();
            let Some(matched) = matched_arr
                .iter()
                .find(|m| m["via"].as_str() != Some("family"))
            else {
                continue;
            };
            let arm = matched["arm"].as_str().unwrap_or("index").to_string();
            let via = matched["via"].as_str().unwrap_or("direct").to_string();
            let term_path = matched["term_path"].as_str().map(str::to_string);
            direct_paths.push(json!([
                step("mention", &m.name, m.name.as_str(), None, None),
                step(
                    "icd_code",
                    &normalized,
                    hit["desc"].as_str().unwrap_or(&normalized),
                    Some("resolves_to"),
                    Some(json!({
                        "icd": "resolve",
                        "arm": arm,
                        "via": via,
                        "term_path": term_path,
                        "source": "icd10.resolve (Index/Tabular, all_terms)",
                    })),
                ),
            ]));
        }

        // ── medication: drug mention -> enables -> mesh_disease ->
        //    variant_of -> icd category/code (this node or an ancestor of
        //    the requested code). ─────────────────────────────────────────
        if let Some(target) = &target {
            for m in usable.iter().filter(|m| m.kind == "medication") {
                let result = medication_evidence_for_mention(
                    runtime,
                    token,
                    &m.name,
                    target.id,
                    &normalized,
                )
                .await?;
                medication_paths.extend(result.paths);
                abstentions.extend(result.abstentions);
            }
        }

        let (verdict, paths) = if !direct_paths.is_empty() {
            (Some("direct"), direct_paths)
        } else if !medication_paths.is_empty() {
            (Some("medication"), medication_paths)
        } else {
            (None, Vec::new())
        };

        states.push(CodeState {
            raw: raw_code.clone(),
            target_id,
            verdict,
            paths,
            abstentions,
        });
    }

    // The support set pass 2 checks premises/conclusions against: entity ids
    // of every requested code pass 1 actually confirmed as direct/medication
    // — never a code that merely resolves to a graph entity but carries no
    // support of its own.
    let supported_ids: HashSet<Uuid> = states
        .iter()
        .filter(|s| s.verdict.is_some())
        .filter_map(|s| s.target_id)
        .collect();

    // ── Pass 2: for every code pass 1 left undecided, the guideline arm —
    //    every rule whose premises are all present among `supported_ids`
    //    concludes its conclusions (category-or-below both ways, see module
    //    doc) — then category_only/unsupported for whatever is left. ───────
    let mut codes_out: Vec<Value> = Vec::with_capacity(states.len());
    for state in states {
        if let Some(verdict) = state.verdict {
            codes_out.push(json!({"code": state.raw, "verdict": verdict, "paths": state.paths, "abstentions": state.abstentions}));
            continue;
        }
        if let Some(target_id) = state.target_id {
            if let Some(gp) = guideline_path(runtime, token, target_id, &supported_ids).await? {
                codes_out.push(json!({"code": state.raw, "verdict": "guideline", "paths": [gp], "abstentions": state.abstentions}));
                continue;
            }
            if let Some(cp) = category_only_path(
                runtime,
                token,
                target_id,
                &condition_like,
                &resolved_by_mention,
            )
            .await?
            {
                codes_out
                    .push(json!({"code": state.raw, "verdict": "category_only", "paths": [cp], "abstentions": state.abstentions}));
                continue;
            }
        }
        codes_out.push(json!({"code": state.raw, "verdict": "unsupported", "paths": [], "abstentions": state.abstentions}));
    }

    Ok(json!({"codes": codes_out}))
}

/// Medication support plus explicit reasons for withholding an ambiguous
/// drug-name match. The path alone cannot express why no match was returned.
pub(crate) struct MedicationEvidence {
    pub paths: Vec<Value>,
    pub abstentions: Vec<Value>,
}

/// Source-specific medication path to an ICD category/code that is the
/// requested target or an ancestor of it. An ambiguous RxCUI match abstains
/// and is reported separately from absence of a MED-RT relation.
pub(crate) async fn medication_evidence_for_mention(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    mention_name: &str,
    target_id: Uuid,
    normalized_code: &str,
) -> Result<MedicationEvidence, RuntimeError> {
    let mut result = MedicationEvidence {
        paths: Vec::new(),
        abstentions: Vec::new(),
    };
    let drugs = match find_drug_by_name(runtime, token, mention_name).await? {
        DrugLookup::Match(drugs) => drugs,
        DrugLookup::Absent => return Ok(result),
        DrugLookup::Ambiguous { rxcuis, method } => {
            result.abstentions.push(json!({
                "mention": mention_name,
                "reason": "ambiguous_drug_name",
                "match_method": method,
                "rxcuis": rxcuis,
            }));
            return Ok(result);
        }
    };
    for drug in drugs {
        let disease_edges = crate::memo::list_edges(
            runtime,
            token,
            EdgeListFilter {
                source_id: Some(drug.id),
                relations: vec![EdgeRelation::Enables],
                ..Default::default()
            },
            1000,
            0,
        )
        .await?;
        for de in &disease_edges {
            // MED-RT also contains may_prevent.  Ingest merges both predicates
            // onto one khive edge, so the generic relation alone cannot tell us
            // whether this is support for a present disease.
            let may_treat = de
                .metadata
                .as_ref()
                .and_then(|m| m.get("icd"))
                .and_then(Value::as_array)
                .is_some_and(|roles| roles.iter().any(|role| role == "may_treat"));
            if !may_treat {
                continue;
            }
            let variant_edges = crate::memo::list_edges(
                runtime,
                token,
                EdgeListFilter {
                    source_id: Some(de.target_id),
                    relations: vec![EdgeRelation::VariantOf],
                    ..Default::default()
                },
                1000,
                0,
            )
            .await?;
            for ve in &variant_edges {
                if !is_self_or_descendant(runtime, token, ve.target_id, target_id).await? {
                    continue;
                }
                let mesh_entity = fetch_one(runtime, token, de.target_id).await?;
                let reached_entity = fetch_one(runtime, token, ve.target_id).await?;
                let (Some(mesh_entity), Some(reached_entity)) = (mesh_entity, reached_entity)
                else {
                    continue;
                };
                // Keep the original MED-RT predicates for provenance, but the
                // supporting role of this path is only the explicit may_treat.
                let mut treatment_metadata = de.metadata.clone().unwrap_or(Value::Null);
                if let Some(metadata) = treatment_metadata.as_object_mut() {
                    if let Some(predicates) = metadata.get("icd").cloned() {
                        metadata.insert("source_predicates".to_string(), predicates);
                    }
                    metadata.insert("icd".to_string(), json!("may_treat"));
                }
                let mut path = vec![
                    step("mention", mention_name, mention_name, None, None),
                    step(
                        "drug",
                        &drug.name, // rxcui: the natural key ensure_entities dedups drugs on
                        &drug_display_name(&drug),
                        Some("mentions"),
                        Some(json!({"icd": "mention_match"})),
                    ),
                    step(
                        "mesh_disease",
                        mesh_entity
                            .properties
                            .as_ref()
                            .and_then(|p| p.get("mesh_id"))
                            .and_then(Value::as_str)
                            .unwrap_or(&mesh_entity.name),
                        &mesh_display_name(&mesh_entity),
                        Some("enables"),
                        Some(treatment_metadata),
                    ),
                    step(
                        reached_entity
                            .entity_type
                            .as_deref()
                            .unwrap_or("icd_category"),
                        &reached_entity.name,
                        &icd_display_name(&reached_entity),
                        Some("variant_of"),
                        Some(ve.metadata.clone().unwrap_or(Value::Null)),
                    ),
                ];
                if reached_entity.name != normalized_code {
                    if let Some(chain) =
                        contains_descent(runtime, token, ve.target_id, target_id).await?
                    {
                        for hop_id in chain {
                            if hop_id == ve.target_id {
                                continue;
                            }
                            if let Some(e) = fetch_one(runtime, token, hop_id).await? {
                                path.push(step(
                                    e.entity_type.as_deref().unwrap_or("icd_code"),
                                    &e.name,
                                    &icd_display_name(&e),
                                    Some("contains"),
                                    Some(json!({"icd": "hierarchy"})),
                                ));
                            }
                        }
                    }
                }
                let path = json!(path);
                if !result.paths.contains(&path) {
                    result.paths.push(path);
                }
            }
        }
    }
    Ok(result)
}

/// `true` when `descendant_id` equals `ancestor_id` or `ancestor_id` is in
/// `descendant_id`'s `contains`-hierarchy ancestor closure. Shared by both
/// halves of the category-or-below rule (see module doc). `pub(crate)`:
/// `icd10.verify`'s guideline facet reuses this exact "category or
/// below" test rather than re-deriving it.
pub(crate) async fn is_self_or_descendant(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    ancestor_id: Uuid,
    descendant_id: Uuid,
) -> Result<bool, RuntimeError> {
    if ancestor_id == descendant_id {
        return Ok(true);
    }
    let anc = ancestors(runtime, token, descendant_id).await?;
    Ok(anc.contains(&ancestor_id))
}

/// The ordered `contains` chain from `from_id` down to `to_id` (inclusive of
/// both ends), or `None` when `to_id` is not a descendant of `from_id`. Built
/// by walking `to_id` upward (mirrors `check::ancestors`) and reversing —
/// the code hierarchy is shallow (chapter/block/category/code/code(7th), at
/// most 5 hops), so this repeats work `ancestors()` already validated rather
/// than adding a second downward-traversal primitive.
async fn contains_descent(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    from_id: Uuid,
    to_id: Uuid,
) -> Result<Option<Vec<Uuid>>, RuntimeError> {
    if from_id == to_id {
        return Ok(Some(vec![from_id]));
    }
    let mut chain = vec![to_id];
    let mut current = to_id;
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
            return Ok(None);
        };
        chain.push(parent);
        if parent == from_id {
            chain.reverse();
            return Ok(Some(chain));
        }
        current = parent;
    }
    Ok(None)
}

/// A medication mention matches a `drug` by case-insensitive name. MED-RT
/// carries both ingredient and salt names, so a mention that only names the
/// ingredient (`"metformin"`) can match a stored salt name
/// (`"metformin hydrochloride"`) by trying the drug name with its trailing
/// word removed once. Either lookup abstains when more than one RxCUI matches.
enum DrugLookup {
    Match(Vec<khive_storage::Entity>),
    Absent,
    Ambiguous {
        rxcuis: Vec<String>,
        method: &'static str,
    },
}

fn classify_drug_matches(matches: Vec<&khive_storage::Entity>, method: &'static str) -> DrugLookup {
    let mut rxcuis: Vec<String> = matches.iter().map(|drug| drug.name.clone()).collect();
    rxcuis.sort_unstable();
    rxcuis.dedup();
    match rxcuis.len() {
        0 => DrugLookup::Absent,
        1 => DrugLookup::Match(matches.into_iter().cloned().collect()),
        _ => DrugLookup::Ambiguous { rxcuis, method },
    }
}

async fn find_drug_by_name(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    mention_name: &str,
) -> Result<DrugLookup, RuntimeError> {
    const PAGE_SIZE: u32 = 10_000;
    let mut drugs = Vec::new();
    let mut offset = 0_u32;
    loop {
        let page = crate::memo::entities_of_type(runtime, token, "drug", PAGE_SIZE, offset).await?;
        let count = u32::try_from(page.len()).map_err(|_| {
            RuntimeError::InvalidInput("drug lookup page exceeds u32 range".to_string())
        })?;
        drugs.extend(page);
        if count < PAGE_SIZE {
            break;
        }
        offset = offset.checked_add(count).ok_or_else(|| {
            RuntimeError::InvalidInput("drug lookup offset exceeds u32 range".to_string())
        })?;
    }
    let target = mention_name.trim().to_lowercase();
    // `ingest_medrt` keys a `drug` entity's canonical `name` field by rxcui
    // (the natural key `ensure_entities` dedups on), never by the drug's own
    // name — the human-readable name lives only in `properties.name` (and
    // `description`, which `properties.name` mirrors at ingest time). A
    // mention names a drug, never an rxcui, so matching must read the
    // property, not the entity name (see `drug_display_name`).
    let exact: Vec<_> = drugs
        .iter()
        .filter(|d| drug_display_name(d).to_lowercase() == target)
        .collect();
    if !exact.is_empty() {
        return Ok(classify_drug_matches(exact, "exact"));
    }
    let shortened: Vec<_> = drugs
        .iter()
        .filter(|d| {
            drug_display_name(d)
                .to_lowercase()
                .rsplit_once(' ')
                .is_some_and(|(head, _)| head == target)
        })
        .collect();
    Ok(classify_drug_matches(shortened, "shortened"))
}

/// `d.name` is the rxcui (the natural key `ingest_medrt` dedups drugs on,
/// and the correct `<kind>:<natural key>` trace-node id); the human-readable name a
/// mention would actually use lives in
/// `properties.name`.
fn drug_display_name(d: &khive_storage::Entity) -> String {
    d.properties
        .as_ref()
        .and_then(|p| p.get("name"))
        .and_then(Value::as_str)
        .unwrap_or(&d.name)
        .to_string()
}

/// `e.name` is the mesh_id (`ingest_medrt`'s natural key); the human-readable
/// MeSH name lives in `properties.name` (MED-RT's `mesh_name`, stored at
/// ingest — see `ingest_external.rs::ingest_medrt`). A medication-path
/// `mesh_disease` step's LABEL reads this, never the bare id.
fn mesh_display_name(e: &khive_storage::Entity) -> String {
    e.properties
        .as_ref()
        .and_then(|p| p.get("name"))
        .and_then(Value::as_str)
        .unwrap_or(&e.name)
        .to_string()
}

/// `e.name` is the bare ICD-10-CM code/category (e.g. `"E11.9"`); the human
/// description lives in `properties.desc` (`ingest.rs`'s category/code
/// entities both carry it). An `icd_category`/`icd_code` step's LABEL on
/// the medication path reads this, never the bare code.
fn icd_display_name(e: &khive_storage::Entity) -> String {
    e.properties
        .as_ref()
        .and_then(|p| p.get("desc"))
        .and_then(Value::as_str)
        .unwrap_or(&e.name)
        .to_string()
}

async fn fetch_one(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    id: Uuid,
) -> Result<Option<khive_storage::Entity>, RuntimeError> {
    crate::memo::entity_by_id(runtime, token, id).await
}

/// Every `guideline_rule` whose premises are ALL present among
/// `supported_ids` (each premise present via `is_self_or_descendant`) and
/// whose conclusions reach `target_id` (again via `is_self_or_descendant`,
/// so a category conclusion covers every code below it) returns the first
/// such path found. `target_id` is never itself a member of the caller's
/// `supported_ids` (pass 1 excludes a code's own entity from the support set
/// it computes for every OTHER code), so a rule cannot use the code it is
/// trying to conclude as its own premise.
async fn guideline_path(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    target_id: Uuid,
    supported_ids: &HashSet<Uuid>,
) -> Result<Option<Value>, RuntimeError> {
    // `supported_ids` is pass 1's confirmed direct/medication set (see the
    // module doc and `handle_evidence`'s pass-1/pass-2 split) — never
    // re-derived from bare existence here, since that was this function's
    // own bug before the split: a requested code that merely NAMED a real
    // entity used to satisfy a premise even when its own verdict was
    // `unsupported`.
    if supported_ids.is_empty() {
        return Ok(None);
    }
    let ids: Vec<Uuid> = supported_ids.iter().copied().collect();
    let label_entities = crate::memo::entities_by_ids(runtime, token, &ids).await?;
    let label_by_id: HashMap<Uuid, String> =
        label_entities.into_iter().map(|e| (e.id, e.name)).collect();

    let rules = crate::memo::entities_of_type(runtime, token, "guideline_rule", 10_000, 0).await?;

    for rule in &rules {
        let premise_edges = crate::memo::list_edges(
            runtime,
            token,
            EdgeListFilter {
                source_id: Some(rule.id),
                relations: vec![EdgeRelation::DependsOn],
                ..Default::default()
            },
            1000,
            0,
        )
        .await?;
        if premise_edges.is_empty() {
            continue;
        }
        let mut premise_hits: Vec<Uuid> = Vec::new();
        let mut all_present = true;
        for pe in &premise_edges {
            let mut hit = None;
            for &supported in supported_ids {
                if is_self_or_descendant(runtime, token, pe.target_id, supported).await? {
                    hit = Some(supported);
                    break;
                }
            }
            match hit {
                Some(h) => premise_hits.push(h),
                None => {
                    all_present = false;
                    break;
                }
            }
        }
        if !all_present {
            continue;
        }

        let conclusion_edges = crate::memo::list_edges(
            runtime,
            token,
            EdgeListFilter {
                source_id: Some(rule.id),
                relations: vec![EdgeRelation::Enables],
                ..Default::default()
            },
            1000,
            0,
        )
        .await?;
        let mut concl = None;
        for ce in &conclusion_edges {
            if is_self_or_descendant(runtime, token, ce.target_id, target_id).await? {
                concl = Some(ce);
                break;
            }
        }
        let Some(concl) = concl else {
            continue;
        };

        let mut path: Vec<Value> = Vec::new();
        for (i, pe) in premise_edges.iter().enumerate() {
            let supported_id = premise_hits[i];
            let label = label_by_id.get(&supported_id).cloned().unwrap_or_default();
            path.push(step(
                "icd_code",
                &label,
                &label,
                if i == 0 { None } else { Some("depends_on") },
                if i == 0 {
                    None
                } else {
                    Some(pe.metadata.clone().unwrap_or(Value::Null))
                },
            ));
        }
        path.push(step(
            "guideline_rule",
            &rule.name,
            rule.description.as_deref().unwrap_or(&rule.name),
            Some("depends_on"),
            Some(
                premise_edges
                    .first()
                    .and_then(|e| e.metadata.clone())
                    .unwrap_or(Value::Null),
            ),
        ));
        let concl_entity = fetch_one(runtime, token, concl.target_id).await?;
        let concl_label = concl_entity
            .as_ref()
            .map(|e| e.name.clone())
            .unwrap_or_default();
        path.push(step(
            concl_entity
                .as_ref()
                .and_then(|e| e.entity_type.clone())
                .unwrap_or_else(|| "icd_category".to_string())
                .as_str(),
            &concl_label,
            &concl_label,
            Some("enables"),
            Some(concl.metadata.clone().unwrap_or(Value::Null)),
        ));
        if concl.target_id != target_id {
            if let Some(chain) =
                contains_descent(runtime, token, concl.target_id, target_id).await?
            {
                for hop_id in chain {
                    if hop_id == concl.target_id {
                        continue;
                    }
                    if let Some(e) = fetch_one(runtime, token, hop_id).await? {
                        path.push(step(
                            e.entity_type.as_deref().unwrap_or("icd_code"),
                            &e.name,
                            &e.name,
                            Some("contains"),
                            Some(json!({"icd": "hierarchy"})),
                        ));
                    }
                }
            }
        }
        return Ok(Some(json!(path)));
    }
    Ok(None)
}

/// The `category_only` verdict ("only the category is reached"):
/// `target_id`'s own immediate `contains`-parent is a
/// real code/category the graph knows, but nothing about `target_id` itself
/// is directly, medically, or guideline-supported — a usable
/// condition/symptom/encounter mention reached that PARENT (by any
/// `icd10.resolve` arm) without ever naming `target_id`'s own code.
/// `Ok(None)` when no usable mention reaches the parent at all — the caller
/// falls back to `unsupported` (a mention that names nothing above this code
/// — an unrelated code, or a negated/family mention filtered out before any
/// arm runs — must stay `unsupported`, never fall back to `category_only`
/// purely because the code happens to exist in the graph).
async fn category_only_path(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    target_id: Uuid,
    condition_like: &[&MentionIn],
    resolved_by_mention: &HashMap<String, Vec<Value>>,
) -> Result<Option<Value>, RuntimeError> {
    let Some(parent_id) = immediate_parent(runtime, token, target_id).await? else {
        return Ok(None);
    };
    let Some(parent) = fetch_one(runtime, token, parent_id).await? else {
        return Ok(None);
    };

    for m in condition_like {
        let candidates = resolved_by_mention
            .get(&m.name)
            .cloned()
            .unwrap_or_default();
        let Some(hit) = candidates
            .iter()
            .find(|c| c["code"].as_str() == Some(parent.name.as_str()))
        else {
            continue;
        };
        let matched = hit["matched"][0].clone();
        let arm = matched["arm"].as_str().unwrap_or("index").to_string();
        let via = matched["via"].as_str().unwrap_or("direct").to_string();
        let term_path = matched["term_path"].as_str().map(str::to_string);

        let mut path = vec![
            step("mention", &m.name, m.name.as_str(), None, None),
            step(
                parent.entity_type.as_deref().unwrap_or("icd_category"),
                &parent.name,
                hit["desc"].as_str().unwrap_or(&parent.name),
                Some("resolves_to"),
                Some(json!({
                    "icd": "resolve",
                    "arm": arm,
                    "via": via,
                    "term_path": term_path,
                    "source": "icd10.resolve (Index/Tabular, all_terms)",
                })),
            ),
        ];
        if let Some(chain) = contains_descent(runtime, token, parent_id, target_id).await? {
            for hop_id in chain {
                if hop_id == parent_id {
                    continue;
                }
                if let Some(e) = fetch_one(runtime, token, hop_id).await? {
                    path.push(step(
                        e.entity_type.as_deref().unwrap_or("icd_code"),
                        &e.name,
                        &e.name,
                        Some("contains"),
                        Some(json!({"icd": "hierarchy"})),
                    ));
                }
            }
        }
        return Ok(Some(json!(path)));
    }
    Ok(None)
}
