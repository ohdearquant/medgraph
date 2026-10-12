//! `icd10.ingest_external(medrt_jsonl?, doid_obo?, ccsr_csv?, hcc_csv?, rules_jsonl?)`
//! — loads the medical-graph sources into the `drug` / `mesh_disease` /
//! `ccsr_category` / `hcc` / `guideline_rule` subtypes. Every source is independently
//! optional so it can be loaded (and re-loaded) on its own; every write
//! goes through `ingest::ensure_entities` (list-then-create-if-missing by
//! natural-key name) and `runtime.link_many` with `resurrect: true`, the
//! same deterministic-id / idempotent-re-run pattern `icd10.ingest` already
//! uses — a repeat call with the same source files reports zero newly
//! created entities and the identical edge counts.
//!
//! No model-invented edge: every edge this verb writes names its source in
//! `metadata.source` (plus `metadata.icd` naming the specific relation).

use std::collections::{HashMap, HashSet};
use std::time::Instant;

use khive_runtime::operations::LinkSpec;
use khive_runtime::{KhiveRuntime, NamespaceToken, RuntimeError};
use khive_storage::entity::EntityFilter;
use khive_types::EdgeRelation;
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

use crate::csv_lite::{parse_csv, strip_single_quotes};
use crate::ingest::ensure_entities;
use crate::neighborhood::find_by_code;
use crate::refs::normalize_code;

const EDGE_BATCH: usize = 1000;
const ISA_WALK_MAX_HOPS: u32 = 3;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct IngestExternalParams {
    medrt_jsonl: Option<String>,
    doid_obo: Option<String>,
    ccsr_csv: Option<String>,
    hcc_csv: Option<String>,
    rules_jsonl: Option<String>,
}

/// One (source, target, relation) edge accumulator, mirroring `ingest.rs`'s
/// own `EdgeAccum` shape: `icd` collects every distinct role string touching
/// this pair (so a pair reached two ways still yields one edge), and `extra`
/// carries the per-source metadata fields (source, doid, parent_hops,
/// default_inpatient, ...), last-write-wins per
/// key (sources here never disagree on their own fields across roles).
struct EdgeAccum {
    icd: Vec<String>,
    extra: serde_json::Map<String, Value>,
}

struct EdgeAcc {
    map: HashMap<(Uuid, Uuid, EdgeRelation), EdgeAccum>,
}

impl EdgeAcc {
    fn new() -> Self {
        Self {
            map: HashMap::new(),
        }
    }

    fn push(
        &mut self,
        source: Uuid,
        target: Uuid,
        relation: EdgeRelation,
        role: &str,
        extra: serde_json::Map<String, Value>,
    ) {
        let entry = self
            .map
            .entry((source, target, relation))
            .or_insert_with(|| EdgeAccum {
                icd: Vec::new(),
                extra: serde_json::Map::new(),
            });
        if !entry.icd.iter().any(|r| r == role) {
            entry.icd.push(role.to_string());
        }
        for (k, v) in extra {
            entry.extra.insert(k, v);
        }
    }

    async fn flush(
        self,
        runtime: &KhiveRuntime,
        token: &NamespaceToken,
    ) -> Result<usize, RuntimeError> {
        let total = self.map.len();
        let mut batch: Vec<LinkSpec> = Vec::with_capacity(EDGE_BATCH);
        for ((source_id, target_id, relation), acc) in self.map {
            let mut metadata = acc.extra;
            metadata.insert("icd".to_string(), json!(acc.icd));
            batch.push(LinkSpec {
                namespace: None,
                source_id,
                target_id,
                relation,
                weight: 1.0,
                metadata: Some(Value::Object(metadata)),
                resurrect: true,
            });
            if batch.len() >= EDGE_BATCH {
                runtime.link_many(token, std::mem::take(&mut batch)).await?;
            }
        }
        if !batch.is_empty() {
            runtime.link_many(token, batch).await?;
        }
        Ok(total)
    }
}

