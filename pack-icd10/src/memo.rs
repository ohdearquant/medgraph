//! Per-request memo of the pack's graph reads.
//!
//! One `icd10.*` read verb (resolve, verify, check, exclude, evidence,
//! neighborhood) issues hundreds of small `list_edges` /
//! `list_entities_filtered` calls whose inputs repeat: the same term's
//! `variant_of` pointers from the name and the quote arm, the same category's
//! billable descendants under family expansion, the same ancestors for
//! sibling codes, the same guideline rule table per hypothesis. Each one is a
//! pooled SQLite reader round-trip (measured 2026-09-30: one `icd10.resolve`
//! of a quote = 20 s idle, `icd10.verify` ~30 s per hypothesis, the daemon's
//! time in B-tree point reads).
//!
//! The memo lives for exactly one dispatch of a read verb (a tokio task-local
//! opened by [`scoped`] in `handlers.rs`) and is keyed on the filter's `Debug`
//! form plus limit/offset and the token's visible namespaces, so a verb reads
//! the store exactly as before, once per distinct query. Write verbs
//! (`ingest`, `ingest_external`) never open a scope: outside one, the helpers
//! are plain pass-throughs, so a read issued during an ingest always sees the
//! rows that ingest just wrote.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::sync::Arc;

use khive_runtime::{EdgeListFilter, KhiveRuntime, NamespaceToken, RuntimeError};
use khive_storage::{Edge, Entity, EntityFilter};
use serde_json::Value;
use uuid::Uuid;

#[derive(Default)]
struct Memo {
    edges: HashMap<String, Arc<Vec<Edge>>>,
    entities: HashMap<String, Arc<Vec<Entity>>>,
    /// One entity per id (`None` = no live row), filled by [`entity_by_id`].
    by_id: HashMap<Uuid, Option<Arc<Entity>>>,
    /// `icd10.resolve` results by `(mode, text)`: `icd10.verify` resolves a
    /// mention's name and quote once per hypothesis, and several hypotheses
    /// share a mention.
    resolved: HashMap<String, Arc<Value>>,
}

tokio::task_local! {
    static MEMO: RefCell<Memo>;
}

/// Run `f` with a fresh memo; every helper below hits the memo inside it.
pub(crate) async fn scoped<F, T>(f: F) -> T
where
    F: Future<Output = T>,
{
    MEMO.scope(RefCell::new(Memo::default()), f).await
}

fn ns_key(token: &NamespaceToken) -> String {
    format!("{:?}", token.visible_namespaces())
}

/// `runtime.list_edges` with the per-request memo.
pub(crate) async fn list_edges(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    filter: EdgeListFilter,
    limit: u32,
    offset: u32,
) -> Result<Vec<Edge>, RuntimeError> {
    let key = format!("{filter:?}|{limit}|{offset}|{}", ns_key(token));
    if let Ok(Some(hit)) = MEMO.try_with(|m| m.borrow().edges.get(&key).cloned()) {
        return Ok((*hit).clone());
    }
    let rows = runtime.list_edges(token, filter, limit, offset).await?;
    let _ = MEMO.try_with(|m| m.borrow_mut().edges.insert(key, Arc::new(rows.clone())));
    Ok(rows)
}

/// `runtime.list_entities_filtered` with the per-request memo.
pub(crate) async fn list_entities_filtered(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    filter: EntityFilter,
    limit: u32,
    offset: u32,
) -> Result<Vec<Entity>, RuntimeError> {
    let key = format!("{filter:?}|{limit}|{offset}|{}", ns_key(token));
    if let Ok(Some(hit)) = MEMO.try_with(|m| m.borrow().entities.get(&key).cloned()) {
        return Ok((*hit).clone());
    }
    let rows = runtime
        .list_entities_filtered(token, filter, limit, offset)
        .await?;
    let _ = MEMO.try_with(|m| m.borrow_mut().entities.insert(key, Arc::new(rows.clone())));
    Ok(rows)
}

