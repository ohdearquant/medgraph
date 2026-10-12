//! Nearest-neighbour search over entity vectors, one index per entity family.
//!
//! The store's own vector search (`KhiveRuntime::vector_search`, sqlite-vec)
//! is an exact scan of every entity vector in the namespace, and it cannot
//! restrict by entity type: a query for ICD-10-CM codes would compete with
//! every SNOMED CT concept, and the other way round. khive has an ANN engine
//! (`khive-vamana`, the one its memory and knowledge packs use for notes and
//! atoms) and none over entity vectors at this revision. This module is that
//! piece for the packs: a family is a set of entity types, its index is a
//! Vamana graph over the family's vectors as the store holds them, saved
//! under the database's own ANN root and memory-mapped on load.
//!
//! The store stays the system of record. An index is a derived artifact:
//! built by an explicit call (`icd10.index`, `clinical.index`) from the
//! vectors in the store, rebuilt the same way after an ingest, and never the
//! only path: [`Mode::Exact`] scans the same vectors in memory and is the
//! reference the ANN is measured against ([`parity`]), and a family with no
//! index leaves the caller on the store's search.
//!
//! What it takes from khive: `khive_vamana::VamanaIndex` (build, search,
//! atomic save, load), the external-id sidecar bound to the commit digest,
//! `KhiveRuntime::backend_ann_root` for the directory, and the runtime's
//! embedder for the query. What it does not use: the ANN write log and its
//! consumer registry. Those serve indexes that replay writes; this corpus
//! changes only by ingest, the index is rebuilt after one, and `status`
//! reports the drift between the index and the store.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock, RwLock};
use std::time::Instant;

use khive_runtime::{KhiveRuntime, NamespaceToken, RuntimeError};
use khive_score::DeterministicScore;
use khive_storage::types::{SqlStatement, SqlValue, VectorSearchHit};
use khive_storage::EntityFilter;
use khive_vamana::{
    read_commit_fingerprint, read_external_ids_sidecar, segment_commit_digest,
    write_external_ids_sidecar, VamanaConfig, VamanaIndex,
};
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

/// A set of entity types searched together.
#[derive(Debug, Clone, Copy)]
pub struct Family {
    pub name: &'static str,
    /// The entity kind every type of the family is stored under.
    pub kind: &'static str,
    pub entity_types: &'static [&'static str],
}

/// ICD-10-CM codes (the Tabular List's code entities).
pub const ICD_CODE: Family = Family {
    name: "icd_code",
    kind: "concept",
    entity_types: &["icd_code"],
};
/// ICD-10-CM three-character categories.
pub const ICD_CATEGORY: Family = Family {
    name: "icd_category",
    kind: "concept",
    entity_types: &["icd_category"],
};
/// Alphabetic Index terms.
pub const ICD_TERM: Family = Family {
    name: "icd_term",
    kind: "concept",
    entity_types: &["icd_term"],
};
/// The families this pack owns.
pub const ICD_FAMILIES: [Family; 3] = [ICD_CODE, ICD_CATEGORY, ICD_TERM];

/// How a family is searched.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    /// The Vamana graph.
    Ann,
    /// Every vector of the family, in memory.
    Exact,
}

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Ann => "ann",
            Mode::Exact => "exact",
        }
    }
}

/// Entities read per keyset page while listing a family.
const ENTITY_PAGE: u32 = 5000;
/// Vector rows read per page of the vector table (about 60 MB at 768
/// dimensions).
const VECTOR_PAGE: i64 = 20_000;

/// One family's index and the entity id of each of its rows.
pub struct FamilyIndex {
    index: VamanaIndex,
    ids: Vec<Uuid>,
}

impl FamilyIndex {
    pub fn len(&self) -> usize {
        self.ids.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ids.is_empty()
    }

    pub fn dimensions(&self) -> usize {
        self.index.dimensions()
    }

