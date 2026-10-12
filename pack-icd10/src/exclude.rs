//! `icd10.exclude(candidates, sex?, age_years?, chosen?, mention_chapters?)` —
//! logical exclusion over already-resolved candidate codes. No model calls.
//!
//! E2/E3 (sex/age) reuse `check::sex_conflict`/`check::age_conflict` — the
//! same table and thresholds `icd10.check`'s R5/R6 use, never copied. E1
//! reuses `check::unique_unspecified_child`, the same repair helper R2/R7
//! use in `icd10.check`.

use std::collections::HashSet;

use khive_runtime::{KhiveRuntime, NamespaceToken, RuntimeError};
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

use crate::check::{
    age_conflict, ancestors, excludes1_neighbors, prop_bool, sex_conflict, unique_unspecified_child,
};
use crate::neighborhood::find_by_code;
use crate::refs::normalize_code;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ExcludeParams {
    candidates: Vec<String>,
    #[serde(default)]
    sex: Option<String>,
    #[serde(default)]
    age_years: Option<f64>,
    #[serde(default)]
    chosen: Vec<String>,
    #[serde(default)]
    mention_chapters: Vec<String>,
    /// Rule names (e.g. `"E4"`) to skip entirely, as though that check never
    /// ran. Debug/measurement knob only, for one control: whether disabling
    /// E4 (Excludes1-vs-chosen) stops a
    /// given drop, distinguishing "E4 caused this" from "E4 and something
    /// else would both have caused this". Empty (the default) is normal
    /// behavior with every rule active; a disabled rule falls through to the
    /// next rule in order, it is not a guarantee the candidate survives.
    #[serde(default)]
    disable_rules: Vec<String>,
}