struct Unplaced {
    rows: Vec<(String, String)>, // (source, reason)
}

impl Unplaced {
    fn new() -> Self {
        Self { rows: Vec::new() }
    }
    fn push(&mut self, source: &str, reason: impl Into<String>) {
        self.rows.push((source.to_string(), reason.into()));
    }
}

pub async fn handle_ingest_external(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    params: Value,
) -> Result<Value, RuntimeError> {
    let started = Instant::now();
    let params: IngestExternalParams = serde_json::from_value(params).map_err(|e| {
        RuntimeError::InvalidInput(format!("invalid icd10.ingest_external arguments: {e}"))
    })?;

    let mut source_results: serde_json::Map<String, Value> = serde_json::Map::new();
    let mut unplaced = Unplaced::new();

    if let Some(path) = &params.medrt_jsonl {
        let r = ingest_medrt(runtime, token, path, &mut unplaced).await?;
        source_results.insert("medrt".to_string(), r);
    }
    if let Some(path) = &params.doid_obo {
        let r = ingest_doid(runtime, token, path, &mut unplaced).await?;
        source_results.insert("doid".to_string(), r);
    }
    if let Some(path) = &params.ccsr_csv {
        let r = ingest_ccsr(runtime, token, path, &mut unplaced).await?;
        source_results.insert("ccsr".to_string(), r);
    }
    if let Some(path) = &params.hcc_csv {
        let r = ingest_hcc(runtime, token, path, &mut unplaced).await?;
        source_results.insert("hcc".to_string(), r);
    }
    if let Some(path) = &params.rules_jsonl {
        let r = ingest_rules(runtime, token, path, &mut unplaced).await?;
        source_results.insert("rules".to_string(), r);
    }

    let unplaced_examples: Vec<Value> = unplaced
        .rows
        .iter()
        .take(20)
        .map(|(source, reason)| json!({"source": source, "reason": reason}))
        .collect();

    Ok(json!({
        "sources": source_results,
        "unplaced": {"count": unplaced.rows.len(), "examples": unplaced_examples},
        "elapsed_ms": started.elapsed().as_millis(),
    }))
}

// ── MED-RT ──────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct MedrtRow {
    mesh_id: String,
    mesh_name: String,
    rela: String,
    rxcui: String,
    drug: String,
    tty: String,
}

async fn ingest_medrt(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    path: &str,
    unplaced: &mut Unplaced,
) -> Result<Value, RuntimeError> {
    let content = std::fs::read_to_string(path)
        .map_err(|e| RuntimeError::InvalidInput(format!("reading medrt_jsonl {path:?}: {e}")))?;

    let mut drug_props: HashMap<String, (String, String)> = HashMap::new(); // rxcui -> (name, tty)
    let mut mesh_props: HashMap<String, String> = HashMap::new(); // mesh_id -> mesh_name
    let mut rows: Vec<MedrtRow> = Vec::new();

    for (i, line) in content.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        match serde_json::from_str::<MedrtRow>(line) {
            Ok(row) => {
                if row.mesh_id.trim().is_empty() || row.rxcui.trim().is_empty() {
                    unplaced.push("medrt", format!("line {}: mesh_id or rxcui empty", i + 1));
                    continue;
                }
                drug_props
                    .entry(row.rxcui.clone())
                    .or_insert_with(|| (row.drug.clone(), row.tty.clone()));
                mesh_props
                    .entry(row.mesh_id.clone())
                    .or_insert_with(|| row.mesh_name.clone());
                rows.push(row);
            }
            Err(e) => {
                unplaced.push("medrt", format!("line {}: invalid JSON: {e}", i + 1));
            }
        }
    }

    let drug_wanted: Vec<(String, Value, Option<String>)> = drug_props
        .iter()
        .map(|(rxcui, (name, tty))| {
            (
                rxcui.clone(),
                json!({"rxcui": rxcui, "name": name, "tty": tty}),
                Some(name.clone()),
            )
        })
        .collect();
    let (drug_ids, drug_created) = ensure_entities(runtime, token, "drug", drug_wanted).await?;

    let mesh_wanted: Vec<(String, Value, Option<String>)> = mesh_props
        .iter()
        .map(|(mesh_id, name)| {
            (
                mesh_id.clone(),
                json!({"mesh_id": mesh_id, "name": name}),
                Some(name.clone()),
            )
        })
        .collect();
    let (mesh_ids, mesh_created) =
        ensure_entities(runtime, token, "mesh_disease", mesh_wanted).await?;

    let mut edges = EdgeAcc::new();
    for row in &rows {
        let (Some(&drug_id), Some(&mesh_id)) =
            (drug_ids.get(&row.rxcui), mesh_ids.get(&row.mesh_id))
        else {
            unplaced.push(
                "medrt",
                format!(
                    "rxcui {} / mesh_id {}: entity missing after create",
                    row.rxcui, row.mesh_id
                ),
            );
            continue;
        };
        let mut extra = serde_json::Map::new();
        extra.insert("source".to_string(), json!("MED-RT (NLM RxClass)"));
        edges.push(drug_id, mesh_id, EdgeRelation::Enables, &row.rela, extra);
    }
    let edges_total = edges.flush(runtime, token).await?;

    Ok(json!({
        "created": {"drug": drug_created, "mesh_disease": mesh_created},
        "rows_read": rows.len(),
        "edges_total": edges_total,
    }))
}