    /// The `k` nearest rows to the unit vector `q`, best first, through the graph.
    pub fn ann(&self, q: &[f32], k: usize) -> Result<Vec<VectorSearchHit>, String> {
        let k = k.min(self.ids.len());
        if k == 0 {
            return Ok(Vec::new());
        }
        let raw = self
            .index
            .search(q, k)
            .map_err(|e| format!("vamana search: {e}"))?;
        let mut scored: Vec<(f32, Uuid)> = Vec::with_capacity(raw.len());
        for (ordinal, dist) in raw {
            let id = self
                .ids
                .get(ordinal as usize)
                .ok_or_else(|| format!("vamana returned row {ordinal} outside the id map"))?;
            // Squared L2 between unit vectors to cosine: 1 - d / 2.
            scored.push((1.0 - dist / 2.0, *id));
        }
        Ok(ranked(scored))
    }

    /// The `k` nearest rows to the unit vector `q`, best first, by scanning
    /// every vector of the family.
    pub fn exact(&self, q: &[f32], k: usize) -> Result<Vec<VectorSearchHit>, String> {
        let dim = self.index.dimensions();
        if q.len() != dim {
            return Err(format!("query has {} dimensions, index {dim}", q.len()));
        }
        let flat = self
            .index
            .vectors()
            .map_err(|e| format!("vamana vectors: {e}"))?;
        let mut scored: Vec<(f32, Uuid)> = flat
            .chunks_exact(dim)
            .zip(&self.ids)
            .map(|(row, id)| (dot(q, row), *id))
            .collect();
        let k = k.min(scored.len());
        if k == 0 {
            return Ok(Vec::new());
        }
        if k < scored.len() {
            scored.select_nth_unstable_by(k - 1, by_score_then_id);
            scored.truncate(k);
        }
        Ok(ranked(scored))
    }

    fn save(&self, dir: &Path) -> Result<(), String> {
        if self.ids.len() != self.index.num_vectors() {
            return Err(format!(
                "id map has {} rows, index {}",
                self.ids.len(),
                self.index.num_vectors()
            ));
        }
        std::fs::create_dir_all(dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
        self.index
            .save_atomic(dir)
            .map_err(|e| format!("save_atomic: {e}"))?;
        let digest = segment_commit_digest(dir)?
            .ok_or_else(|| "saved, but the commit record is absent".to_string())?;
        write_external_ids_sidecar(dir, &digest, &self.ids).map_err(|e| e.to_string())
    }

    /// Load a saved index. The id sidecar must carry the digest of the commit
    /// record beside it and one id per vector; anything else is an error.
    fn load(dir: &Path) -> Result<Self, String> {
        read_commit_fingerprint(dir)
            .map_err(|e| format!("commit record: {e}"))?
            .ok_or_else(|| "no commit record".to_string())?;
        let index = VamanaIndex::load(dir).map_err(|e| format!("load: {e}"))?;
        let (sidecar_digest, ids) = read_external_ids_sidecar(dir)?;
        let digest =
            segment_commit_digest(dir)?.ok_or_else(|| "commit record vanished".to_string())?;
        if sidecar_digest != digest {
            return Err("id sidecar belongs to another commit".into());
        }
        if ids.len() != index.num_vectors() {
            return Err(format!(
                "id sidecar has {} rows, index {}",
                ids.len(),
                index.num_vectors()
            ));
        }
        Ok(Self { index, ids })
    }
}

fn by_score_then_id(a: &(f32, Uuid), b: &(f32, Uuid)) -> std::cmp::Ordering {
    b.0.total_cmp(&a.0).then_with(|| a.1.cmp(&b.1))
}

/// Best first (score, then id), with 1-based ranks.
fn ranked(mut scored: Vec<(f32, Uuid)>) -> Vec<VectorSearchHit> {
    scored.sort_by(by_score_then_id);
    scored
        .into_iter()
        .enumerate()
        .map(|(i, (score, id))| VectorSearchHit {
            subject_id: id,
            score: DeterministicScore::from_f64(f64::from(score)),
            rank: (i + 1) as u32,
        })
        .collect()
}

/// Dot product in eight running sums, so the compiler can vectorize it.
fn dot(a: &[f32], b: &[f32]) -> f32 {
    let mut acc = [0f32; 8];
    let (a8, a_rest) = a.as_chunks::<8>();
    let (b8, b_rest) = b.as_chunks::<8>();
    for (x, y) in a8.iter().zip(b8) {
        for i in 0..8 {
            acc[i] += x[i] * y[i];
        }
    }
    let mut sum: f32 = acc.iter().sum();
    for (x, y) in a_rest.iter().zip(b_rest) {
        sum += x * y;
    }
    sum
}

fn l2_normalize(v: &mut [f32]) {
    let norm = dot(v, v).sqrt();
    if norm > 0.0 {
        for x in v.iter_mut() {
            *x /= norm;
        }
    }
}

fn hex(s: &str) -> String {
    s.bytes().map(|b| format!("{b:02x}")).collect()
}

/// The store's vector table for a model: `vec_<name>` with every character
/// outside `[A-Za-z0-9]` replaced by `_` (khive-runtime `config.rs`
/// `sanitize_key`, not public).
fn vector_table(model: &str) -> String {
    let key: String = model
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    format!("vec_{key}")
}

/// What a family's directory name encodes. It must not contain `::vamana::`
/// (the knowledge pack adopts every such directory under the ANN root as one
/// of its own) and must not start with `global::memory_vamana::` (the memory
/// pack's).
fn family_key(namespace: &str, family: &Family, model: &str) -> String {
    format!("{namespace}::entity_family::{}::{model}", family.name)
}

/// The family's index directory, or `None` when the store is in memory or
/// has no default embedding model.
fn family_dir(runtime: &KhiveRuntime, token: &NamespaceToken, family: &Family) -> Option<PathBuf> {
    let root = runtime.backend_ann_root()?;
    let model = runtime.default_embedder_name();
    if model.is_empty() {
        return None;
    }
    Some(root.join(hex(&family_key(token.namespace().as_str(), family, model))))
}

fn cache() -> &'static RwLock<HashMap<PathBuf, Arc<FamilyIndex>>> {
    static LOADED: OnceLock<RwLock<HashMap<PathBuf, Arc<FamilyIndex>>>> = OnceLock::new();
    LOADED.get_or_init(|| RwLock::new(HashMap::new()))
}

