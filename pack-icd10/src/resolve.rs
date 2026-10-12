//! `icd10.resolve(mentions, sex?, age_years?, per_arm_limit?, expand_family?,
//! text_mode?, text_arms?, dense?, dense_mode?)` — candidate ICD-10-CM codes
//! for free-text problem mentions, via the index (Alphabetic Index term
//! search), tabular (code description search), and vector arms, plus family
//! expansion.
//!
//! No language-model calls. The index, tabular and category arms share one
//! full-text search per mention, run by the pack (`crate::search`), never
//! `KhiveRuntime::hybrid_search`: that call restricts by entity type through
//! a filtered list that reads the whole namespace, once per arm.
//!
//! The vector arm (`dense: true`) embeds the mention once with the store's
//! default model and takes its nearest Index terms, codes and billable
//! categories from the three ICD-10-CM entity families (`crate::ann`), each
//! searched on its own so the mention's neighbours are ICD-10-CM entries and
//! nothing else in the store. A term hit is followed through its pointer as
//! the index arm follows it; a code hit is a candidate as a tabular hit is.
//! It needs the vectors (`icd10.embed`) and the family indexes
//! (`icd10.index`), and says so when they are missing. `dense_mode` is
//! `exact` (every vector of the family, default) or `ann` (the Vamana graph);
//! `text_arms: false` leaves the vector arm alone. Exact is the default
//! because it is the true ranking and costs about 6 ms per family at this
//! size; the graph answers in under 1 ms and loses a neighbour now and then
//! (`icd10.index(action="parity")` measures how often).
//!
//! `text_mode`: `"all_terms"` (default) is `hybrid_search`'s text ranking,
//! `TextQueryMode::Plain`: an implicit AND over every whitespace token in the
//! mention, scored by rank with the exact-title boost. `"any_term"` runs
//! `TextQueryMode::AnyTerm` (OR-join) in the text index's own order. Either
//! way the hits are restricted to an arm's entity type by reading each
//! entity (`TextFilter.record_kinds` cannot do it: the index's `record_kind`
//! is the entity kind, `"concept"` for every icd10 subtype). Family
//! expansion, sex/age and the unknown list are unchanged by `text_mode`.
//!
//! The text index is trigram, so a mention's one- and two-digit words ("2" in "type 2") never
//! constrain a text arm. Candidates that carry every such digit in their description or Index
//! path are ordered ahead of those that do not; nothing is removed.

use std::collections::{HashMap, HashSet};

use khive_runtime::{
    EdgeListFilter, KhiveRuntime, NamespaceToken, RuntimeError, SearchHit, SearchSource,
};
use khive_storage::types::{Direction, TextQueryMode, TraversalOptions, VectorSearchHit};
use khive_storage::Entity;
use khive_types::EdgeRelation;
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

use crate::ann::{self, Mode};
use crate::check::{chapter_range_of, excludes1_codes, prop_bool};
use crate::neighborhood::partner_codes;

const DEFAULT_PER_ARM_LIMIT: u32 = 20;

/// Mirrors the private `CANDIDATE_MULTIPLIER` in
/// `khive-runtime/src/retrieval.rs` (not `pub`, so not importable): the
/// `any_term` text search over-fetches this many times `per_arm_limit`
/// before the entity-type alive-set filter and truncation, matching
/// `hybrid_search_inner`'s own over-fetch shape.
const ANY_TERM_CANDIDATE_MULTIPLIER: u32 = 4;

/// Which `TextQueryMode` the index/tabular arms run under. `AllTerms` is
/// `hybrid_search`'s existing behavior (implicit AND, `TextQueryMode::Plain`);
/// `AnyTerm` runs `TextQueryMode::AnyTerm` (OR-join) directly against the
/// text store. See the module doc for why `AnyTerm` restricts candidates via
/// an entity-type alive-set query rather than `TextFilter.record_kinds`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum TextMode {
    AllTerms,
    AnyTerm,
}

impl TextMode {
    fn as_str(self) -> &'static str {
        match self {
            TextMode::AllTerms => "all_terms",
            TextMode::AnyTerm => "any_term",
        }
    }
}

fn default_text_mode() -> TextMode {
    TextMode::AllTerms
}

