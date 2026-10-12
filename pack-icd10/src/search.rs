//! Entity search run by the pack: the two legs of
//! `KhiveRuntime::hybrid_search` called directly, fused the same way, and the
//! entity-type restriction applied by primary key.
//!
//! Why the packs do not call `hybrid_search` itself (pinned khive rev,
//! `khive-runtime/src/retrieval.rs` `hybrid_search_inner`):
//!
//! 1. It restricts the fused candidates with a filtered entity list
//!    (`id IN (...) AND entity_type IN (...)`, a `COUNT(*)` of the same
//!    predicate first). On a store without planner statistics that keeps every
//!    entity in one namespace, SQLite answers both from the namespace index,
//!    so each call reads all ~324k entities twice. [`keep_typed`] reads each
//!    candidate through its key instead and stops at `limit`.
//! 2. It embeds the query and searches vectors whenever the store's config
//!    carries a default embedding engine. The ICD-10-CM entities have no
//!    vectors, so for `icd10.resolve` that leg can only return entities the
//!    type filter then drops; [`text_ranked`] is the same ranking without it.
//! 3. Several arms of one request search the same text (`icd10.resolve` has
//!    three, `clinical.umls_retrieve` two). The legs are per text, not per
//!    type, so the caller runs them once and applies each type to the result.
//!
//! The fusion is khive-fusion's reciprocal rank fusion with the runtime's
//! entity-search constants; the pack tests pin the result against
//! `hybrid_search` on the same store.

use std::collections::{HashMap, HashSet};

use khive_fusion::reciprocal_rank_fusion;
use khive_runtime::{KhiveRuntime, NamespaceToken, RuntimeError, SearchHit, SearchSource};
use khive_score::DeterministicScore;
use khive_storage::types::{
    TextFilter, TextQueryMode, TextSearchHit, TextSearchRequest, VectorSearchHit,
};
use khive_storage::Entity;
use khive_types::SubstrateKind;
use uuid::Uuid;

/// `hybrid_search`'s private `RRF_K`.
const RRF_K: usize = 10;
/// `hybrid_search`'s private `EXACT_MATCH_BOOST`.
const EXACT_MATCH_BOOST: f64 = 0.5;
/// `hybrid_search`'s private `CANDIDATE_MULTIPLIER`: each leg fetches this
/// many times the caller's limit.
pub const CANDIDATE_MULTIPLIER: u32 = 4;

/// How many candidates each leg fetches for a caller limit of `limit`.
pub fn candidates_for(limit: u32) -> u32 {
    limit.saturating_mul(CANDIDATE_MULTIPLIER).max(limit)
}

/// The full-text leg over every visible namespace, best match first.
pub async fn text_leg(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    text: &str,
    mode: TextQueryMode,
    top_k: u32,
) -> Result<Vec<TextSearchHit>, RuntimeError> {
    let namespaces: Vec<String> = token
        .visible_namespaces()
        .iter()
        .map(|ns| ns.as_str().to_owned())
        .collect();
    let result = runtime
        .text(token)?
        .search(TextSearchRequest {
            query: text.to_string(),
            mode,
            filter: Some(TextFilter {
                namespaces,
                ..TextFilter::default()
            }),
            top_k,
            // No caller reads the snippet; asking for one makes the index
            // build it for every row.
            snippet_chars: 0,
        })
        .await
        .map_err(RuntimeError::from);
    khive_runtime::error::fts_text_leg_or_err(result, "pack search", text)
}

/// The vector leg: the query embedded with the store's default model and
/// searched against the entity vectors (exact, as the runtime does it).
pub async fn vector_leg(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    text: &str,
    top_k: u32,
) -> Result<Vec<VectorSearchHit>, RuntimeError> {
    // The runtime embeds the text inside this call; see `ann::embed_permit`.
    let _permit = crate::ann::embed_permit().await?;
    runtime
        .vector_search(token, None, Some(text), top_k, Some(SubstrateKind::Entity))
        .await
}

/// The two legs fused as `hybrid_search` ranks entities: khive-fusion's
/// reciprocal rank fusion (`1 / (10 + rank)` per leg), plus 0.5 for a text hit
/// whose title equals the query (case-insensitive), ordered by score, then id.
///
/// The fusion is the library's. The exact-title boost is the one step the
/// runtime's entity search adds on top of it (private there), kept so a
/// mention that names a term exactly still ranks it first.
pub fn fuse(
    text_hits: Vec<TextSearchHit>,
    vector_hits: Vec<VectorSearchHit>,
    query_text: &str,
) -> Vec<SearchHit> {
    let query_lower = query_text.to_lowercase();
    let vector_ids: HashSet<Uuid> = vector_hits.iter().map(|h| h.subject_id).collect();
    let vector_stream: Vec<(Uuid, DeterministicScore)> = vector_hits
        .into_iter()
        .map(|h| (h.subject_id, h.score))
        .collect();
    let text_stream: Vec<(Uuid, DeterministicScore)> =
        text_hits.iter().map(|h| (h.subject_id, h.score)).collect();
    // First occurrence of each text hit carries its title and snippet.
    let mut text_meta: HashMap<Uuid, (Option<String>, Option<String>)> =
        HashMap::with_capacity(text_hits.len());
    for hit in text_hits {
        text_meta
            .entry(hit.subject_id)
            .or_insert((hit.title, hit.snippet));
    }

    let mut hits: Vec<SearchHit> = reciprocal_rank_fusion(vec![vector_stream, text_stream], RRF_K)
        .into_iter()
        .map(|(id, mut score)| {
            let in_vector = vector_ids.contains(&id);
            let (source, title, snippet) = match text_meta.remove(&id) {
                Some((title, snippet)) => {
                    if title
                        .as_deref()
                        .is_some_and(|t| t.to_lowercase() == query_lower)
                    {
                        score = score + DeterministicScore::from_f64(EXACT_MATCH_BOOST);
                    }
                    let source = if in_vector {
                        SearchSource::Both
                    } else {
                        SearchSource::Text
                    };
                    (source, title, snippet)
                }
                None => (SearchSource::Vector, None, None),
            };
            SearchHit {
                entity_id: id,
                score,
                source,
                title,
                snippet,
            }
        })
        .collect();
    hits.sort_by(|a, b| b.score.cmp(&a.score).then(a.entity_id.cmp(&b.entity_id)));
    hits
}