fn internal(family: &Family, what: impl std::fmt::Display) -> RuntimeError {
    RuntimeError::Internal(format!("entity family index {}: {what}", family.name))
}

/// The family's index: from this process's cache, else loaded from disk.
/// `None` when none has been built (or the store cannot hold one).
pub async fn loaded(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    family: &Family,
) -> Result<Option<Arc<FamilyIndex>>, RuntimeError> {
    let Some(dir) = family_dir(runtime, token, family) else {
        return Ok(None);
    };
    if let Some(hit) = cache().read().expect("family cache").get(&dir).cloned() {
        return Ok(Some(hit));
    }
    if !dir.join("metadata.bin").exists() {
        return Ok(None);
    }
    let from = dir.clone();
    let index = tokio::task::spawn_blocking(move || FamilyIndex::load(&from))
        .await
        .map_err(|e| internal(family, e))?
        .map_err(|e| internal(family, e))?;
    let model = runtime.default_embedder_name();
    if let Some(dim) = runtime.embedder_dimensions(model) {
        if dim != index.dimensions() {
            return Err(internal(
                family,
                format!(
                    "built for {} dimensions, {model} has {dim}; rebuild it",
                    index.dimensions()
                ),
            ));
        }
    }
    let index = Arc::new(index);
    cache()
        .write()
        .expect("family cache")
        .insert(dir, index.clone());
    Ok(Some(index))
}

/// Query embeddings the packs let wait on the runtime's embedder at once.
///
/// The embedder takes jobs through a queue of 32 and refuses the next one
/// ("embedding worker queue is full") instead of waiting. One visit's
/// candidate lookup is a batch of some thirty searches and several visits run
/// at a time, so the searches take a permit here and wait their turn; half
/// the queue stays free for every other caller.
const EMBED_PERMITS: usize = 16;

/// A permit to hand one query to the embedder (held until it answers).
pub async fn embed_permit() -> Result<tokio::sync::SemaphorePermit<'static>, RuntimeError> {
    static GATE: OnceLock<tokio::sync::Semaphore> = OnceLock::new();
    GATE.get_or_init(|| tokio::sync::Semaphore::new(EMBED_PERMITS))
        .acquire()
        .await
        .map_err(|e| RuntimeError::Internal(format!("embedding gate closed: {e}")))
}