// ── DOID ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Default)]
struct DoidTerm {
    id: String,
    #[allow(dead_code)]
    name: String,
    mesh_ids: Vec<String>,
    icd10cm_xrefs: Vec<String>,
    is_a: Vec<String>,
}

fn parse_doid_obo(content: &str) -> Vec<DoidTerm> {
    let mut terms: Vec<DoidTerm> = Vec::new();
    let mut current: Option<DoidTerm> = None;
    let mut in_term_stanza = false;

    for raw_line in content.lines() {
        let line = raw_line.trim_end();
        if line.starts_with('[') && line.ends_with(']') {
            if let Some(t) = current.take() {
                terms.push(t);
            }
            in_term_stanza = line == "[Term]";
            if in_term_stanza {
                current = Some(DoidTerm::default());
            }
            continue;
        }
        if !in_term_stanza {
            continue;
        }
        let Some(t) = current.as_mut() else { continue };
        if let Some(rest) = line.strip_prefix("id:") {
            t.id = rest.trim().to_string();
        } else if let Some(rest) = line.strip_prefix("name:") {
            t.name = rest.trim().to_string();
        } else if let Some(rest) = line.strip_prefix("xref:") {
            let rest = rest.trim();
            if let Some(mesh) = rest.strip_prefix("MESH:") {
                let mesh = mesh.split_whitespace().next().unwrap_or("").to_string();
                if !mesh.is_empty() {
                    t.mesh_ids.push(mesh);
                }
            } else if let Some(icd) = rest.strip_prefix("ICD10CM:") {
                let icd = icd.split_whitespace().next().unwrap_or("").to_string();
                if !icd.is_empty() {
                    t.icd10cm_xrefs.push(icd);
                }
            }
        } else if let Some(rest) = line.strip_prefix("is_a:") {
            let token = rest.trim().split('!').next().unwrap_or("").trim();
            if !token.is_empty() {
                t.is_a.push(token.to_string());
            }
        }
    }
    if let Some(t) = current.take() {
        terms.push(t);
    }
    terms
}

