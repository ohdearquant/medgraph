//! `icd10.ingest` — parse the two CMS XML files and write khive records.
//!
//! Operates on the pack's own bound runtime (whichever db the host process
//! was started against, e.g. `kkernel exec --db <path>`); there is no `db`
//! parameter on this verb.

use std::collections::{HashMap, HashSet};
use std::time::Instant;

use khive_runtime::operations::{EntityCreateSpec, LinkSpec};
use khive_runtime::{KhiveRuntime, NamespaceToken, RuntimeError};
use khive_storage::entity::EntityFilter;
use khive_types::EdgeRelation;
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

use crate::model::{IndexParse, Laterality, Specificity, TabularParse, TermRec};
use crate::refs::{extract_code_refs, resolve_ref, RefKind};
use crate::{xml_index, xml_tabular};

const ENTITY_BATCH: usize = 1000;
const EDGE_BATCH: usize = 1000;
const LIST_PAGE: u32 = 1000;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct IngestParams {
    tabular_path: String,
    index_path: String,
    #[serde(default)]
    dry_run: bool,
}

pub async fn handle_ingest(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    params: Value,
) -> Result<Value, RuntimeError> {
    let started = Instant::now();
    let params: IngestParams = serde_json::from_value(params)
        .map_err(|e| RuntimeError::InvalidInput(format!("invalid icd10.ingest arguments: {e}")))?;

    let tabular_xml = std::fs::read_to_string(&params.tabular_path).map_err(|e| {
        RuntimeError::InvalidInput(format!(
            "reading tabular_path {:?}: {e}",
            params.tabular_path
        ))
    })?;
    let index_xml = std::fs::read_to_string(&params.index_path).map_err(|e| {
        RuntimeError::InvalidInput(format!("reading index_path {:?}: {e}", params.index_path))
    })?;

    let tabular = xml_tabular::parse_tabular(&tabular_xml)
        .map_err(|e| RuntimeError::InvalidInput(format!("parsing tabular XML: {e:#}")))?;
    let index = xml_index::parse_index(&index_xml)
        .map_err(|e| RuntimeError::InvalidInput(format!("parsing index XML: {e:#}")))?;

    let categories: HashSet<String> = tabular.categories.iter().map(|c| c.code.clone()).collect();
    let codes: HashSet<String> = tabular.codes.iter().map(|c| c.code.clone()).collect();
    let blocks: HashSet<String> = tabular.blocks.iter().map(|b| b.id.clone()).collect();

    let mut counts = json!({
        "icd_chapter": tabular.chapters.len(),
        "icd_block": tabular.blocks.len(),
        "icd_category": tabular.categories.len(),
        "icd_code": tabular.codes.len(),
        "icd_term": index.terms.len(),
    });

    let mut role_counts: HashMap<&'static str, usize> = HashMap::new();
    let mut unresolved: Vec<String> = Vec::new();
    let mut unresolved_count = 0usize;

    if params.dry_run {
        // Same resolution pass as a real ingest, minus every write, so the
        // dry-run counts (including unresolved refs) match a real run.
        let mut edge_acc: HashMap<(String, String, EdgeRelation), EdgeAccum> = HashMap::new();
        accumulate_hierarchy_dry(&tabular, &mut role_counts);
        accumulate_notes(
            &tabular,
            &categories,
            &codes,
            &blocks,
            &mut edge_acc,
            &mut role_counts,
            &mut unresolved,
            &mut unresolved_count,
        );
        accumulate_index_dry(
            &index,
            &categories,
            &codes,
            &mut role_counts,
            &mut unresolved,
            &mut unresolved_count,
        );
        return Ok(json!({
            "dry_run": true,
            "counts": counts,
            "edges_planned": role_counts,
            "unresolved": {"count": unresolved_count, "examples": first_n(&unresolved, 20)},
            "elapsed_ms": started.elapsed().as_millis(),
        }));
    }

    // ── Entities ────────────────────────────────────────────────────────
    // `description` is set on every entity (never left None): khive's entity
    // FTS/embedding document is built from name + description only
    // (`entity_fts_document`/`entity_embedding_text`, khive-runtime
    // curation.rs) — properties are unindexed. `icd10.resolve` depends on
    // hybrid_search reaching this text, so it is set here rather than
    // deferred.
    let mut chapter_wanted: Vec<(String, Value, Option<String>)> = Vec::new();
    for c in &tabular.chapters {
        chapter_wanted.push((
            c.title.clone(),
            json!({"number": c.number, "range": c.range}),
            Some(c.title.clone()),
        ));
    }
    let (chapter_ids, chapter_created) =
        ensure_entities(runtime, token, "icd_chapter", chapter_wanted).await?;

    let mut block_wanted: Vec<(String, Value, Option<String>)> = Vec::new();
    for b in &tabular.blocks {
        block_wanted.push((
            b.id.clone(),
            json!({"title": b.title, "range": b.id}),
            Some(b.title.clone()),
        ));
    }
    let (block_ids, block_created) =
        ensure_entities(runtime, token, "icd_block", block_wanted).await?;

    let mut category_wanted: Vec<(String, Value, Option<String>)> = Vec::new();
    for cat in &tabular.categories {
        let mut props = json!({
            "code": cat.code,
            "desc": cat.desc,
            "billable": cat.billable,
            "requires_7th": cat.requires_7th,
        });
        if let Some(sc) = &cat.seven_chr_def {
            props["seven_chr_def"] = serde_json::to_value(sc).unwrap_or(Value::Null);
        }
        category_wanted.push((cat.code.clone(), props, Some(cat.desc.clone())));
    }
    let (category_ids, category_created) =
        ensure_entities(runtime, token, "icd_category", category_wanted).await?;

    let mut code_wanted: Vec<(String, Value, Option<String>)> = Vec::new();
    for code in &tabular.codes {
        let description = if code.notes.inclusion_terms.is_empty() {
            code.desc.clone()
        } else {
            let mut parts = vec![code.desc.clone()];
            parts.extend(code.notes.inclusion_terms.iter().cloned());
            parts.join("; ")
        };
        code_wanted.push((
            code.code.clone(),
            json!({
                "code": code.code,
                "desc": code.desc,
                "billable": code.billable,
                "specificity": specificity_str(code.specificity),
                "laterality": code.laterality.map(laterality_str),
                "requires_7th": code.requires_7th,
                "inclusion_terms": code.notes.inclusion_terms,
                "chapter": code.chapter_range,
            }),
            Some(description),
        ));
    }
    let (code_ids, code_created) = ensure_entities(runtime, token, "icd_code", code_wanted).await?;

    let mut term_wanted: Vec<(String, Value, Option<String>)> = Vec::new();
    for t in &index.terms {
        term_wanted.push((
            t.path.clone(),
            json!({
                "title": t.title,
                "level": t.level,
                "nemod": t.nemod,
                "code": t.code_ptr,
            }),
            Some(t.title.clone()),
        ));
    }
    let (term_ids, term_created) = ensure_entities(runtime, token, "icd_term", term_wanted).await?;

    counts["icd_chapter"] = json!(chapter_ids.len());
    counts["icd_block"] = json!(block_ids.len());
    counts["icd_category"] = json!(category_ids.len());
    counts["icd_code"] = json!(code_ids.len());
    counts["icd_term"] = json!(term_ids.len());

    // ── Edges ───────────────────────────────────────────────────────────
    let mut edge_acc: HashMap<(Uuid, Uuid, EdgeRelation), EdgeAccum> = HashMap::new();

    // Hierarchy: chapter -> block -> category -> code -> code.
    for b in &tabular.blocks {
        if let (Some(&chapter_id), Some(&block_id)) = (
            chapter_by_range(&tabular, &chapter_ids, &b.chapter_range),
            block_ids.get(&b.id),
        ) {
            push_edge(
                &mut edge_acc,
                chapter_id,
                block_id,
                EdgeRelation::Contains,
                "hierarchy",
                None,
            );
            *role_counts.entry("hierarchy").or_insert(0) += 1;
        }
    }
    for cat in &tabular.categories {
        if let (Some(&block_id), Some(&cat_id)) =
            (block_ids.get(&cat.block_id), category_ids.get(&cat.code))
        {
            push_edge(
                &mut edge_acc,
                block_id,
                cat_id,
                EdgeRelation::Contains,
                "hierarchy",
                None,
            );
            *role_counts.entry("hierarchy").or_insert(0) += 1;
        }
    }
    for code in &tabular.codes {
        let source_id = if code.parent_is_category {
            category_ids.get(&code.parent_code).copied()
        } else {
            code_ids.get(&code.parent_code).copied()
        };
        if let (Some(source_id), Some(&target_id)) = (source_id, code_ids.get(&code.code)) {
            push_edge(
                &mut edge_acc,
                source_id,
                target_id,
                EdgeRelation::Contains,
                "hierarchy",
                None,
            );
            *role_counts.entry("hierarchy").or_insert(0) += 1;
        }
    }

    // Instructional notes -> excludes1/excludes2/code_also/code_first/use_additional.
    for code in &tabular.codes {
        let Some(&this_id) = code_ids.get(&code.code) else {
            continue;
        };
        resolve_notes_into(
            this_id,
            &code.code,
            &code.notes.excludes1,
            "excludes1",
            EdgeRelation::CompetesWith,
            false,
            &categories,
            &codes,
            &blocks,
            &category_ids,
            &code_ids,
            &mut edge_acc,
            &mut role_counts,
            &mut unresolved,
            &mut unresolved_count,
        );
        resolve_notes_into(
            this_id,
            &code.code,
            &code.notes.excludes2,
            "excludes2",
            EdgeRelation::ComposedWith,
            false,
            &categories,
            &codes,
            &blocks,
            &category_ids,
            &code_ids,
            &mut edge_acc,
            &mut role_counts,
            &mut unresolved,
            &mut unresolved_count,
        );
        resolve_notes_into(
            this_id,
            &code.code,
            &code.notes.code_also,
            "code_also",
            EdgeRelation::ComposedWith,
            false,
            &categories,
            &codes,
            &blocks,
            &category_ids,
            &code_ids,
            &mut edge_acc,
            &mut role_counts,
            &mut unresolved,
            &mut unresolved_count,
        );
        // Code first: referenced (etiology) -> this.
        resolve_notes_into(
            this_id,
            &code.code,
            &code.notes.code_first,
            "code_first",
            EdgeRelation::Precedes,
            true,
            &categories,
            &codes,
            &blocks,
            &category_ids,
            &code_ids,
            &mut edge_acc,
            &mut role_counts,
            &mut unresolved,
            &mut unresolved_count,
        );
        // Use additional code: this -> referenced.
        resolve_notes_into(
            this_id,
            &code.code,
            &code.notes.use_additional,
            "use_additional",
            EdgeRelation::Precedes,
            false,
            &categories,
            &codes,
            &blocks,
            &category_ids,
            &code_ids,
            &mut edge_acc,
            &mut role_counts,
            &mut unresolved,
            &mut unresolved_count,
        );
    }

    // Index: term -> subterm hierarchy, term -> code/category, term -> term (see/seeAlso).
    let see_targets = SeeTargets::new(&index.terms);

    for t in &index.terms {
        let Some(&this_id) = term_ids.get(&t.path) else {
            continue;
        };
        if let Some(parent_path) = &t.parent_path {
            if let Some(&parent_id) = term_ids.get(parent_path) {
                push_edge(
                    &mut edge_acc,
                    parent_id,
                    this_id,
                    EdgeRelation::Contains,
                    "index_hierarchy",
                    None,
                );
                *role_counts.entry("index_hierarchy").or_insert(0) += 1;
            }
        }
        if let Some(ptr) = &t.code_ptr {
            match resolve_ref(ptr, &categories, &codes, &blocks) {
                Some(r) if r.kind != RefKind::Block => {
                    let target = if r.kind == RefKind::Code {
                        code_ids.get(&r.key).copied()
                    } else {
                        category_ids.get(&r.key).copied()
                    };
                    if let Some(target_id) = target {
                        push_edge(
                            &mut edge_acc,
                            this_id,
                            target_id,
                            EdgeRelation::VariantOf,
                            "index",
                            Some(ptr.clone()),
                        );
                        *role_counts.entry("index").or_insert(0) += 1;
                    } else {
                        unresolved.push(format!("index code_ptr {ptr:?} (term {:?})", t.path));
                        unresolved_count += 1;
                    }
                }
                _ => {
                    unresolved.push(format!("index code_ptr {ptr:?} (term {:?})", t.path));
                    unresolved_count += 1;
                }
            }
        }
        if let Some(see) = &t.see {
            resolve_term_link(
                this_id,
                see,
                &see_targets,
                &term_ids,
                EdgeRelation::LinksTo,
                "see",
                &mut edge_acc,
                &mut role_counts,
                &mut unresolved,
                &mut unresolved_count,
            );
        }
        for sa in &t.see_also {
            resolve_term_link(
                this_id,
                sa,
                &see_targets,
                &term_ids,
                EdgeRelation::LinksTo,
                "see_also",
                &mut edge_acc,
                &mut role_counts,
                &mut unresolved,
                &mut unresolved_count,
            );
        }
    }

    let total_edges = edge_acc.len();
    let mut batch: Vec<LinkSpec> = Vec::with_capacity(EDGE_BATCH);
    for ((source_id, target_id, relation), acc) in edge_acc {
        let mut metadata = json!({"icd": acc.icd});
        if !acc.text.is_empty() {
            metadata["text"] = json!(acc.text);
        }
        batch.push(LinkSpec {
            namespace: None,
            source_id,
            target_id,
            relation,
            weight: 1.0,
            metadata: Some(metadata),
            resurrect: true,
        });
        if batch.len() >= EDGE_BATCH {
            runtime.link_many(token, std::mem::take(&mut batch)).await?;
        }
    }
    if !batch.is_empty() {
        runtime.link_many(token, batch).await?;
    }

    Ok(json!({
        "dry_run": false,
        "counts": counts,
        "created": {
            "icd_chapter": chapter_created,
            "icd_block": block_created,
            "icd_category": category_created,
            "icd_code": code_created,
            "icd_term": term_created,
        },
        "edges": role_counts,
        "edges_total": total_edges,
        "unresolved": {"count": unresolved_count, "examples": first_n(&unresolved, 20)},
        "elapsed_ms": started.elapsed().as_millis(),
        "embedded_vectors": false,
    }))
}