/// The text embedded as a query with the store's default model, unit length.
pub async fn embed_query(runtime: &KhiveRuntime, text: &str) -> Result<Vec<f32>, RuntimeError> {
    let mut q = {
        let _permit = embed_permit().await?;
        runtime.embed_query(text).await?
    };
    l2_normalize(&mut q);
    Ok(q)
}

/// The `k` nearest entities of the family to the unit vector `q`.
pub async fn search_vector(
    family: &Family,
    index: Arc<FamilyIndex>,
    q: Vec<f32>,
    k: usize,
    mode: Mode,
) -> Result<Vec<VectorSearchHit>, RuntimeError> {
    tokio::task::spawn_blocking(move || match mode {
        Mode::Ann => index.ann(&q, k),
        Mode::Exact => index.exact(&q, k),
    })
    .await
    .map_err(|e| internal(family, e))?
    .map_err(|e| internal(family, e))
}

/// The `k` nearest entities of the family to `text`, or `None` when the
/// family has no index.
pub async fn search(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    family: &Family,
    text: &str,
    k: u32,
    mode: Mode,
) -> Result<Option<Vec<VectorSearchHit>>, RuntimeError> {
    let Some(index) = loaded(runtime, token, family).await? else {
        return Ok(None);
    };
    let q = embed_query(runtime, text).await?;
    Ok(Some(
        search_vector(family, index, q, k as usize, mode).await?,
    ))
}

/// The ids of every live entity of the family, in insertion order.
///
/// A keyset walk (`list_entities_after_filtered`): each page continues from
/// the last entity's insertion sequence, so the whole list costs one pass.
/// The offset form re-reads every matching row for each page. The filter
/// names the entity types only, which lets the store drive the walk from the
/// sequence; the kind is checked on each entity.
pub(crate) async fn family_entities(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    family: &Family,
) -> Result<Vec<Uuid>, RuntimeError> {
    let filter = EntityFilter {
        entity_types: family.entity_types.iter().map(|t| t.to_string()).collect(),
        ..Default::default()
    };
    let mut ids = Vec::new();
    let mut after = None;
    loop {
        let (page, next) = runtime
            .list_entities_after_filtered(token, filter.clone(), after, ENTITY_PAGE)
            .await?;
        ids.extend(page.iter().filter(|e| e.kind == family.kind).map(|e| e.id));
        match next {
            Some(cursor) => after = Some(cursor),
            None => break,
        }
    }
    Ok(ids)
}

/// One page of the store's vector table for this namespace and model, in the
/// table's own order: `(subject id, embedding bytes)` when `with_embedding`,
/// else the id alone.
///
/// The table is read in pages of whole rows, never by `subject_id IN (...)`:
/// sqlite-vec answers that predicate with a scan of the whole table per
/// statement, so reading a family 400 ids at a time costs hundreds of scans.
pub(crate) async fn vector_rows(
    runtime: &KhiveRuntime,
    namespace: &str,
    model: &str,
    with_embedding: bool,
    offset: i64,
) -> Result<Vec<(Uuid, Vec<u8>)>, RuntimeError> {
    let table = vector_table(model);
    let columns = if with_embedding {
        "subject_id, embedding"
    } else {
        "subject_id"
    };
    let sql = runtime.sql();
    let mut reader = sql.reader().await.map_err(RuntimeError::Storage)?;
    let rows = reader
        .query_all(SqlStatement {
            sql: format!(
                "SELECT {columns} FROM {table} \
                 WHERE namespace = ?1 AND embedding_model = ?2 LIMIT ?3 OFFSET ?4"
            ),
            params: vec![
                SqlValue::Text(namespace.to_string()),
                SqlValue::Text(model.to_string()),
                SqlValue::Integer(VECTOR_PAGE),
                SqlValue::Integer(offset),
            ],
            label: Some("entity_family_vectors".into()),
        })
        .await
        .map_err(RuntimeError::Storage)?;
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let Some(SqlValue::Text(id)) = row.get("subject_id") else {
            continue;
        };
        let Ok(id) = Uuid::parse_str(id) else {
            continue;
        };
        let bytes = match row.get("embedding") {
            Some(SqlValue::Blob(bytes)) => bytes.clone(),
            _ => Vec::new(),
        };
        out.push((id, bytes));
    }
    Ok(out)
}