/// `hybrid_search`'s ranking with the text leg alone (`all_terms`: every word
/// of the mention must appear), unfiltered by type.
pub async fn text_ranked(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    text: &str,
    limit: u32,
) -> Result<Vec<SearchHit>, RuntimeError> {
    let hits = text_leg(
        runtime,
        token,
        text,
        TextQueryMode::Plain,
        candidates_for(limit),
    )
    .await?;
    Ok(fuse(hits, Vec::new(), text))
}

/// Which vector leg a hybrid ranking runs.
#[derive(Debug, Clone, Copy)]
pub enum VectorLeg<'a> {
    /// The store's search: an exact scan of every entity vector in the
    /// namespace, whatever its type (what `hybrid_search` runs).
    Store,
    /// One entity family's index ([`crate::ann`]). When the family has none,
    /// `required` decides between an error and the store's search.
    Family {
        family: &'a crate::ann::Family,
        mode: crate::ann::Mode,
        required: bool,
    },
}

/// `hybrid_search`'s ranking with both legs, unfiltered by type.
pub async fn hybrid_ranked(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    text: &str,
    limit: u32,
) -> Result<Vec<SearchHit>, RuntimeError> {
    Ok(
        hybrid_ranked_with(runtime, token, text, limit, VectorLeg::Store)
            .await?
            .0,
    )
}

/// [`hybrid_ranked`] with the vector leg chosen by the caller. Returns the
/// ranking and the leg that ran: `"none"` (the store has no embedding
/// model), `"store"`, `"ann"` or `"exact"`.
pub async fn hybrid_ranked_with(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    text: &str,
    limit: u32,
    leg: VectorLeg<'_>,
) -> Result<(Vec<SearchHit>, &'static str), RuntimeError> {
    let top_k = candidates_for(limit);
    // As the runtime decides it: a store with no default embedding model has
    // no vector leg, and the ranking is the text leg's alone.
    if runtime.config().embedding_model.is_none() {
        let text_hits = text_leg(runtime, token, text, TextQueryMode::Plain, top_k).await?;
        return Ok((fuse(text_hits, Vec::new(), text), "none"));
    }
    let vector = async {
        if let VectorLeg::Family {
            family,
            mode,
            required,
        } = leg
        {
            match crate::ann::search(runtime, token, family, text, top_k, mode).await? {
                Some(hits) => return Ok((hits, mode.as_str())),
                None if required => {
                    return Err(RuntimeError::InvalidInput(format!(
                        "entity family {} has no index; build it first",
                        family.name
                    )))
                }
                None => {}
            }
        }
        Ok((vector_leg(runtime, token, text, top_k).await?, "store"))
    };
    let (text_hits, (vector_hits, ran)) = tokio::try_join!(
        text_leg(runtime, token, text, TextQueryMode::Plain, top_k),
        vector,
    )?;
    Ok((fuse(text_hits, vector_hits, text), ran))
}

/// The first `limit` hits of `ranked` that are live entities of `entity_type`
/// (and of `kind`, when given), each with its entity.
///
/// One key read per candidate, in rank order, stopping at `limit`. A hit the
/// vector leg alone produced has no title; it takes the entity's name and
/// description, as `hybrid_search` fills them.
pub async fn keep_typed(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    ranked: &[SearchHit],
    kind: Option<&str>,
    entity_type: &str,
    limit: u32,
) -> Result<Vec<(SearchHit, Entity)>, RuntimeError> {
    let mut out = Vec::new();
    for hit in ranked {
        if out.len() >= limit as usize {
            break;
        }
        let Some(entity) = crate::memo::entity_by_id(runtime, token, hit.entity_id).await? else {
            continue;
        };
        if entity.entity_type.as_deref() != Some(entity_type) {
            continue;
        }
        if kind.is_some_and(|k| entity.kind != k) {
            continue;
        }
        let mut hit = hit.clone();
        if hit.title.is_none() {
            hit.title = Some(entity.name.clone());
        }
        if hit.snippet.is_none() {
            hit.snippet = entity.description.clone();
        }
        out.push((hit, entity));
    }
    Ok(out)
}