fn first_n(v: &[String], n: usize) -> Vec<String> {
    v.iter().take(n).cloned().collect()
}

fn specificity_str(s: Specificity) -> &'static str {
    s.as_str()
}

fn laterality_str(l: Laterality) -> &'static str {
    l.as_str()
}

fn chapter_by_range<'a>(
    tabular: &TabularParse,
    chapter_ids: &'a HashMap<String, Uuid>,
    range: &str,
) -> Option<&'a Uuid> {
    let title = tabular
        .chapters
        .iter()
        .find(|c| c.range == range)?
        .title
        .as_str();
    chapter_ids.get(title)
}

struct EdgeAccum {
    icd: Vec<String>,
    text: Vec<String>,
}

fn push_edge(
    map: &mut HashMap<(Uuid, Uuid, EdgeRelation), EdgeAccum>,
    source: Uuid,
    target: Uuid,
    relation: EdgeRelation,
    role: &str,
    text: Option<String>,
) {
    let entry = map
        .entry((source, target, relation))
        .or_insert_with(|| EdgeAccum {
            icd: Vec::new(),
            text: Vec::new(),
        });
    if !entry.icd.iter().any(|r| r == role) {
        entry.icd.push(role.to_string());
    }
    if let Some(t) = text {
        if !entry.text.iter().any(|x| x == &t) {
            entry.text.push(t);
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn resolve_notes_into(
    this_id: Uuid,
    this_code: &str,
    notes: &[String],
    role: &'static str,
    relation: EdgeRelation,
    reverse_direction: bool,
    categories: &HashSet<String>,
    codes: &HashSet<String>,
    blocks: &HashSet<String>,
    category_ids: &HashMap<String, Uuid>,
    code_ids: &HashMap<String, Uuid>,
    edge_acc: &mut HashMap<(Uuid, Uuid, EdgeRelation), EdgeAccum>,
    role_counts: &mut HashMap<&'static str, usize>,
    unresolved: &mut Vec<String>,
    unresolved_count: &mut usize,
) {
    for note in notes {
        let refs = extract_code_refs(note);
        if refs.is_empty() {
            continue;
        }
        for tok in refs {
            match resolve_ref(&tok, categories, codes, blocks) {
                Some(r) if r.kind != RefKind::Block => {
                    let target_id = if r.kind == RefKind::Code {
                        code_ids.get(&r.key).copied()
                    } else {
                        category_ids.get(&r.key).copied()
                    };
                    let Some(target_id) = target_id else {
                        unresolved.push(format!(
                            "{role} {tok:?} on {this_code} (no entity for {})",
                            r.key
                        ));
                        *unresolved_count += 1;
                        continue;
                    };
                    if target_id == this_id {
                        continue;
                    }
                    let (source_id, dest_id) = if reverse_direction {
                        (target_id, this_id)
                    } else {
                        (this_id, target_id)
                    };
                    push_edge(
                        edge_acc,
                        source_id,
                        dest_id,
                        relation,
                        role,
                        Some(note.clone()),
                    );
                    *role_counts.entry(role).or_insert(0) += 1;
                }
                _ => {
                    unresolved.push(format!("{role} {tok:?} on {this_code}"));
                    *unresolved_count += 1;
                }
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
/// Resolves the text of a `see` / `see also` reference to the one Index term
/// it names. CMS writes the target as the term's levels separated by commas,
/// in the term's own words without nonessential modifiers (`Examination` for
/// the main term `Examination (for) (following) (general) (of) (routine)`;
/// `Injury, nerve, hand` for `Injury > nerve > hand`), and a main term whose
/// title lists several forms (`Malaria, malarial`) may be cited by its first
/// form. The exact full path and the first-comma split are tried first. A
/// target that names no term, or more than one, stays unresolved.
struct SeeTargets<'a> {
    by_path: HashMap<String, &'a str>,
    by_levels: HashMap<Vec<String>, Vec<&'a str>>,
    main_forms: HashMap<String, HashSet<String>>,
}

impl<'a> SeeTargets<'a> {
    fn new(terms: &'a [TermRec]) -> Self {
        let by_rec: HashMap<&str, &TermRec> = terms.iter().map(|t| (t.path.as_str(), t)).collect();
        let mut by_path = HashMap::new();
        let mut by_levels: HashMap<Vec<String>, Vec<&'a str>> = HashMap::new();
        let mut main_forms: HashMap<String, HashSet<String>> = HashMap::new();
        for t in terms {
            by_path.insert(t.path.to_lowercase(), t.path.as_str());
            let mut levels = vec![t.plain_title.to_lowercase()];
            let mut parent = t.parent_path.as_deref();
            while let Some(pp) = parent {
                let Some(rec) = by_rec.get(pp) else { break };
                levels.push(rec.plain_title.to_lowercase());
                parent = rec.parent_path.as_deref();
            }
            levels.reverse();
            if levels.len() == 1 {
                let main = levels[0].clone();
                let first = main.split(',').next().unwrap_or(&main).trim().to_string();
                main_forms.entry(first).or_default().insert(main.clone());
                main_forms.entry(main.clone()).or_default().insert(main);
            }
            let entry = by_levels.entry(levels).or_default();
            if !entry.contains(&t.path.as_str()) {
                entry.push(t.path.as_str());
            }
        }
        Self {
            by_path,
            by_levels,
            main_forms,
        }
    }

    fn resolve(&self, target_text: &str) -> Option<&'a str> {
        if let Some(p) = self.by_path.get(&target_text.trim().to_lowercase()) {
            return Some(p);
        }
        if let Some((head, tail)) = target_text.split_once(',') {
            let candidate = format!("{} > {}", head.trim(), tail.trim()).to_lowercase();
            if let Some(p) = self.by_path.get(&candidate) {
                return Some(p);
            }
        }
        let parts: Vec<String> = target_text
            .split(',')
            .map(|p| {
                p.split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ")
                    .to_lowercase()
            })
            .filter(|p| !p.is_empty())
            .collect();
        let mut hits: HashSet<&'a str> = HashSet::new();
        for k in 1..=parts.len() {
            let Some(mains) = self.main_forms.get(&parts[..k].join(", ")) else {
                continue;
            };
            for main in mains {
                let mut key = vec![main.clone()];
                key.extend(parts[k..].iter().cloned());
                if let Some(paths) = self.by_levels.get(&key) {
                    hits.extend(paths.iter().copied());
                }
            }
        }
        if hits.len() == 1 {
            hits.into_iter().next()
        } else {
            None
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn resolve_term_link(
    this_id: Uuid,
    target_text: &str,
    see_targets: &SeeTargets<'_>,
    term_ids: &HashMap<String, Uuid>,
    relation: EdgeRelation,
    role: &'static str,
    edge_acc: &mut HashMap<(Uuid, Uuid, EdgeRelation), EdgeAccum>,
    role_counts: &mut HashMap<&'static str, usize>,
    unresolved: &mut Vec<String>,
    unresolved_count: &mut usize,
) {
    let target_id = see_targets
        .resolve(target_text)
        .and_then(|path| term_ids.get(path).copied());
    match target_id {
        Some(target_id) if target_id != this_id => {
            push_edge(
                edge_acc,
                this_id,
                target_id,
                relation,
                role,
                Some(target_text.to_string()),
            );
            *role_counts.entry(role).or_insert(0) += 1;
        }
        Some(_) => {}
        None => {
            unresolved.push(format!("{role} -> {target_text:?}"));
            *unresolved_count += 1;
        }
    }
}

/// List every existing entity of `entity_type`, then create whatever in
/// `wanted` is missing by name. Returns the full name -> id map (existing
/// plus newly created) and how many were newly created.
///
/// `pub(crate)`: shared with `ingest_external.rs`, which follows the same
/// deterministic-id / idempotent-re-run pattern for the medical-graph
/// subtypes (`drug`, `mesh_disease`, `ccsr_category`, `hcc`,
/// `guideline_rule`) rather than re-deriving it.
pub(crate) async fn ensure_entities(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    entity_type: &'static str,
    wanted: Vec<(String, Value, Option<String>)>,
) -> Result<(HashMap<String, Uuid>, usize), RuntimeError> {
    let mut existing: HashMap<String, Uuid> = HashMap::new();
    let mut offset = 0u32;
    loop {
        let filter = EntityFilter {
            entity_types: vec![entity_type.to_string()],
            ..Default::default()
        };
        let page = runtime
            .list_entities_filtered(token, filter, LIST_PAGE, offset)
            .await?;
        let n = page.len();
        for e in page {
            existing.entry(e.name.clone()).or_insert(e.id);
        }
        if n < LIST_PAGE as usize {
            break;
        }
        offset += LIST_PAGE;
    }

    let mut seen_names: HashSet<String> = HashSet::new();
    let mut created = 0usize;
    let mut batch: Vec<EntityCreateSpec> = Vec::with_capacity(ENTITY_BATCH);
    for (name, properties, description) in wanted {
        if !seen_names.insert(name.clone()) {
            continue;
        }
        if existing.contains_key(&name) {
            continue;
        }
        batch.push(EntityCreateSpec {
            kind: "concept".to_string(),
            entity_type: Some(entity_type.to_string()),
            name,
            description,
            properties: Some(properties),
            tags: vec!["icd10".to_string()],
        });
        if batch.len() >= ENTITY_BATCH {
            let created_entities = runtime
                .create_many(token, std::mem::take(&mut batch))
                .await?;
            created += created_entities.len();
            for e in created_entities {
                existing.insert(e.name.clone(), e.id);
            }
        }
    }
    if !batch.is_empty() {
        let created_entities = runtime.create_many(token, batch).await?;
        created += created_entities.len();
        for e in created_entities {
            existing.insert(e.name.clone(), e.id);
        }
    }
    Ok((existing, created))
}

fn accumulate_hierarchy_dry(
    tabular: &TabularParse,
    role_counts: &mut HashMap<&'static str, usize>,
) {
    *role_counts.entry("hierarchy").or_insert(0) +=
        tabular.blocks.len() + tabular.categories.len() + tabular.codes.len();
}

#[allow(clippy::too_many_arguments)]
fn accumulate_notes(
    tabular: &TabularParse,
    categories: &HashSet<String>,
    codes: &HashSet<String>,
    blocks: &HashSet<String>,
    edge_acc: &mut HashMap<(String, String, EdgeRelation), EdgeAccum>,
    role_counts: &mut HashMap<&'static str, usize>,
    unresolved: &mut Vec<String>,
    unresolved_count: &mut usize,
) {
    let _ = edge_acc; // dry-run reuses only the resolution/counting side effects
    for code in &tabular.codes {
        for (notes, role) in [
            (&code.notes.excludes1, "excludes1"),
            (&code.notes.excludes2, "excludes2"),
            (&code.notes.code_also, "code_also"),
            (&code.notes.code_first, "code_first"),
            (&code.notes.use_additional, "use_additional"),
        ] {
            for note in notes {
                for tok in extract_code_refs(note) {
                    match resolve_ref(&tok, categories, codes, blocks) {
                        Some(r) if r.kind != RefKind::Block => {
                            *role_counts.entry(role).or_insert(0) += 1;
                        }
                        _ => {
                            unresolved.push(format!("{role} {tok:?} on {}", code.code));
                            *unresolved_count += 1;
                        }
                    }
                }
            }
        }
    }
}

fn accumulate_index_dry(
    index: &IndexParse,
    categories: &HashSet<String>,
    codes: &HashSet<String>,
    role_counts: &mut HashMap<&'static str, usize>,
    unresolved: &mut Vec<String>,
    unresolved_count: &mut usize,
) {
    let blocks: HashSet<String> = HashSet::new();
    let see_targets = SeeTargets::new(&index.terms);
    for t in &index.terms {
        if t.parent_path.is_some() {
            *role_counts.entry("index_hierarchy").or_insert(0) += 1;
        }
        if let Some(ptr) = &t.code_ptr {
            match resolve_ref(ptr, categories, codes, &blocks) {
                Some(r) if r.kind != RefKind::Block => {
                    *role_counts.entry("index").or_insert(0) += 1;
                }
                _ => {
                    unresolved.push(format!("index code_ptr {ptr:?} (term {:?})", t.path));
                    *unresolved_count += 1;
                }
            }
        }
        if let Some(see) = &t.see {
            count_term_link(
                see,
                &see_targets,
                "see",
                role_counts,
                unresolved,
                unresolved_count,
            );
        }
        for sa in &t.see_also {
            count_term_link(
                sa,
                &see_targets,
                "see_also",
                role_counts,
                unresolved,
                unresolved_count,
            );
        }
    }
}

fn count_term_link(
    target_text: &str,
    see_targets: &SeeTargets<'_>,
    role: &'static str,
    role_counts: &mut HashMap<&'static str, usize>,
    unresolved: &mut Vec<String>,
    unresolved_count: &mut usize,
) {
    let resolved = see_targets.resolve(target_text).is_some();
    if resolved {
        *role_counts.entry(role).or_insert(0) += 1;
    } else {
        unresolved.push(format!("{role} -> {target_text:?}"));
        *unresolved_count += 1;
    }
}

#[cfg(test)]
mod see_target_tests {
    use super::SeeTargets;
    use crate::model::TermRec;

    fn term(path: &str, plain_title: &str, parent: Option<&str>) -> TermRec {
        TermRec {
            path: path.to_string(),
            title: path.rsplit(" > ").next().unwrap_or(path).to_string(),
            plain_title: plain_title.to_string(),
            letter: path[..1].to_string(),
            level: path.matches(" > ").count() as i32,
            nemod: Vec::new(),
            code_ptr: None,
            see: None,
            see_also: Vec::new(),
            parent_path: parent.map(str::to_string),
        }
    }

    fn fixture() -> Vec<TermRec> {
        let exam = "Examination (for) (routine)";
        let injury = "Injury (traumatic)";
        let malaria = "Malaria, malarial (fever)";
        let gout = "Gout, chronic";
        let gouty = "Gout, gouty";
        vec![
            term(exam, "Examination", None),
            term(
                &format!("{exam} > child (over 28 days old)"),
                "child",
                Some(exam),
            ),
            term(injury, "Injury", None),
            term(&format!("{injury} > nerve"), "nerve", Some(injury)),
            term(
                &format!("{injury} > nerve > hand"),
                "hand",
                Some(&format!("{injury} > nerve")),
            ),
            term(malaria, "Malaria, malarial", None),
            term(gout, "Gout, chronic", None),
            term(gouty, "Gout, gouty", None),
        ]
    }

    #[test]
    fn see_text_resolves_by_plain_title_comma_levels_and_first_form() {
        let terms = fixture();
        let t = SeeTargets::new(&terms);
        assert_eq!(
            t.resolve("Examination"),
            Some("Examination (for) (routine)")
        );
        assert_eq!(
            t.resolve("Injury, nerve, hand"),
            Some("Injury (traumatic) > nerve > hand")
        );
        assert_eq!(t.resolve("Malaria"), Some("Malaria, malarial (fever)"));
        // exact full path still wins
        assert_eq!(
            t.resolve("Examination (for) (routine) > child (over 28 days old)"),
            Some("Examination (for) (routine) > child (over 28 days old)")
        );
    }

    #[test]
    fn see_text_naming_two_terms_or_none_stays_unresolved() {
        let terms = fixture();
        let t = SeeTargets::new(&terms);
        assert_eq!(
            t.resolve("Gout"),
            None,
            "two main terms share the first form"
        );
        assert_eq!(t.resolve("Injury, nerve, foot"), None);
    }
}