/// The ids in the store's vector table for this namespace and model.
pub(crate) async fn embedded_ids(
    runtime: &KhiveRuntime,
    namespace: &str,
    model: &str,
) -> Result<std::collections::HashSet<Uuid>, RuntimeError> {
    let mut ids = std::collections::HashSet::new();
    let mut offset = 0i64;
    loop {
        let rows = vector_rows(runtime, namespace, model, false, offset).await?;
        let n = rows.len() as i64;
        ids.extend(rows.into_iter().map(|(id, _)| id));
        if n < VECTOR_PAGE {
            break;
        }
        offset += n;
    }
    Ok(ids)
}

/// Every live entity of the family that has a vector: the vectors (unit
/// length, row-major), the entity id of each row, and how many entities the
/// family has in all. Rows are in the entity list's order, so a rebuild of an
/// unchanged store lays the index out the same way.
async fn scan(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    family: &Family,
    model: &str,
    dim: usize,
) -> Result<(Vec<f32>, Vec<Uuid>, usize), RuntimeError> {
    let namespace = token.namespace().as_str().to_string();
    let entity_ids = family_entities(runtime, token, family).await?;
    let entities = entity_ids.len();
    let row_of: HashMap<Uuid, usize> = entity_ids
        .iter()
        .enumerate()
        .map(|(row, id)| (*id, row))
        .collect();
    let mut flat = vec![0f32; entities * dim];
    let mut present = vec![false; entities];
    let mut offset = 0i64;
    loop {
        let rows = vector_rows(runtime, &namespace, model, true, offset).await?;
        let n = rows.len() as i64;
        for (id, bytes) in rows {
            let Some(&row) = row_of.get(&id) else {
                continue;
            };
            if bytes.len() != dim * 4 {
                return Err(internal(
                    family,
                    format!(
                        "vector of {id} is {} bytes, expected {}",
                        bytes.len(),
                        dim * 4
                    ),
                ));
            }
            let v = &mut flat[row * dim..(row + 1) * dim];
            for (x, c) in v.iter_mut().zip(bytes.as_chunks::<4>().0) {
                *x = f32::from_le_bytes(*c);
            }
            l2_normalize(v);
            present[row] = true;
        }
        if n < VECTOR_PAGE {
            break;
        }
        offset += n;
    }
    if present.iter().all(|p| *p) {
        return Ok((flat, entity_ids, entities));
    }
    // Entities with no vector leave the layout; the rest keep their order.
    let mut kept = Vec::with_capacity(entities);
    let mut write = 0usize;
    for (row, id) in entity_ids.into_iter().enumerate() {
        if !present[row] {
            continue;
        }
        if write != row {
            flat.copy_within(row * dim..(row + 1) * dim, write * dim);
        }
        kept.push(id);
        write += 1;
    }
    flat.truncate(write * dim);
    Ok((flat, kept, entities))
}

/// Build the family's index from the store's vectors, save it, and serve it
/// from the saved (memory-mapped) copy.
pub async fn build(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    family: &Family,
) -> Result<Value, RuntimeError> {
    let dir = family_dir(runtime, token, family).ok_or_else(|| {
        RuntimeError::InvalidInput(
            "an entity family index needs a database file and a default embedding model".into(),
        )
    })?;
    if runtime.is_read_only() {
        return Err(RuntimeError::InvalidInput(
            "the store is read-only; an index cannot be saved beside it".into(),
        ));
    }
    let model = runtime.default_embedder_name().to_string();
    let dim = runtime
        .embedder_dimensions(&model)
        .ok_or_else(|| internal(family, format!("no dimensions known for {model}")))?;

    let started = Instant::now();
    let (flat, ids, entities) = scan(runtime, token, family, &model, dim).await?;
    let scan_ms = started.elapsed().as_millis();
    if ids.is_empty() {
        return Err(RuntimeError::InvalidInput(format!(
            "family {} has {entities} entities and no vectors; embed them first",
            family.name
        )));
    }
    let vectors = ids.len();

    let started = Instant::now();
    let to = dir.clone();
    let (build_ms, save_ms, index) = tokio::task::spawn_blocking(move || {
        let built = VamanaIndex::build(&flat, VamanaConfig::with_dimensions(dim))
            .map_err(|e| format!("build: {e}"))?;
        drop(flat);
        let build_ms = started.elapsed().as_millis();
        let saving = Instant::now();
        FamilyIndex { index: built, ids }.save(&to)?;
        // Serve the saved copy: its vectors and codes are file-backed pages,
        // the one just built holds them on the heap.
        let index = FamilyIndex::load(&to)?;
        Ok::<_, String>((build_ms, saving.elapsed().as_millis(), index))
    })
    .await
    .map_err(|e| internal(family, e))?
    .map_err(|e| internal(family, e))?;

    cache()
        .write()
        .expect("family cache")
        .insert(dir.clone(), Arc::new(index));
    Ok(json!({
        "family": family.name,
        "entity_types": family.entity_types,
        "model": model,
        "dimensions": dim,
        "entities": entities,
        "vectors": vectors,
        "without_vector": entities - vectors,
        "scan_ms": scan_ms,
        "build_ms": build_ms,
        "save_ms": save_ms,
        "dir": dir.display().to_string(),
    }))
}