/// The live entity `id` names, read through the primary key
/// (`runtime.get_entity`: `WHERE id = ?`), once per request.
///
/// Every by-id read in this pack goes through here or [`entities_by_ids`],
/// never through a filtered list (`EntityFilter { ids, .. }`). The filtered
/// list's SQL is `namespace = ? AND deleted_at IS NULL AND id IN (...)`, and a
/// store without planner statistics (no `sqlite_stat1`: every store this pack
/// ingests) answers it from the namespace index as soon as the list has a
/// handful of ids. This pack keeps the whole terminology in one namespace, so
/// that plan reads all ~324k entities per call, and the list runs a
/// `COUNT(*)` with the same plan first. Measured 2026-09-30 on a scratch
/// table of the same shape: 5 ids 38 ms and 300 ids 68 ms through the filtered
/// list, under 1 ms through the key; a thread sample of the daemon during one
/// `icd10.verify` had 97% of wall time in that one query.
pub(crate) async fn entity_by_id(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    id: Uuid,
) -> Result<Option<Entity>, RuntimeError> {
    if let Ok(Some(hit)) = MEMO.try_with(|m| m.borrow().by_id.get(&id).cloned()) {
        return Ok(hit.map(|e| (*e).clone()));
    }
    let entity = match runtime.get_entity(token, id).await {
        // The key read carries no namespace predicate; the filtered list did.
        Ok(e) => token
            .visible_namespaces()
            .iter()
            .any(|ns| ns.as_str() == e.namespace)
            .then_some(e),
        Err(RuntimeError::NotFound(_)) => None,
        Err(e) => return Err(e),
    };
    let _ = MEMO.try_with(|m| {
        m.borrow_mut()
            .by_id
            .insert(id, entity.clone().map(Arc::new))
    });
    Ok(entity)
}

/// The live entities among `ids` (duplicates read once, missing ids dropped),
/// in the order the store's filtered list returned them: newest first, then
/// id descending.
pub(crate) async fn entities_by_ids(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    ids: &[Uuid],
) -> Result<Vec<Entity>, RuntimeError> {
    let mut seen: HashSet<Uuid> = HashSet::with_capacity(ids.len());
    let mut out = Vec::with_capacity(ids.len());
    for &id in ids {
        if !seen.insert(id) {
            continue;
        }
        if let Some(entity) = entity_by_id(runtime, token, id).await? {
            out.push(entity);
        }
    }
    out.sort_by(|a, b| b.created_at.cmp(&a.created_at).then(b.id.cmp(&a.id)));
    Ok(out)
}

/// Every live entity of one pack type, newest first (the filtered list's own
/// order), paged by `limit` / `offset`.
///
/// The filter names the kind as well as the type. Every entity this pack and
/// the clinical pack ingest is a `concept` (`ingest.rs` `ensure_entities`), and
/// the store's index is `(namespace, kind, entity_type)`: a filter with the
/// type alone can use only its namespace prefix, which here is the whole
/// store, twice per call (the list counts before it selects). Do not add the
/// kind to a `name_exact` lookup: there the `(namespace, name)` index is the
/// right one and the extra equality makes the planner leave it.
pub(crate) async fn entities_of_type(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    entity_type: &str,
    limit: u32,
    offset: u32,
) -> Result<Vec<Entity>, RuntimeError> {
    list_entities_filtered(
        runtime,
        token,
        EntityFilter {
            kinds: vec!["concept".to_string()],
            entity_types: vec![entity_type.to_string()],
            ..Default::default()
        },
        limit,
        offset,
    )
    .await
}

/// A memoized entity list for `key` (the caller names what was computed),
/// computing it with `compute` on a miss.
pub(crate) async fn entity_list<F>(key: String, compute: F) -> Result<Vec<Entity>, RuntimeError>
where
    F: Future<Output = Result<Vec<Entity>, RuntimeError>>,
{
    if let Ok(Some(hit)) = MEMO.try_with(|m| m.borrow().entities.get(&key).cloned()) {
        return Ok((*hit).clone());
    }
    let rows = compute.await?;
    let _ = MEMO.try_with(|m| m.borrow_mut().entities.insert(key, Arc::new(rows.clone())));
    Ok(rows)
}

/// A memoized resolve result for `key` (the caller composes mode + text),
/// computing it with `compute` on a miss.
pub(crate) async fn resolved<F>(key: String, compute: F) -> Result<Value, RuntimeError>
where
    F: Future<Output = Result<Value, RuntimeError>>,
{
    if let Ok(Some(hit)) = MEMO.try_with(|m| m.borrow().resolved.get(&key).cloned()) {
        return Ok((*hit).clone());
    }
    let value = compute.await?;
    let _ = MEMO.try_with(|m| m.borrow_mut().resolved.insert(key, Arc::new(value.clone())));
    Ok(value)
}