/// Resolve one ICD10CM xref token (a code, a category, or a block range like
/// `"E08-E13"`) to the graph entity/entities it names. A range links every
/// icd_category child of the matching icd_block.
async fn resolve_icd10cm_token(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    raw: &str,
) -> Result<Vec<Uuid>, RuntimeError> {
    let raw = raw.trim();
    if xref_range(raw).is_some() {
        // Block range: find the icd_block entity, then every category it contains.
        let block = runtime
            .list_entities_filtered(
                token,
                EntityFilter {
                    entity_types: vec!["icd_block".to_string()],
                    name_exact: Some(raw.to_string()),
                    ..Default::default()
                },
                1,
                0,
            )
            .await?;
        if let Some(block_entity) = block.into_iter().next() {
            let children = runtime
                .list_edges(
                    token,
                    khive_runtime::EdgeListFilter {
                        source_id: Some(block_entity.id),
                        relations: vec![EdgeRelation::Contains],
                        ..Default::default()
                    },
                    1000,
                    0,
                )
                .await?;
            return Ok(children.into_iter().map(|e| e.target_id).collect());
        }
        return Ok(Vec::new());
    }
    let code = normalize_code(raw);
    match find_by_code(runtime, token, &code).await? {
        Some(entity) => Ok(vec![entity.id]),
        None => Ok(Vec::new()),
    }
}

fn looks_like_code(tok: &str) -> bool {
    let mut chars = tok.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_uppercase())
        && chars.next().is_some_and(|c| c.is_ascii_digit())
}

/// `Some(raw)` when `raw` is a block-range xref token (e.g. `"E08-E13"`, both
/// halves `looks_like_code`), `None` for a single code/category token. Shared
/// by `resolve_icd10cm_token` (decides how to resolve the token) and
/// `ingest_doid`, which tags the resulting edge(s) `props.xref_range` so a
/// range source is never indistinguishable from a precise single-code xref
/// on the trace.
fn xref_range(raw: &str) -> Option<&str> {
    let (a, b) = raw.split_once('-')?;
    (looks_like_code(a) && looks_like_code(b)).then_some(raw)
}