async fn entity_count(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    family: &Family,
) -> Result<u64, RuntimeError> {
    let namespaces = token
        .visible_namespaces()
        .iter()
        .map(|n| n.as_str().to_owned())
        .collect::<Vec<_>>();
    let mut total = 0u64;
    // One count per type: the store's index is (namespace, kind, entity_type).
    for entity_type in family.entity_types {
        total += runtime
            .entities(token)?
            .count_entities(
                token.namespace().as_str(),
                EntityFilter {
                    kinds: vec![family.kind.to_string()],
                    entity_types: vec![entity_type.to_string()],
                    namespaces: namespaces.clone(),
                    ..Default::default()
                },
            )
            .await?;
    }
    Ok(total)
}

/// Whether the family has an index, how many vectors it holds, and how many
/// entities the family has now.
pub async fn status(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    family: &Family,
) -> Result<Value, RuntimeError> {
    let entities = entity_count(runtime, token, family).await?;
    let dir = family_dir(runtime, token, family);
    let index = loaded(runtime, token, family).await?;
    Ok(json!({
        "family": family.name,
        "entity_types": family.entity_types,
        "entities": entities,
        "built": index.is_some(),
        "vectors": index.as_ref().map(|i| i.len()),
        "dimensions": index.as_ref().map(|i| i.dimensions()),
        // Entities the index does not cover: not embedded yet, or added since the build.
        "entities_outside_index": index.as_ref().map(|i| entities as i64 - i.len() as i64),
        "dir": dir.map(|d| d.display().to_string()),
    }))
}

fn median(mut values: Vec<f64>) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    values.sort_by(f64::total_cmp);
    values[values.len() / 2]
}