pub async fn handle_exclude(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    params: Value,
) -> Result<Value, RuntimeError> {
    let params: ExcludeParams = serde_json::from_value(params)
        .map_err(|e| RuntimeError::InvalidInput(format!("invalid icd10.exclude arguments: {e}")))?;

    let sex_first = params
        .sex
        .as_deref()
        .map(|s| s.trim().to_lowercase())
        .and_then(|s| s.chars().next());

    let mention_chapters: HashSet<String> = params.mention_chapters.iter().cloned().collect();
    let disabled_rules: HashSet<String> = params.disable_rules.iter().cloned().collect();

    // Resolve `chosen` codes to entity ids up front — E4/E5 both need them.
    // `chosen_families[i]` = {chosen id} ∪ its ancestors, the same
    // ancestor-inclusive family `icd10.check`'s R4 uses: an Excludes1 note
    // written at the category level (e.g. K82.4's note names the K81
    // *category*) still conflicts with a more specific chosen descendant
    // (K81.9) of that category.
    let mut chosen_ids: Vec<Uuid> = Vec::new();
    let mut chosen_families: Vec<HashSet<Uuid>> = Vec::new();
    for raw in &params.chosen {
        let code = normalize_code(raw);
        if let Some(entity) = find_by_code(runtime, token, &code).await? {
            let mut family: HashSet<Uuid> = ancestors(runtime, token, entity.id).await?;
            family.insert(entity.id);
            chosen_ids.push(entity.id);
            chosen_families.push(family);
        }
    }
    // Ancestors of every chosen code (self excluded), for E5.
    let mut chosen_ancestors: HashSet<Uuid> = HashSet::new();
    for &id in &chosen_ids {
        chosen_ancestors.extend(ancestors(runtime, token, id).await?);
    }

    let mut kept: Vec<String> = Vec::new();
    let mut dropped: Vec<Value> = Vec::new();
    let mut unknown: Vec<Value> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();

    for raw in &params.candidates {
        let code = normalize_code(raw);
        if !seen.insert(code.clone()) {
            continue;
        }
        let Some(entity) = find_by_code(runtime, token, &code).await? else {
            // Not in the graph at all: nothing to check against, but a
            // caller (esp. an M1 gold-code scorer) must be able to tell
            // "this code has no record" apart from "every rule dropped it"
            // or "it was silently swallowed" — those are three different
            // failure shapes with different fixes. Report it, never drop it
            // off both lists.
            unknown.push(json!({
                "code": code,
                "reason": "no record in graph",
            }));
            continue;
        };

        // E4 and E5 run before E1-E3: both are pure positional facts about
        // `chosen` (a fixed, already-decided set), independent of the
        // candidate's own billability. Running them first matters in
        // practice, not just in principle: this pack's own billable
        // invariant (`xml_tabular.rs`'s `parse_category`/`parse_code`) makes
        // `billable == true` imply "has no `contains` children", so a
        // candidate that is a genuine ancestor of a chosen code is *always*
        // non-billable — if E1 ran first it would shadow E5 every time and
        // E5 would be unreachable in practice. Checking E4/E5 first lets a
        // caller see the more specific, more useful reason (redundant with
        // an already-chosen code) instead of the generic "not billable".

        // E4: Excludes1 against any code in `chosen`, family-inclusive (see
        // the `chosen_families` comment above) — the same closure
        // `icd10.check`'s R4 uses among survivors, applied here between
        // `candidates` and the fixed `chosen` set.
        if !disabled_rules.contains("E4") {
            let mut candidate_family: HashSet<Uuid> = ancestors(runtime, token, entity.id).await?;
            candidate_family.insert(entity.id);
            let mut conflict_with: Option<usize> = None;
            'search: for &x in &candidate_family {
                let neighbors = excludes1_neighbors(runtime, token, x).await?;
                for (i, family_f) in chosen_families.iter().enumerate() {
                    if neighbors.iter().any(|n| family_f.contains(n)) {
                        conflict_with = Some(i);
                        break 'search;
                    }
                }
            }
            if let Some(i) = conflict_with {
                let conflict_code = code_of(runtime, token, chosen_ids[i]).await?;
                dropped.push(dropped_entry(
                    &code,
                    "E4",
                    &format!("excludes1 conflict with chosen code {conflict_code}"),
                ));
                continue;
            }
        }

        // E5: an ancestor of a code in `chosen`.
        if !disabled_rules.contains("E5") && chosen_ancestors.contains(&entity.id) {
            dropped.push(dropped_entry(
                &code,
                "E5",
                "ancestor of a code already in chosen",
            ));
            continue;
        }

        // E1: non-billable. Its billable leaves stay only if they are
        // independently present in `candidates` — that check happens
        // naturally: this candidate is simply dropped, and any leaf code
        // that was ALSO passed in `candidates` is evaluated on its own merits
        // below, not synthesized here.
        if !disabled_rules.contains("E1") && !prop_bool(&entity, "billable") {
            let reason = match unique_unspecified_child(runtime, token, entity.id).await? {
                Some((child_code, _)) => {
                    format!("not billable; unique unspecified child is {child_code}")
                }
                None => "not billable, pick a billable child".to_string(),
            };
            dropped.push(dropped_entry(&code, "E1", &reason));
            continue;
        }

        // E2: sex.
        if !disabled_rules.contains("E2") {
            if let Some(reason) = sex_conflict(&code, sex_first) {
                dropped.push(dropped_entry(&code, "E2", reason));
                continue;
            }
        }

        // E3: age.
        if !disabled_rules.contains("E3") {
            if let Some(reason) = age_conflict(&code, params.age_years) {
                dropped.push(dropped_entry(&code, "E3", reason));
                continue;
            }
        }

        // E6: outside every chapter in `mention_chapters` — only applied
        // when that set is non-empty, so it is inert (and separately
        // measurable) otherwise.
        if !disabled_rules.contains("E6") && !mention_chapters.is_empty() {
            let chapter = crate::check::chapter_range_of(runtime, token, entity.id).await?;
            let in_scope = chapter
                .as_deref()
                .is_some_and(|c| mention_chapters.contains(c));
            if !in_scope {
                let reason = match &chapter {
                    Some(c) => format!("chapter {c} not in mention_chapters"),
                    None => "chapter unknown; not in mention_chapters".to_string(),
                };
                dropped.push(dropped_entry(&code, "E6", &reason));
                continue;
            }
        }

        kept.push(code);
    }

    Ok(json!({
        "kept": kept,
        "dropped": dropped,
        "unknown": unknown,
    }))
}

fn dropped_entry(code: &str, rule: &str, reason: &str) -> Value {
    json!({
        "code": code,
        "rule": rule,
        "reason": reason,
    })
}

async fn code_of(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    id: Uuid,
) -> Result<String, RuntimeError> {
    let hit = crate::memo::entity_by_id(runtime, token, id).await?;
    Ok(hit.map(|e| e.name).unwrap_or_else(|| id.to_string()))
}