async fn ingest_doid(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    path: &str,
    unplaced: &mut Unplaced,
) -> Result<Value, RuntimeError> {
    let content = std::fs::read_to_string(path)
        .map_err(|e| RuntimeError::InvalidInput(format!("reading doid_obo {path:?}: {e}")))?;
    let terms = parse_doid_obo(&content);

    let by_id: HashMap<String, usize> = terms
        .iter()
        .enumerate()
        .map(|(i, t)| (t.id.clone(), i))
        .collect();

    // Every mesh_disease entity currently in the graph (created by the MED-RT
    // pass, in this call or an earlier one) is a candidate xref target.
    let mesh_entities = runtime
        .list_entities_filtered(
            token,
            EntityFilter {
                entity_types: vec!["mesh_disease".to_string()],
                ..Default::default()
            },
            10_000,
            0,
        )
        .await?;
    let mesh_id_by_entity: HashMap<String, Uuid> = mesh_entities
        .iter()
        .filter_map(|e| {
            e.properties
                .as_ref()
                .and_then(|p| p.get("mesh_id"))
                .and_then(Value::as_str)
                .map(|m| (m.to_string(), e.id))
        })
        .collect();

    let mut mesh_by_id: HashMap<&str, Vec<&DoidTerm>> = HashMap::new();
    for t in &terms {
        for m in &t.mesh_ids {
            mesh_by_id.entry(m.as_str()).or_default().push(t);
        }
    }

    let mut edges = EdgeAcc::new();
    let mut xref_edges_written = 0usize;
    for (mesh_id, mesh_entity_id) in &mesh_id_by_entity {
        let Some(matching_terms) = mesh_by_id.get(mesh_id.as_str()) else {
            unplaced.push("doid", format!("mesh_id {mesh_id}: no DOID term xrefs it"));
            continue;
        };
        for doid_term in matching_terms {
            let (xrefs, parent_hops): (Vec<String>, u32) = if !doid_term.icd10cm_xrefs.is_empty() {
                (doid_term.icd10cm_xrefs.clone(), 0)
            } else {
                let mut found: Vec<String> = Vec::new();
                let mut hop = 0u32;
                let mut frontier: Vec<&str> = doid_term.is_a.iter().map(|s| s.as_str()).collect();
                let mut seen: HashSet<String> = HashSet::new();
                while hop < ISA_WALK_MAX_HOPS && found.is_empty() && !frontier.is_empty() {
                    hop += 1;
                    let mut next_frontier: Vec<&str> = Vec::new();
                    for parent_id in &frontier {
                        if !seen.insert(parent_id.to_string()) {
                            continue;
                        }
                        let Some(&idx) = by_id.get(*parent_id) else {
                            continue;
                        };
                        let parent = &terms[idx];
                        if !parent.icd10cm_xrefs.is_empty() {
                            found.extend(parent.icd10cm_xrefs.iter().cloned());
                        } else {
                            next_frontier.extend(parent.is_a.iter().map(|s| s.as_str()));
                        }
                    }
                    frontier = next_frontier;
                }
                (found, hop)
            };
            if xrefs.is_empty() {
                unplaced.push(
                    "doid",
                    format!(
                        "{} ({}): no ICD10CM xref within {ISA_WALK_MAX_HOPS} is_a hops",
                        doid_term.id, mesh_id
                    ),
                );
                continue;
            }
            for xref in &xrefs {
                let range = xref_range(xref);
                let targets = resolve_icd10cm_token(runtime, token, xref).await?;
                if targets.is_empty() {
                    unplaced.push(
                        "doid",
                        format!(
                            "{} xref {xref:?}: no matching icd_code/icd_category/icd_block",
                            doid_term.id
                        ),
                    );
                    continue;
                }
                for target_id in targets {
                    let mut extra = serde_json::Map::new();
                    extra.insert("source".to_string(), json!("DOID 2026-08-31"));
                    extra.insert("doid".to_string(), json!(doid_term.id));
                    extra.insert("parent_hops".to_string(), json!(parent_hops));
                    // A range xref (e.g. `E08-E13`) is kept as a step to
                    // every category in the block, but tagged with the range
                    // it actually came from, so a trace reader never
                    // mistakes it for a precise single-code xref to this one
                    // category.
                    if let Some(range) = range {
                        extra.insert("xref_range".to_string(), json!(range));
                    }
                    edges.push(
                        *mesh_entity_id,
                        target_id,
                        EdgeRelation::VariantOf,
                        "disease_xref",
                        extra,
                    );
                    xref_edges_written += 1;
                }
            }
        }
    }
    let edges_total = edges.flush(runtime, token).await?;

    Ok(json!({
        "terms_parsed": terms.len(),
        "mesh_disease_matched": mesh_id_by_entity.len(),
        "xref_edges_attempted": xref_edges_written,
        "edges_total": edges_total,
    }))
}

// ── AHRQ DXCCSR ─────────────────────────────────────────────────────────