/// The ANN against the exact scan of the same vectors, for these queries:
/// the share of the exact top `k` the ANN returned, whether the first hit is
/// the same, and what each costs.
pub async fn parity(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    family: &Family,
    queries: &[String],
    k: usize,
) -> Result<Value, RuntimeError> {
    let index = loaded(runtime, token, family).await?.ok_or_else(|| {
        RuntimeError::InvalidInput(format!(
            "family {} has no index; build it first",
            family.name
        ))
    })?;
    let mut recalls = Vec::with_capacity(queries.len());
    let mut top1_same = 0usize;
    let (mut embed_ms, mut ann_ms, mut exact_ms) = (Vec::new(), Vec::new(), Vec::new());
    let mut worst: Vec<(f64, String)> = Vec::new();
    for text in queries {
        let t = Instant::now();
        let q = embed_query(runtime, text).await?;
        embed_ms.push(t.elapsed().as_secs_f64() * 1e3);
        let t = Instant::now();
        let ann = search_vector(family, index.clone(), q.clone(), k, Mode::Ann).await?;
        ann_ms.push(t.elapsed().as_secs_f64() * 1e3);
        let t = Instant::now();
        let exact = search_vector(family, index.clone(), q, k, Mode::Exact).await?;
        exact_ms.push(t.elapsed().as_secs_f64() * 1e3);
        let truth: std::collections::HashSet<Uuid> = exact.iter().map(|h| h.subject_id).collect();
        let found = ann.iter().filter(|h| truth.contains(&h.subject_id)).count();
        let recall = if truth.is_empty() {
            1.0
        } else {
            found as f64 / truth.len() as f64
        };
        if ann.first().map(|h| h.subject_id) == exact.first().map(|h| h.subject_id) {
            top1_same += 1;
        }
        recalls.push(recall);
        worst.push((recall, text.clone()));
    }
    worst.sort_by(|a, b| a.0.total_cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    worst.truncate(5);
    let n = queries.len().max(1) as f64;
    Ok(json!({
        "family": family.name,
        "vectors": index.len(),
        "k": k,
        "queries": queries.len(),
        "recall_at_k": {
            "mean": recalls.iter().sum::<f64>() / n,
            "min": recalls.iter().copied().fold(f64::INFINITY, f64::min),
        },
        "first_hit_same": top1_same,
        "ms_median": {
            "embed_query": median(embed_ms),
            "ann": median(ann_ms),
            "exact": median(exact_ms),
        },
        "lowest_recall": worst
            .into_iter()
            .map(|(recall, query)| json!({"query": query, "recall": recall}))
            .collect::<Vec<_>>(),
    }))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct IndexParams {
    action: String,
    #[serde(default)]
    family: Option<String>,
    #[serde(default)]
    queries: Vec<String>,
    #[serde(default)]
    k: Option<u32>,
}

/// `<pack>.index(action, family?, queries?, k?)` over the families a pack
/// owns: `status` (every family, or one), `build` (one family), `parity`
/// (one family, the given queries).
pub async fn handle_index(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    verb: &str,
    families: &[Family],
    params: Value,
) -> Result<Value, RuntimeError> {
    let p: IndexParams = serde_json::from_value(params)
        .map_err(|e| RuntimeError::InvalidInput(format!("{verb} params: {e}")))?;
    let pick = |name: &Option<String>| -> Result<Family, RuntimeError> {
        let names: Vec<&str> = families.iter().map(|f| f.name).collect();
        let name = name.as_deref().ok_or_else(|| {
            RuntimeError::InvalidInput(format!("{verb}: family is one of {names:?}"))
        })?;
        families
            .iter()
            .find(|f| f.name == name)
            .copied()
            .ok_or_else(|| {
                RuntimeError::InvalidInput(format!(
                    "{verb}: family {name:?} is not one of {names:?}"
                ))
            })
    };
    match p.action.as_str() {
        "status" => {
            let mut out = Vec::new();
            for family in families {
                if p.family.as_deref().is_some_and(|name| name != family.name) {
                    continue;
                }
                out.push(status(runtime, token, family).await?);
            }
            Ok(json!({"families": out}))
        }
        "build" => build(runtime, token, &pick(&p.family)?).await,
        "parity" => {
            if p.queries.is_empty() {
                return Err(RuntimeError::InvalidInput(format!(
                    "{verb}: parity needs queries"
                )));
            }
            let k = p.k.unwrap_or(10).clamp(1, 2000) as usize;
            parity(runtime, token, &pick(&p.family)?, &p.queries, k).await
        }
        other => Err(RuntimeError::InvalidInput(format!(
            "{verb}: action {other:?} is not one of [\"status\", \"build\", \"parity\"]"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unit(mut v: Vec<f32>) -> Vec<f32> {
        l2_normalize(&mut v);
        v
    }

    /// 600 unit vectors in 16 dimensions, deterministic.
    fn corpus() -> (Vec<f32>, Vec<Uuid>) {
        let dim = 16;
        let mut flat = Vec::new();
        let mut ids = Vec::new();
        let mut state = 0x9e3779b97f4a7c15u64;
        for i in 0..600u128 {
            let mut v = Vec::with_capacity(dim);
            for _ in 0..dim {
                state = state
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                v.push(((state >> 33) as f32 / (1u64 << 31) as f32) - 0.5);
            }
            flat.extend(unit(v));
            ids.push(Uuid::from_u128(i + 1));
        }
        (flat, ids)
    }

    fn family_index() -> FamilyIndex {
        let (flat, ids) = corpus();
        let index = VamanaIndex::build(&flat, VamanaConfig::with_dimensions(16)).unwrap();
        FamilyIndex { index, ids }
    }

    #[test]
    fn dot_matches_the_plain_sum() {
        let a: Vec<f32> = (0..19).map(|i| i as f32 * 0.25 - 2.0).collect();
        let b: Vec<f32> = (0..19).map(|i| 1.5 - i as f32 * 0.125).collect();
        let plain: f32 = a.iter().zip(&b).map(|(x, y)| x * y).sum();
        assert!((dot(&a, &b) - plain).abs() < 1e-4);
    }

    #[test]
    fn exact_returns_the_true_nearest_in_order() {
        let fi = family_index();
        let (flat, ids) = corpus();
        let q = flat[16 * 37..16 * 38].to_vec();
        let hits = fi.exact(&q, 5).unwrap();
        assert_eq!(hits.len(), 5);
        // A corpus vector's nearest neighbour is itself, at cosine 1.
        assert_eq!(hits[0].subject_id, ids[37]);
        assert!((hits[0].score.to_f64() - 1.0).abs() < 1e-4);
        assert!(hits.windows(2).all(|w| w[0].score >= w[1].score));
        assert_eq!(
            hits.iter().map(|h| h.rank).collect::<Vec<_>>(),
            vec![1, 2, 3, 4, 5]
        );
        // The same five a plain scan finds.
        let mut all: Vec<(f32, Uuid)> = flat
            .as_chunks::<16>()
            .0
            .iter()
            .zip(&ids)
            .map(|(row, id)| (row.iter().zip(&q).map(|(x, y)| x * y).sum::<f32>(), *id))
            .collect();
        all.sort_by(by_score_then_id);
        let want: Vec<Uuid> = all.iter().take(5).map(|(_, id)| *id).collect();
        assert_eq!(hits.iter().map(|h| h.subject_id).collect::<Vec<_>>(), want);
    }

    #[test]
    fn ann_agrees_with_exact_on_a_small_corpus() {
        let fi = family_index();
        let (flat, _) = corpus();
        let mut found = 0usize;
        let mut total = 0usize;
        for row in [3usize, 99, 250, 411, 598] {
            let q = flat[16 * row..16 * (row + 1)].to_vec();
            let exact = fi.exact(&q, 10).unwrap();
            let ann = fi.ann(&q, 10).unwrap();
            assert_eq!(ann[0].subject_id, exact[0].subject_id);
            // Both legs put cosine on the same scale.
            assert!((ann[0].score.to_f64() - exact[0].score.to_f64()).abs() < 1e-3);
            let truth: std::collections::HashSet<Uuid> =
                exact.iter().map(|h| h.subject_id).collect();
            found += ann.iter().filter(|h| truth.contains(&h.subject_id)).count();
            total += truth.len();
        }
        assert!(found * 10 >= total * 9, "recall {found}/{total}");
    }

    #[test]
    fn k_larger_than_the_family_returns_the_family() {
        let fi = family_index();
        let (flat, _) = corpus();
        let q = flat[..16].to_vec();
        assert_eq!(fi.exact(&q, 5000).unwrap().len(), 600);
        assert_eq!(fi.ann(&q, 5000).unwrap().len(), 600);
    }

    #[test]
    fn saved_index_loads_with_its_ids() {
        let fi = family_index();
        let dir = std::env::temp_dir().join(format!("medgraph-family-{}", Uuid::new_v4()));
        fi.save(&dir).unwrap();
        let back = FamilyIndex::load(&dir).unwrap();
        assert_eq!(back.ids, fi.ids);
        let (flat, _) = corpus();
        let q = flat[16 * 7..16 * 8].to_vec();
        assert_eq!(
            back.exact(&q, 3)
                .unwrap()
                .iter()
                .map(|h| h.subject_id)
                .collect::<Vec<_>>(),
            fi.exact(&q, 3)
                .unwrap()
                .iter()
                .map(|h| h.subject_id)
                .collect::<Vec<_>>()
        );
        // A sidecar from another commit is refused.
        write_external_ids_sidecar(&dir, &[7u8; 32], &fi.ids).unwrap();
        assert!(FamilyIndex::load(&dir).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn directory_key_stays_out_of_the_other_packs_namespaces() {
        let key = family_key("local", &ICD_CODE, "bge-base-en-v1.5");
        assert!(!key.contains("::vamana::"));
        assert!(!key.starts_with("global::memory_vamana::"));
        assert_eq!(vector_table("bge-base-en-v1.5"), "vec_bge_base_en_v1_5");
        assert_eq!(hex("a:"), "613a");
    }
}