#[derive(Debug, Deserialize)]
struct MentionIn {
    text: String,
    #[serde(default)]
    #[allow(dead_code)] // carried through the params shape; not read by resolve itself.
    evidence: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ResolveParams {
    mentions: Vec<MentionIn>,
    // Accepted for signature parity with `icd10.exclude` and forward
    // compatibility; resolve is pure retrieval and applies no sex/age
    // filtering itself — that is `icd10.exclude`'s E2/E3.
    #[serde(default)]
    #[allow(dead_code)]
    sex: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    age_years: Option<f64>,
    #[serde(default = "default_per_arm_limit")]
    per_arm_limit: u32,
    #[serde(default = "default_true")]
    expand_family: bool,
    #[serde(default = "default_text_mode")]
    text_mode: TextMode,
    #[serde(default = "default_true")]
    text_arms: bool,
    #[serde(default)]
    dense: bool,
    #[serde(default = "default_dense_mode")]
    dense_mode: Mode,
}

fn default_dense_mode() -> Mode {
    Mode::Exact
}

/// Which arms one resolve runs.
#[derive(Debug, Clone, Copy)]
struct Arms {
    /// The index, tabular and category text arms.
    text: bool,
    /// The vector arm, and how its families are searched.
    dense: Option<Mode>,
}

fn default_per_arm_limit() -> u32 {
    DEFAULT_PER_ARM_LIMIT
}

fn default_true() -> bool {
    true
}

/// One arm/via hit contributing to a candidate.
#[derive(Debug, Clone)]
struct Match {
    arm: &'static str,
    via: &'static str,
    term_path: Option<String>,
    rank: u32,
    score: f64,
}

struct CandidateAcc {
    entity: Entity,
    matched: Vec<Match>,
}

pub async fn handle_resolve(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    params: Value,
) -> Result<Value, RuntimeError> {
    let params: ResolveParams = serde_json::from_value(params)
        .map_err(|e| RuntimeError::InvalidInput(format!("invalid icd10.resolve arguments: {e}")))?;

    if !params.text_arms && !params.dense {
        return Err(RuntimeError::InvalidInput(
            "icd10.resolve: text_arms is false and dense is false, so no arm would run".into(),
        ));
    }
    let arms = Arms {
        text: params.text_arms,
        dense: params.dense.then_some(params.dense_mode),
    };
    let mut mention_results = Vec::with_capacity(params.mentions.len());
    for mention in &params.mentions {
        let result = resolve_one_mention(
            runtime,
            token,
            &mention.text,
            params.per_arm_limit,
            params.expand_family,
            params.text_mode,
            arms,
            false,
        )
        .await?;
        mention_results.push(result);
    }

    Ok(json!({
        "mentions": mention_results,
        "vector_arm": arms.dense.map_or("off", Mode::as_str),
    }))
}

/// The resolve `icd10.verify` runs for reachability: `any_term`, family
/// expansion on, and the light render. Verify reads only each candidate's
/// `code`, `desc` and `matched` (arm, via, term_path), so the per-candidate
/// excludes1 / code-first / use-additional lookups and the chapter walk are
/// skipped: three or more store reads per candidate that nothing downstream
/// reads (measured 2026-09-30: 147 candidates, 21 s with them). The candidate
/// set and every field verify reads are the same as `handle_resolve`'s.
pub(crate) async fn resolve_for_reachability(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    text: &str,
    per_arm_limit: Option<u32>,
) -> Result<Value, RuntimeError> {
    let result = resolve_one_mention(
        runtime,
        token,
        text,
        per_arm_limit.unwrap_or(DEFAULT_PER_ARM_LIMIT),
        true,
        TextMode::AnyTerm,
        Arms {
            text: true,
            dense: None,
        },
        true,
    )
    .await?;
    Ok(json!({ "mentions": [result] }))
}

#[allow(clippy::too_many_arguments)]
async fn resolve_one_mention(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    text: &str,
    per_arm_limit: u32,
    expand_family: bool,
    text_mode: TextMode,
    arms: Arms,
    light: bool,
) -> Result<Value, RuntimeError> {
    let mut candidates: HashMap<Uuid, CandidateAcc> = HashMap::new();
    let mut chapters_reached: HashSet<String> = HashSet::new();

    // One text search per mention: the three arms below differ only in the
    // entity type they keep from it. With the text arms off the ranking is
    // empty and each of them finds nothing.
    let ranked = if arms.text {
        ranked_hits(runtime, token, text, per_arm_limit, text_mode).await?
    } else {
        Vec::new()
    };

    // ── 1. Index arm: text search over icd_term. ───────────────────────────
    let term_hits = typed_hits(runtime, token, &ranked, "icd_term", per_arm_limit).await?;
    let index_hit_count = term_hits.len();

    for (i, hit) in term_hits.iter().enumerate() {
        let rank = (i + 1) as u32;
        let score = hit.score.to_f64();
        let term_path = hit.title.clone();
        let term_id = hit.entity_id;

        // Primary pointer: term -[variant_of]-> code/category.
        let primary_targets = variant_of_targets(runtime, token, term_id).await?;
        if !light {
            for target_id in &primary_targets {
                if let Some(chapter) = chapter_range_of(runtime, token, *target_id).await? {
                    chapters_reached.insert(chapter);
                }
            }
        }
        add_resolved_targets(
            runtime,
            token,
            &primary_targets,
            "index",
            "direct",
            "family",
            term_path.clone(),
            rank,
            score,
            expand_family,
            &mut candidates,
        )
        .await?;

        // One hop through see/see_also: this term -[links_to]-> other term,
        // then that term's own variant_of pointer(s), marked via=see.
        let linked_terms = crate::memo::list_edges(
            runtime,
            token,
            EdgeListFilter {
                source_id: Some(term_id),
                relations: vec![EdgeRelation::LinksTo],
                ..Default::default()
            },
            1000,
            0,
        )
        .await?;
        for link in linked_terms {
            let see_targets = variant_of_targets(runtime, token, link.target_id).await?;
            add_resolved_targets(
                runtime,
                token,
                &see_targets,
                "index",
                "see",
                "see",
                term_path.clone(),
                rank,
                score,
                true, // a see-hop pointer to a non-billable target still needs expanding
                &mut candidates,
            )
            .await?;
            if is_see_link(&link) {
                follow_see_into_subterms(
                    runtime,
                    token,
                    text,
                    term_path.as_deref(),
                    link.target_id,
                    rank,
                    score,
                    expand_family,
                    &mut candidates,
                )
                .await?;
            }
        }
    }

    // ── 2. Tabular arm: text search over icd_code. ─────────────────────────
    let code_hits = typed_hits(runtime, token, &ranked, "icd_code", per_arm_limit).await?;
    let tabular_hit_count = code_hits.len();
    let code_ids: Vec<Uuid> = code_hits.iter().map(|h| h.entity_id).collect();
    if !code_ids.is_empty() {
        let entities = crate::memo::entities_by_ids(runtime, token, &code_ids).await?;
        let by_id: HashMap<Uuid, Entity> = entities.into_iter().map(|e| (e.id, e)).collect();
        for (i, hit) in code_hits.iter().enumerate() {
            let Some(entity) = by_id.get(&hit.entity_id).cloned() else {
                continue;
            };
            let rank = (i + 1) as u32;
            let score = hit.score.to_f64();
            upsert_candidate(
                &mut candidates,
                entity,
                Match {
                    arm: "tabular",
                    via: "direct",
                    term_path: None,
                    rank,
                    score,
                },
            );
        }
    }

    // ── 2b. Tabular arm, billable-category leaves: a childless top-level
    //    category with no `sevenChrDef` (e.g. real codes I10, O80, A09) is
    //    billable directly and has NO separate `icd_code` entity — by
    //    design, `find_by_code`/`icd10.check`/`icd10.neighborhood` already
    //    treat such an `icd_category` (billable: true) as the code itself
    //    (see `integration.rs::leaf_category_is_billable_and_checked_directly`).
    //    The tabular arm's own text search was the one place still missing
    //    that fallback — restricted to `entity_type: "icd_code"` alone, a
    //    mention naming exactly such a category (e.g. "hypertension" for
    //    I10) never surfaced it as a candidate at all. Same search, same
    //    upsert, `entity_type: "icd_category"` and a `billable` post-filter
    //    (a non-billable category is reached through family expansion off a
    //    billable sibling instead, never as a direct tabular hit).
    let category_hits = typed_hits(runtime, token, &ranked, "icd_category", per_arm_limit).await?;
    let category_ids: Vec<Uuid> = category_hits.iter().map(|h| h.entity_id).collect();
    if !category_ids.is_empty() {
        let entities = crate::memo::entities_by_ids(runtime, token, &category_ids).await?;
        let by_id: HashMap<Uuid, Entity> = entities.into_iter().map(|e| (e.id, e)).collect();
        for (i, hit) in category_hits.iter().enumerate() {
            let Some(entity) = by_id.get(&hit.entity_id).cloned() else {
                continue;
            };
            let is_billable = entity
                .properties
                .as_ref()
                .and_then(|p| p.get("billable"))
                .and_then(Value::as_bool)
                .unwrap_or(false);
            if !is_billable {
                continue;
            }
            let rank = (i + 1) as u32;
            let score = hit.score.to_f64();
            upsert_candidate(
                &mut candidates,
                entity,
                Match {
                    arm: "tabular",
                    via: "direct",
                    term_path: None,
                    rank,
                    score,
                },
            );
        }
    }

    // ── 3. Vector arm: the mention's nearest Index terms, codes and billable
    //    categories, each from its own family index. ───────────────────────
    let mut vector_hit_count = 0usize;
    if let Some(mode) = arms.dense {
        let q = ann::embed_query(runtime, text).await?;

        // 3a. Index terms: each term's pointer, as the index arm reads it.
        let term_hits = dense_hits(runtime, token, &ann::ICD_TERM, &q, per_arm_limit, mode).await?;
        vector_hit_count += term_hits.len();
        for hit in &term_hits {
            let Some(term) = fetch_one(runtime, token, hit.subject_id).await? else {
                continue;
            };
            let targets = variant_of_targets(runtime, token, term.id).await?;
            if !light {
                for target_id in &targets {
                    if let Some(chapter) = chapter_range_of(runtime, token, *target_id).await? {
                        chapters_reached.insert(chapter);
                    }
                }
            }
            add_resolved_targets(
                runtime,
                token,
                &targets,
                "vector",
                "direct",
                "family",
                Some(term.name.clone()),
                hit.rank,
                hit.score.to_f64(),
                expand_family,
                &mut candidates,
            )
            .await?;
        }

        // 3b. Codes, and 3c. billable categories (a childless category is
        //     its own code, as in the tabular arm).
        for (family, billable_only) in [(&ann::ICD_CODE, false), (&ann::ICD_CATEGORY, true)] {
            let hits = dense_hits(runtime, token, family, &q, per_arm_limit, mode).await?;
            vector_hit_count += hits.len();
            for hit in &hits {
                let Some(entity) = fetch_one(runtime, token, hit.subject_id).await? else {
                    continue;
                };
                if billable_only && !prop_bool(&entity, "billable") {
                    continue;
                }
                upsert_candidate(
                    &mut candidates,
                    entity,
                    Match {
                        arm: "vector",
                        via: "direct",
                        term_path: None,
                        rank: hit.rank,
                        score: hit.score.to_f64(),
                    },
                );
            }
        }
    }

    // ── Family expansion over the whole candidate set so far. ─────────────
    if expand_family {
        let mut hit_code_ids: Vec<Uuid> = candidates
            .values()
            .filter(|c| c.entity.entity_type.as_deref() == Some("icd_code"))
            .map(|c| c.entity.id)
            .collect();
        // `candidates` is a HashMap: walk the hit codes in a fixed order so the
        // order of each sibling's `matched` entries is the same on every call.
        hit_code_ids.sort();
        for code_id in hit_code_ids {
            let Some(parent_id) = immediate_parent(runtime, token, code_id).await? else {
                continue;
            };
            // Find which arm/rank/score originated this code, to inherit for
            // the family-derived siblings (they are not hits in their own
            // right).
            let origin = candidates
                .get(&code_id)
                .and_then(|c| c.matched.first().cloned());
            let Some(origin) = origin else { continue };
            let siblings = billable_descendants(runtime, token, parent_id).await?;
            for entity in siblings {
                if entity.id == code_id {
                    continue;
                }
                upsert_candidate(
                    &mut candidates,
                    entity,
                    Match {
                        arm: origin.arm,
                        via: "family",
                        term_path: None,
                        rank: origin.rank,
                        score: origin.score,
                    },
                );
            }
        }
    }

    // ── Render candidates, ordered by how many of the mention's digit words
    //    (see `short_digit_words`) they do not carry, then by best (lowest) arm
    //    rank, ties broken by (descending) arm count, then by code
    //    (ascending). A mention with no digit word, or whose digits no
    //    candidate carries, keeps the order it always had. ───────────────────
    //    `candidates` is a HashMap, whose iteration order changes from one
    //    map instance to the next, so without the code key every tie (e.g.
    //    all the family siblings one pointer expands to, which share its
    //    rank) came out in a different order on every call, and a caller
    //    capping the list kept a different subset each time.
    let digits = short_digit_words(text);
    let mut rendered: Vec<(usize, u32, usize, String, Value)> =
        Vec::with_capacity(candidates.len());
    for (_, acc) in candidates {
        let missing = missing_digit_count(&acc, &digits);
        let best_rank = acc.matched.iter().map(|m| m.rank).min().unwrap_or(u32::MAX);
        let arm_count = acc
            .matched
            .iter()
            .map(|m| m.arm)
            .collect::<HashSet<_>>()
            .len();
        rendered.push((
            missing,
            best_rank,
            arm_count,
            acc.entity.name.clone(),
            render_candidate(runtime, token, &acc, light).await?,
        ));
    }
    rendered.sort_by(|a, b| {
        a.0.cmp(&b.0)
            .then(a.1.cmp(&b.1))
            .then(b.2.cmp(&a.2))
            .then_with(|| a.3.cmp(&b.3))
    });
    let candidate_count = rendered.len();
    let candidates_json: Vec<Value> = rendered.into_iter().map(|(_, _, _, _, v)| v).collect();

    let mut chapters: Vec<String> = chapters_reached.into_iter().collect();
    chapters.sort();

    Ok(json!({
        "text": text,
        "candidates": candidates_json,
        "candidate_count": candidate_count,
        "chapters_reached": chapters,
        "arm_hit_counts": {
            "index": index_hit_count,
            "tabular": tabular_hit_count,
            "vector": vector_hit_count,
        },
        "text_mode": {
            "index": text_mode.as_str(),
            "tabular": text_mode.as_str(),
        },
    }))
}

/// The mention's text search, ranked and not yet restricted to a type.
///
/// `AllTerms` is `hybrid_search`'s ranking from the text leg alone
/// ([`crate::search::text_ranked`]). `AnyTerm` is the OR-join search in the
/// text index's own order and scores, over-fetched by
/// `ANY_TERM_CANDIDATE_MULTIPLIER`.
async fn ranked_hits(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    text: &str,
    per_arm_limit: u32,
    text_mode: TextMode,
) -> Result<Vec<SearchHit>, RuntimeError> {
    match text_mode {
        TextMode::AllTerms => crate::search::text_ranked(runtime, token, text, per_arm_limit).await,
        TextMode::AnyTerm => {
            let top_k = per_arm_limit
                .saturating_mul(ANY_TERM_CANDIDATE_MULTIPLIER)
                .max(per_arm_limit);
            let hits = crate::search::text_leg(runtime, token, text, TextQueryMode::AnyTerm, top_k)
                .await?;
            Ok(hits
                .into_iter()
                .map(|h| SearchHit {
                    entity_id: h.subject_id,
                    score: h.score,
                    source: SearchSource::Text,
                    title: h.title,
                    snippet: h.snippet,
                })
                .collect())
        }
    }
}

/// The `k` nearest entities of one ICD-10-CM family to the mention's vector.
/// The vector arm was asked for, so a family with no index is an error that
/// names what to run, never an empty arm.
async fn dense_hits(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    family: &ann::Family,
    q: &[f32],
    k: u32,
    mode: Mode,
) -> Result<Vec<VectorSearchHit>, RuntimeError> {
    let index = ann::loaded(runtime, token, family).await?.ok_or_else(|| {
        RuntimeError::InvalidInput(format!(
            "icd10.resolve: the vector arm needs the {} index: run icd10.embed for that entity \
             type, then icd10.index(action=\"build\", family={:?})",
            family.name, family.name
        ))
    })?;
    ann::search_vector(family, index, q.to_vec(), k as usize, mode).await
}

/// The first `per_arm_limit` of `ranked` that are live `entity_type`
/// entities. The text index's `record_kind` is the entity kind (`concept`
/// for every type of this pack), so the type is checked on the entity.
async fn typed_hits(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    ranked: &[SearchHit],
    entity_type: &str,
    per_arm_limit: u32,
) -> Result<Vec<SearchHit>, RuntimeError> {
    Ok(
        crate::search::keep_typed(runtime, token, ranked, None, entity_type, per_arm_limit)
            .await?
            .into_iter()
            .map(|(hit, _)| hit)
            .collect(),
    )
}

#[allow(clippy::too_many_arguments)]
async fn add_resolved_targets(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    targets: &[Uuid],
    arm: &'static str,
    via_direct: &'static str,
    via_expanded: &'static str,
    term_path: Option<String>,
    rank: u32,
    score: f64,
    expand_non_billable: bool,
    candidates: &mut HashMap<Uuid, CandidateAcc>,
) -> Result<(), RuntimeError> {
    for &target_id in targets {
        let Some(entity) = fetch_one(runtime, token, target_id).await? else {
            continue;
        };
        if prop_bool(&entity, "billable") {
            upsert_candidate(
                candidates,
                entity,
                Match {
                    arm,
                    via: via_direct,
                    term_path: term_path.clone(),
                    rank,
                    score,
                },
            );
        } else if expand_non_billable {
            // A `-`-pointer or a non-leaf category: expand to billable
            // descendants: a pointer ending in `-`, or pointing at a category,
            // expands to that record's billable descendants.
            let descendants = billable_descendants(runtime, token, target_id).await?;
            for child in descendants {
                upsert_candidate(
                    candidates,
                    child,
                    Match {
                        arm,
                        via: via_expanded,
                        term_path: term_path.clone(),
                        rank,
                        score,
                    },
                );
            }
        }
    }
    Ok(())
}

fn upsert_candidate(candidates: &mut HashMap<Uuid, CandidateAcc>, entity: Entity, m: Match) {
    candidates
        .entry(entity.id)
        .or_insert_with(|| CandidateAcc {
            entity,
            matched: Vec::new(),
        })
        .matched
        .push(m);
}

/// `light` leaves `excludes1`, `code_first` and `use_additional` empty without
/// reading them (see [`resolve_for_reachability`]).
async fn render_candidate(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    acc: &CandidateAcc,
    light: bool,
) -> Result<Value, RuntimeError> {
    let e = &acc.entity;
    let code = e.name.clone();
    let props = e.properties.clone().unwrap_or(Value::Null);
    let desc = props
        .get("desc")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let billable = prop_bool(e, "billable");
    let specificity = props.get("specificity").cloned().unwrap_or(Value::Null);
    let laterality = props.get("laterality").cloned().unwrap_or(Value::Null);
    let requires_7th = prop_bool(e, "requires_7th");

    let (excludes1, code_first, use_additional) = if light {
        (Vec::new(), Vec::new(), Vec::new())
    } else {
        (
            excludes1_codes(runtime, token, e.id).await?,
            partner_codes(runtime, token, e.id, EdgeRelation::Precedes, "code_first").await?,
            partner_codes(
                runtime,
                token,
                e.id,
                EdgeRelation::Precedes,
                "use_additional",
            )
            .await?,
        )
    };

    let matched: Vec<Value> = acc
        .matched
        .iter()
        .map(|m| {
            json!({
                "arm": m.arm,
                "via": m.via,
                "term_path": m.term_path,
                "rank": m.rank,
                "score": m.score,
            })
        })
        .collect();

    Ok(json!({
        "code": code,
        "desc": desc,
        "billable": billable,
        "specificity": specificity,
        "laterality": laterality,
        "requires_7th": requires_7th,
        "excludes1": excludes1,
        "code_first": code_first,
        "use_additional": use_additional,
        "matched": matched,
    }))
}

async fn fetch_one(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    id: Uuid,
) -> Result<Option<Entity>, RuntimeError> {
    crate::memo::entity_by_id(runtime, token, id).await
}

/// Most subterms `follow_see_into_subterms` reads below one `see` target.
/// The largest subtree a `see` reaches in the FY2026 Index is a few hundred
/// terms; the bound keeps a pathological target from turning one mention into
/// an unbounded walk.
const SEE_SUBTERM_LIMIT: usize = 600;

/// Words too common in Index subterms to carry a mention to one of them.
const SEE_STOP_WORDS: &[&str] = &[
    "and",
    "the",
    "with",
    "without",
    "for",
    "from",
    "due",
    "not",
    "nos",
    "nec",
    "other",
    "unspecified",
];

fn words_of(text: &str) -> Vec<String> {
    text.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(String::from)
        .collect()
}

/// The mention's digit words of one or two characters ("2" in "type 2 diabetes", "3" in "stage 3").
/// The text index tokenizes by trigram, so a word under three characters produces no token and drops
/// out of the match (`khive-db` `FTS5_TRIGRAM_MIN_SAFE_LEN`): the text arms cannot tell type 1 from
/// type 2, or stage 3 from stage 4.
fn short_digit_words(text: &str) -> Vec<String> {
    let mut digits: Vec<String> = words_of(text)
        .into_iter()
        .filter(|w| w.len() <= 2 && w.chars().all(|c| c.is_ascii_digit()))
        .collect();
    digits.sort();
    digits.dedup();
    digits
}

/// How many of the mention's `digits` the candidate does not carry as a whole word, in its description
/// or in the Index path that matched it.
fn missing_digit_count(acc: &CandidateAcc, digits: &[String]) -> usize {
    if digits.is_empty() {
        return 0;
    }
    let desc = acc
        .entity
        .properties
        .as_ref()
        .and_then(|p| p.get("desc"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    let mut carried: HashSet<String> = words_of(desc).into_iter().collect();
    for m in &acc.matched {
        if let Some(path) = &m.term_path {
            carried.extend(words_of(path));
        }
    }
    digits.iter().filter(|d| !carried.contains(*d)).count()
}

/// A `links_to` edge the ingest wrote for a `see` reference (as opposed to
/// `see also`).
fn is_see_link(edge: &khive_storage::types::Edge) -> bool {
    edge.metadata
        .as_ref()
        .and_then(|m| m.get("icd"))
        .and_then(Value::as_array)
        .is_some_and(|roles| roles.iter().any(|r| r == "see"))
}

/// A `see` reference sends the reader to another term and then down that
/// term's subterms with the rest of the wording: "Check-up, see Examination",
/// then "Examination, child (over 28 days old)" for a well child check. The
/// words that found the referring term are spent. Every subterm below the
/// target whose path, relative to the target, carries one of the remaining
/// mention words is reached like a direct Index hit on that subterm: its own
/// pointer is a `see` match with the subterm's path as `term_path`, and a
/// non-billable pointer widens to its billable codes as `family`.
#[allow(clippy::too_many_arguments)]
async fn follow_see_into_subterms(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    mention: &str,
    referring_path: Option<&str>,
    target_id: Uuid,
    rank: u32,
    score: f64,
    expand_family: bool,
    candidates: &mut HashMap<Uuid, CandidateAcc>,
) -> Result<(), RuntimeError> {
    let spent: HashSet<String> = referring_path
        .map(words_of)
        .unwrap_or_default()
        .into_iter()
        .collect();
    let remaining: HashSet<String> = words_of(mention)
        .into_iter()
        .filter(|w| {
            w.chars().count() >= 3 && !spent.contains(w) && !SEE_STOP_WORDS.contains(&w.as_str())
        })
        .collect();
    if remaining.is_empty() {
        return Ok(());
    }
    let Some(target) = fetch_one(runtime, token, target_id).await? else {
        return Ok(());
    };
    let prefix = format!("{} > ", target.name);
    let mut frontier = vec![target_id];
    let mut read = 0usize;
    while let Some(parent) = frontier.pop() {
        let children = crate::memo::list_edges(
            runtime,
            token,
            EdgeListFilter {
                source_id: Some(parent),
                relations: vec![EdgeRelation::Contains],
                ..Default::default()
            },
            1000,
            0,
        )
        .await?;
        if children.is_empty() {
            continue;
        }
        let ids: Vec<Uuid> = children.iter().map(|e| e.target_id).collect();
        let mut subterms: Vec<Entity> = crate::memo::entities_by_ids(runtime, token, &ids)
            .await?
            .into_iter()
            .filter(|e| e.entity_type.as_deref() == Some("icd_term"))
            .collect();
        subterms.sort_by(|a, b| a.name.cmp(&b.name));
        for sub in subterms {
            read += 1;
            if read > SEE_SUBTERM_LIMIT {
                return Ok(());
            }
            let Some(relative) = sub.name.strip_prefix(&prefix) else {
                continue;
            };
            if words_of(relative).iter().any(|w| remaining.contains(w)) {
                let targets = variant_of_targets(runtime, token, sub.id).await?;
                add_resolved_targets(
                    runtime,
                    token,
                    &targets,
                    "index",
                    "see",
                    "family",
                    Some(sub.name.clone()),
                    rank,
                    score,
                    expand_family,
                    candidates,
                )
                .await?;
            }
            frontier.push(sub.id);
        }
    }
    Ok(())
}

/// Every entity a term's `variant_of` edge points at (0 or 1 in practice; the
/// ingest resolves each Index `code` pointer to a single target, but this
/// walks the edge rather than trusting a term's raw `code` property so a
/// pointer the ingest could not resolve never surfaces here).
async fn variant_of_targets(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    term_id: Uuid,
) -> Result<Vec<Uuid>, RuntimeError> {
    let edges = crate::memo::list_edges(
        runtime,
        token,
        EdgeListFilter {
            source_id: Some(term_id),
            relations: vec![EdgeRelation::VariantOf],
            ..Default::default()
        },
        10,
        0,
    )
    .await?;
    Ok(edges.into_iter().map(|e| e.target_id).collect())
}

/// The nearest `contains` parent (one hop up), or `None` at the root.
pub(crate) async fn immediate_parent(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    id: Uuid,
) -> Result<Option<Uuid>, RuntimeError> {
    let incoming = crate::memo::list_edges(
        runtime,
        token,
        EdgeListFilter {
            target_id: Some(id),
            relations: vec![EdgeRelation::Contains],
            ..Default::default()
        },
        5,
        0,
    )
    .await?;
    Ok(incoming.first().map(|e| e.source_id))
}

/// Every descendant of `root_id` (any depth, reached via `contains`) whose
/// own `billable` property is true. Shared by the index arm's category/`-`
/// pointer expansion and the top-level family-expansion step. Returns the
/// entities themselves (the one batched read below already loads them, so a
/// caller never re-fetches a descendant by id), in id order.
pub(crate) async fn billable_descendants(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    root_id: Uuid,
) -> Result<Vec<Entity>, RuntimeError> {
    // Once per root per request: family expansion asks for the same parent's
    // descendants once per sibling that was hit.
    crate::memo::entity_list(
        format!("billable_descendants:{root_id}"),
        billable_descendants_uncached(runtime, token, root_id),
    )
    .await
}

async fn billable_descendants_uncached(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    root_id: Uuid,
) -> Result<Vec<Entity>, RuntimeError> {
    // The runtime's level-order walk: one batched neighbor read per level
    // instead of one edge list per node.
    let nodes = runtime
        .bfs_traverse(
            token,
            root_id,
            TraversalOptions {
                max_depth: 12,
                direction: Direction::Out,
                relations: Some(vec![EdgeRelation::Contains]),
                min_weight: None,
                limit: None,
            },
        )
        .await?;
    let mut ids: Vec<Uuid> = nodes
        .iter()
        .filter(|n| n.depth > 0)
        .map(|n| n.entity_id)
        .collect();
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    ids.sort();
    let entities = crate::memo::entities_by_ids(runtime, token, &ids).await?;
    let mut out: Vec<Entity> = entities
        .into_iter()
        .filter(|e| prop_bool(e, "billable"))
        .collect();
    out.sort_by_key(|e| e.id);
    Ok(out)
}
