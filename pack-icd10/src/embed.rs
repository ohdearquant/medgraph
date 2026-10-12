//! `icd10.embed(entity_type, after?, limit?, batch?)`: write the vectors the
//! ICD-10-CM ingest did not.
//!
//! `icd10.ingest` creates its entities through `create_many`, which indexes
//! their text and embeds nothing, so the codes, categories and Index terms
//! have had no vectors and `icd10.resolve` no dense arm. This verb embeds one
//! slice of one entity type with the store's default model and writes each
//! vector exactly as the runtime writes an entity's: the text is
//! `entity_embedding_text` (name, then description), the field `entity.body`.
//!
//! One call covers `limit` entities of the type in insertion order, starting
//! after the entity `after` (the previous call's `next_after`), and skips
//! every entity that already has a vector, so the caller walks the type in
//! slices (a request's deadline is at most an hour) and a repeated or
//! interrupted slice writes nothing twice.
//!
//! The walk is the store's keyset pager and the vectors already present are
//! read once per call. The offset pager re-reads every matching entity for
//! each page, and the vector store's `batch_exists` scans the whole vector
//! table per 400 ids: together they made a slice slower the further in it
//! started.

use std::time::Instant;

use chrono::Utc;
use khive_runtime::curation::entity_embedding_text;
use khive_runtime::{KhiveRuntime, NamespaceToken, RuntimeError};
use khive_storage::types::VectorRecord;
use khive_storage::EntityFilter;
use khive_types::SubstrateKind;
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

/// The field name the runtime writes an entity's vector under
/// (`reindex_entity`, khive-runtime `curation.rs`).
const ENTITY_FIELD: &str = "entity.body";
/// Entity types this verb embeds: the ones the resolve arms search.
const EMBEDDABLE: [&str; 3] = ["icd_code", "icd_category", "icd_term"];
const PAGE: u32 = 2000;
const DEFAULT_LIMIT: u32 = 20_000;
const MAX_LIMIT: u32 = 50_000;
const DEFAULT_BATCH: u32 = 32;
const MAX_BATCH: u32 = 256;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EmbedParams {
    entity_type: String,
    #[serde(default)]
    after: Option<Uuid>,
    #[serde(default)]
    limit: Option<u32>,
    #[serde(default)]
    batch: Option<u32>,
}

pub async fn handle_embed(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    params: Value,
) -> Result<Value, RuntimeError> {
    let p: EmbedParams = serde_json::from_value(params)
        .map_err(|e| RuntimeError::InvalidInput(format!("invalid icd10.embed arguments: {e}")))?;
    if !EMBEDDABLE.contains(&p.entity_type.as_str()) {
        return Err(RuntimeError::InvalidInput(format!(
            "icd10.embed: entity_type {:?} is not one of {EMBEDDABLE:?}",
            p.entity_type
        )));
    }
    let limit = p.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
    let batch = p.batch.unwrap_or(DEFAULT_BATCH).clamp(1, MAX_BATCH) as usize;
    let model = runtime.default_embedder_name().to_string();
    if model.is_empty() {
        return Err(RuntimeError::Unconfigured("embedding_model".into()));
    }
    let store = runtime.vectors(token)?;
    let namespace = token.namespace().as_str().to_string();
    // The entity type alone: the keyset walk is driven by the insertion
    // sequence when no kind is named. The kind is checked on each entity.
    let filter = EntityFilter {
        entity_types: vec![p.entity_type.clone()],
        ..Default::default()
    };

    let started = Instant::now();
    let have = crate::ann::embedded_ids(runtime, &namespace, &model).await?;
    let exists_ms = started.elapsed().as_millis();
    let (mut scanned, mut already, mut embedded) = (0u32, 0u32, 0u32);
    let mut embed_ms = 0u128;
    let mut write_ms = 0u128;
    let mut after = p.after;
    let mut exhausted = false;
    while scanned < limit {
        let want = PAGE.min(limit - scanned);
        let (page, next) = runtime
            .list_entities_after_filtered(token, filter.clone(), after, want)
            .await?;
        let page: Vec<_> = page.into_iter().filter(|e| e.kind == "concept").collect();
        let n = page.len() as u32;
        scanned += n;
        let todo: Vec<_> = page.into_iter().filter(|e| !have.contains(&e.id)).collect();
        already += n - todo.len() as u32;
        for chunk in todo.chunks(batch) {
            let texts: Vec<String> = chunk.iter().map(entity_embedding_text).collect();
            let t = Instant::now();
            let vectors = runtime.embed_document_batch(&texts).await?;
            embed_ms += t.elapsed().as_millis();
            let now = Utc::now();
            let records: Vec<VectorRecord> = chunk
                .iter()
                .zip(vectors)
                .map(|(entity, vector)| VectorRecord {
                    subject_id: entity.id,
                    kind: SubstrateKind::Entity,
                    namespace: entity.namespace.clone(),
                    field: ENTITY_FIELD.to_string(),
                    embedding_model: Some(model.clone()),
                    vectors: vec![vector],
                    updated_at: now,
                })
                .collect();
            let t = Instant::now();
            let summary = store
                .insert_batch(records)
                .await
                .map_err(RuntimeError::Storage)?;
            write_ms += t.elapsed().as_millis();
            if summary.failed > 0 {
                return Err(RuntimeError::Internal(format!(
                    "icd10.embed: {} of {} vectors were refused: {}",
                    summary.failed, summary.attempted, summary.first_error
                )));
            }
            embedded += summary.affected as u32;
        }
        match next {
            Some(cursor) => after = Some(cursor),
            None => {
                exhausted = true;
                break;
            }
        }
    }

    Ok(json!({
        "entity_type": p.entity_type,
        "model": model,
        "field": ENTITY_FIELD,
        "scanned": scanned,
        "already_embedded": already,
        "embedded": embedded,
        // The entity to continue after; null when the slice reached the end
        // of the type's list.
        "next_after": if exhausted { None } else { after },
        "done": exhausted,
        "ms": {
            "total": started.elapsed().as_millis(),
            "existing_vectors": exists_ms,
            "embed": embed_ms,
            "write": write_ms,
        },
    }))
}