async fn ingest_ccsr(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    path: &str,
    unplaced: &mut Unplaced,
) -> Result<Value, RuntimeError> {
    let content = std::fs::read_to_string(path)
        .map_err(|e| RuntimeError::InvalidInput(format!("reading ccsr_csv {path:?}: {e}")))?;
    let rows = parse_csv(&content);
    let Some(header) = rows.first() else {
        return Ok(json!({"created": {"ccsr_category": 0}, "rows_read": 0, "edges_total": 0}));
    };
    let header: Vec<String> = header.iter().map(|h| strip_single_quotes(h)).collect();
    let idx = |name: &str| header.iter().position(|h| h == name);

    let code_idx = idx("ICD-10-CM CODE");
    let default_ip_idx = idx("Default CCSR CATEGORY IP");
    let default_ip_desc_idx = idx("Default CCSR CATEGORY DESCRIPTION IP");
    let default_op_idx = idx("Default CCSR CATEGORY OP");
    let default_op_desc_idx = idx("Default CCSR CATEGORY DESCRIPTION OP");

    let Some(code_idx) = code_idx else {
        return Err(RuntimeError::InvalidInput(
            "ccsr_csv header missing \"ICD-10-CM CODE\" column".to_string(),
        ));
    };

    let mut category_names: HashMap<String, String> = HashMap::new();
    // code_entity_id -> (category_code -> (default_inpatient, default_outpatient))
    let mut per_code: HashMap<Uuid, HashMap<String, (bool, bool)>> = HashMap::new();
    let mut rows_read = 0usize;

    for row in rows.iter().skip(1) {
        if row.len() <= code_idx {
            continue;
        }
        rows_read += 1;
        let raw_code = strip_single_quotes(&row[code_idx]);
        if raw_code.is_empty() {
            continue;
        }
        let normalized = normalize_code(&raw_code);
        let Some(entity) = find_by_code(runtime, token, &normalized).await? else {
            unplaced.push("ccsr", format!("code {raw_code}: not in graph"));
            continue;
        };

        let default_ip = default_ip_idx
            .and_then(|i| row.get(i))
            .map(|v| strip_single_quotes(v));
        let default_op = default_op_idx
            .and_then(|i| row.get(i))
            .map(|v| strip_single_quotes(v));

        // Every "CCSR CATEGORY N" / "CCSR CATEGORY N DESCRIPTION" pair, plus
        // the two Default columns, contribute to this code's category set.
        let mut slots: Vec<(Option<&str>, usize, usize)> = Vec::new();
        if let (Some(ci), Some(di)) = (default_ip_idx, default_ip_desc_idx) {
            slots.push((Some("ip"), ci, di));
        }
        if let (Some(ci), Some(di)) = (default_op_idx, default_op_desc_idx) {
            slots.push((Some("op"), ci, di));
        }
        for n in 1..=6u32 {
            if let (Some(ci), Some(di)) = (
                idx(&format!("CCSR CATEGORY {n}")),
                idx(&format!("CCSR CATEGORY {n} DESCRIPTION")),
            ) {
                slots.push((None, ci, di));
            }
        }

        let entry = per_code.entry(entity.id).or_default();
        for (default_flag, ci, di) in slots {
            let Some(cell) = row.get(ci) else { continue };
            let cat = strip_single_quotes(cell);
            if cat.is_empty() {
                continue;
            }
            let desc = row
                .get(di)
                .map(|v| strip_single_quotes(v))
                .unwrap_or_default();
            if !desc.is_empty() {
                category_names.entry(cat.clone()).or_insert(desc);
            }
            let is_ip = default_flag == Some("ip") || default_ip.as_deref() == Some(cat.as_str());
            let is_op = default_flag == Some("op") || default_op.as_deref() == Some(cat.as_str());
            let e = entry.entry(cat).or_insert((false, false));
            e.0 |= is_ip;
            e.1 |= is_op;
        }
    }

    let cat_wanted: Vec<(String, Value, Option<String>)> = category_names
        .iter()
        .map(|(code, desc)| {
            (
                code.clone(),
                json!({"code": code, "desc": desc}),
                Some(desc.clone()),
            )
        })
        .collect();
    let (cat_ids, cat_created) =
        ensure_entities(runtime, token, "ccsr_category", cat_wanted).await?;

    let mut edges = EdgeAcc::new();
    for (code_entity_id, cats) in per_code {
        for (cat_code, (is_ip, is_op)) in cats {
            let Some(&cat_id) = cat_ids.get(&cat_code) else {
                unplaced.push(
                    "ccsr",
                    format!("category {cat_code}: entity missing after create"),
                );
                continue;
            };
            let mut extra = serde_json::Map::new();
            extra.insert("source".to_string(), json!("AHRQ DXCCSR v2026.1"));
            extra.insert("default_inpatient".to_string(), json!(is_ip));
            extra.insert("default_outpatient".to_string(), json!(is_op));
            edges.push(code_entity_id, cat_id, EdgeRelation::PartOf, "ccsr", extra);
        }
    }
    let edges_total = edges.flush(runtime, token).await?;

    Ok(json!({
        "created": {"ccsr_category": cat_created},
        "rows_read": rows_read,
        "edges_total": edges_total,
    }))
}

// ── CMS-HCC ─────────────────────────────────────────────────────────────

fn normalize_header_cell(cell: &str) -> String {
    cell.split_whitespace().collect::<Vec<_>>().join(" ")
}

async fn ingest_hcc(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    path: &str,
    unplaced: &mut Unplaced,
) -> Result<Value, RuntimeError> {
    let content = std::fs::read_to_string(path)
        .map_err(|e| RuntimeError::InvalidInput(format!("reading hcc_csv {path:?}: {e}")))?;
    let rows = parse_csv(&content);

    let header_row_idx = rows.iter().position(|r| {
        r.iter()
            .any(|c| normalize_header_cell(c).eq_ignore_ascii_case("Diagnosis Code"))
    });
    let Some(header_row_idx) = header_row_idx else {
        return Err(RuntimeError::InvalidInput(
            "hcc_csv: no header row found (expected a \"Diagnosis Code\" column)".to_string(),
        ));
    };
    let header: Vec<String> = rows[header_row_idx]
        .iter()
        .map(|c| normalize_header_cell(c))
        .collect();

    let diag_idx = header
        .iter()
        .position(|h| h.eq_ignore_ascii_case("Diagnosis Code"));
    let v24_idx = header
        .iter()
        .position(|h| h.contains("V24") && !h.contains("Payment Year"));
    let v28_idx = header
        .iter()
        .position(|h| h.contains("V28") && !h.contains("Payment Year"));

    let Some(diag_idx) = diag_idx else {
        return Err(RuntimeError::InvalidInput(
            "hcc_csv header missing \"Diagnosis Code\" column".to_string(),
        ));
    };

    // Pass 1: which entities each row resolves to, and which HCC natural
    // keys are needed — collected up front because `ensure_entities` (pass
    // 2) must run before an hcc entity id exists to link against.
    let mut hcc_names: HashSet<String> = HashSet::new();
    // (code entity id, [(model, hcc key)]) — captured once here so pass 2
    // never re-parses a row or re-resolves a code.
    let mut resolved_rows: Vec<(Uuid, Vec<(&'static str, String)>)> = Vec::new();
    let mut rows_read = 0usize;

    for row in rows.iter().skip(header_row_idx + 1) {
        if row.len() <= diag_idx {
            continue;
        }
        let diag_code = row[diag_idx].trim();
        if diag_code.is_empty() {
            continue;
        }
        rows_read += 1;
        let normalized = normalize_code(diag_code);
        let Some(entity) = find_by_code(runtime, token, &normalized).await? else {
            unplaced.push("hcc", format!("code {diag_code}: not in graph"));
            continue;
        };
        let mut row_keys: Vec<(&'static str, String)> = Vec::new();
        for (model, idx) in [("V24", v24_idx), ("V28", v28_idx)] {
            let Some(idx) = idx else { continue };
            let Some(val) = row.get(idx) else { continue };
            let val = val.trim();
            if val.is_empty() {
                continue;
            }
            let key = format!("{model}:{val}");
            hcc_names.insert(key.clone());
            row_keys.push((model, key));
        }
        resolved_rows.push((entity.id, row_keys));
    }

    let hcc_wanted: Vec<(String, Value, Option<String>)> = hcc_names
        .iter()
        .map(|key| {
            let (model, category) = key.split_once(':').unwrap_or((key.as_str(), ""));
            (
                key.clone(),
                json!({"model": model, "category": category}),
                Some(format!("HCC {category} ({model})")),
            )
        })
        .collect();
    let (hcc_ids, hcc_created) = ensure_entities(runtime, token, "hcc", hcc_wanted).await?;

    let mut real_edges = EdgeAcc::new();
    for (code_entity_id, row_keys) in &resolved_rows {
        for (model, key) in row_keys {
            let Some(&hcc_id) = hcc_ids.get(key) else {
                unplaced.push("hcc", format!("hcc key {key}: entity missing after create"));
                continue;
            };
            let mut extra = serde_json::Map::new();
            extra.insert("source".to_string(), json!("CMS-HCC 2026 midyear final"));
            extra.insert("model".to_string(), json!(model));
            real_edges.push(*code_entity_id, hcc_id, EdgeRelation::PartOf, "hcc", extra);
        }
    }
    let edges_total = real_edges.flush(runtime, token).await?;

    Ok(json!({
        "created": {"hcc": hcc_created},
        "rows_read": rows_read,
        "edges_total": edges_total,
    }))
}

// ── Guideline rules ─────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct RuleRow {
    id: String,
    section: String,
    quote: String,
    premises: Vec<String>,
    conclusions: Vec<String>,
    /// Present in the real `guideline-rules-FY2026.jsonl` (title/kind/provenance
    /// columns); absent from the two-row test fixture, which only exercises
    /// the five fields above. Kept only as entity properties (never
    /// interpreted) — no `deny_unknown_fields` on this struct either, so a
    /// future column addition to the source file stays non-fatal.
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    provenance: Option<String>,
}

async fn ingest_rules(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    path: &str,
    unplaced: &mut Unplaced,
) -> Result<Value, RuntimeError> {
    let content = std::fs::read_to_string(path)
        .map_err(|e| RuntimeError::InvalidInput(format!("reading rules_jsonl {path:?}: {e}")))?;

    let mut rows: Vec<RuleRow> = Vec::new();
    for (i, line) in content.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        match serde_json::from_str::<RuleRow>(line) {
            Ok(r) => rows.push(r),
            Err(e) => unplaced.push("rules", format!("line {}: invalid JSON: {e}", i + 1)),
        }
    }

    let rule_wanted: Vec<(String, Value, Option<String>)> = rows
        .iter()
        .map(|r| {
            (
                r.id.clone(),
                json!({
                    "section": r.section,
                    "quote": r.quote,
                    "title": r.title,
                    "kind": r.kind,
                    "provenance": r.provenance,
                }),
                Some(r.quote.clone()),
            )
        })
        .collect();
    let (rule_ids, rule_created) =
        ensure_entities(runtime, token, "guideline_rule", rule_wanted).await?;

    let mut edges = EdgeAcc::new();
    for r in &rows {
        let Some(&rule_id) = rule_ids.get(&r.id) else {
            continue;
        };
        for premise in &r.premises {
            let normalized = normalize_code(premise);
            match find_by_code(runtime, token, &normalized).await? {
                Some(target) => {
                    let mut extra = serde_json::Map::new();
                    extra.insert("section".to_string(), json!(r.section));
                    extra.insert("quote".to_string(), json!(r.quote));
                    edges.push(
                        rule_id,
                        target.id,
                        EdgeRelation::DependsOn,
                        "guideline_premise",
                        extra,
                    );
                }
                None => unplaced.push(
                    "rules",
                    format!("{}: premise {premise:?} not in graph", r.id),
                ),
            }
        }
        for conclusion in &r.conclusions {
            let normalized = normalize_code(conclusion);
            match find_by_code(runtime, token, &normalized).await? {
                Some(target) => {
                    let mut extra = serde_json::Map::new();
                    extra.insert("section".to_string(), json!(r.section));
                    extra.insert("quote".to_string(), json!(r.quote));
                    edges.push(
                        rule_id,
                        target.id,
                        EdgeRelation::Enables,
                        "guideline_conclusion",
                        extra,
                    );
                }
                None => unplaced.push(
                    "rules",
                    format!("{}: conclusion {conclusion:?} not in graph", r.id),
                ),
            }
        }
    }
    let edges_total = edges.flush(runtime, token).await?;

    Ok(json!({
        "created": {"guideline_rule": rule_created},
        "rows_read": rows.len(),
        "edges_total": edges_total,
    }))
}
