//! `icd10.verify(hypotheses, mentions, sex?, age_years?)` — the graph checks
//! the model's hypotheses: a deterministic composition of `resolve` /
//! `neighborhood` / `check` (and `exclude`'s Excludes1 rule, shared code with
//! `check`'s own R4) that answers each hypothesis with one of CONFIRMED /
//! REVISE / REJECT / NEEDS_COMPANION / UNSUPPORTED, in the precedence order
//! `build_one` applies.
//!
//! Eight facets: `reachable`, `axis`, `status`, `conflicts`, `obligations`,
//! `support`, `guideline`, `coherence`.
//!
//! `guideline` reports combination-rule matches as `proposals` — these never
//! change the verdict, only list a suggested recode; a `reporting_condition`
//! guideline rule (e.g. Z68 needs E66) still REJECTs, same precedence bucket
//! as the R-rule REJECTs. `support` (medication-path evidence, via
//! `evidence::medication_evidence_for_mention`) never alone confirms: a code
//! reached only through `support` stays UNSUPPORTED, tagged
//! `support_only: true`. Ambiguous drug matches are reported separately in
//! `support_abstentions` and never count as support. `coherence` reports whether the hypothesis code
//! itself carries a CCSR category (AHRQ DXCCSR `part_of` edge) — when it
//! does, `code_ccsr`/`mention_ccsr` are populated from it and `consistent`
//! is `true`; no fixture in this crate's fixture pair ingests CCSR, so this
//! path is exercised only against a graph that has run
//! `icd10.ingest_external`'s `ccsr_csv` arm. This is a narrower reading of
//! "coherence" than a full mention-vs-code CCSR mismatch check.

use std::collections::{HashMap, HashSet};

use khive_runtime::{EdgeListFilter, KhiveRuntime, NamespaceToken, RuntimeError};
use khive_types::EdgeRelation;
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

use crate::check;
use crate::evidence;
use crate::neighborhood;
use crate::refs::normalize_code;
use crate::resolve;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct MentionIn {
    // Accepted per the request shape (condition/medication/...). The
    // history/family exemption is driven by `status`
    // (current/past/negated/family/planned/uncertain), never by `type` —
    // status logic never reads this field. The `support` facet does: only
    // `type == "medication"` mentions are walked through
    // `evidence::medication_paths_for_mention`.
    #[serde(default = "default_mention_type")]
    r#type: String,
    name: String,
    quote: String,
    #[serde(default = "default_status")]
    status: String,
}

fn default_mention_type() -> String {
    "condition".to_string()
}

fn default_status() -> String {
    "current".to_string()
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct HypothesisIn {
    code: String,
    mention: MentionIn,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct VerifyParams {
    hypotheses: Vec<HypothesisIn>,
    #[serde(default)]
    mentions: Vec<MentionIn>,
    #[serde(default)]
    sex: Option<String>,
    #[serde(default)]
    age_years: Option<f64>,
    /// Hits per search arm in the reachability resolves (the menu each
    /// mention's name and quote are resolved against). Absent = the
    /// `icd10.resolve` default.
    #[serde(default)]
    per_arm_limit: Option<u32>,
}

pub async fn handle_verify(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    params: Value,
) -> Result<Value, RuntimeError> {
    let params: VerifyParams = serde_json::from_value(params)
        .map_err(|e| RuntimeError::InvalidInput(format!("invalid icd10.verify arguments: {e}")))?;

    let codes: Vec<String> = params
        .hypotheses
        .iter()
        .map(|h| normalize_code(&h.code))
        .collect();

    // Run `icd10.check` ONCE over the whole hypothesis code set: R1-R7 are
    // set-relative (R3 ancestor, R4 excludes1) so they must see every
    // hypothesis at once, not one at a time.
    let check_result = check::handle_check(
        runtime,
        token,
        json!({
            "codes": codes,
            "sex": params.sex,
            "age_years": params.age_years,
        }),
    )
    .await?;
    let check_verdicts = index_check_verdicts(&check_result);
    let excludes1_pairs = excludes1_pairs_by_code(&check_result);
    // Guideline combination/reporting_condition rules are set-relative
    // over the whole hypothesis set (a combination rule's premises may be
    // satisfied by two different hypotheses), so this runs once too, same
    // reasoning as `icd10.check` above.
    let guideline = compute_guideline(runtime, token, &codes).await?;

    let mut results = Vec::with_capacity(params.hypotheses.len());
    for h in &params.hypotheses {
        let code = normalize_code(&h.code);
        let entry = build_one(
            runtime,
            token,
            &code,
            &h.mention,
            &params.mentions,
            &codes,
            check_verdicts.get(&code),
            &excludes1_pairs,
            &guideline,
            params.per_arm_limit,
        )
        .await?;
        results.push(entry);
    }

    Ok(json!({ "verdicts": results }))
}

/// `check::handle_check`'s `verdicts` array, indexed by (normalized) code —
/// first entry wins, matching `check`'s own `seen` dedup.
fn index_check_verdicts(check_result: &Value) -> HashMap<String, Value> {
    let mut out = HashMap::new();
    if let Some(verdicts) = check_result["verdicts"].as_array() {
        for v in verdicts {
            if let Some(code) = v["code"].as_str() {
                out.entry(code.to_string()).or_insert_with(|| v.clone());
            }
        }
    }
    out
}

/// code -> the code it lost an R4 excludes1 conflict against (the "prev"
/// code the pair names), from `check_result["excludes1_pairs"]`.
fn excludes1_pairs_by_code(check_result: &Value) -> HashMap<String, String> {
    let mut out = HashMap::new();
    if let Some(pairs) = check_result["excludes1_pairs"].as_array() {
        for pair in pairs {
            if let Some(arr) = pair.as_array() {
                if arr.len() == 2 {
                    if let (Some(prev), Some(code)) = (arr[0].as_str(), arr[1].as_str()) {
                        out.insert(code.to_string(), prev.to_string());
                    }
                }
            }
        }
    }
    out
}

#[allow(clippy::too_many_arguments)]
async fn build_one(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    code: &str,
    mention: &MentionIn,
    all_mentions: &[MentionIn],
    hypothesis_codes: &[String],
    check_verdict: Option<&Value>,
    excludes1_pairs: &HashMap<String, String>,
    guideline: &GuidelineFacts,
    per_arm_limit: Option<u32>,
) -> Result<Value, RuntimeError> {
    let reachable = compute_reachable(runtime, token, code, mention, per_arm_limit).await?;
    let axis = compute_axis(runtime, token, code, mention, &reachable.index_term_paths).await?;
    let status = compute_status(mention);
    let obligations = compute_obligations(
        runtime,
        token,
        code,
        all_mentions,
        hypothesis_codes,
        per_arm_limit,
    )
    .await?;
    let evidence::MedicationEvidence {
        paths: support,
        abstentions: support_abstentions,
    } = compute_support(runtime, token, code, all_mentions).await?;
    let coherence = compute_coherence(runtime, token, code).await?;

    let mut reasons: Vec<Value> = Vec::new();
    let mut verdict: Option<&'static str> = None;
    let mut revise_to: Value = Value::Null;
    let mut companions: Value = Value::Null;

    // ── 0/1. Status facet + REJECT precedence ──────────────────────────────
    let status_claimed = status["claimed"].as_str().unwrap_or("current");
    let is_history_code = is_personal_history_code(code);
    let is_family_history_code = is_family_history_code(code);
    let is_symptom_code = is_symptom_range_code(code);
    let history_or_negated = status_claimed == "past" || status_claimed == "negated";
    // "family" is a mention STATUS value (the extractor's vocabulary is
    // current/past/negated/family/planned/uncertain), not a mention TYPE —
    // `mention.r#type` never enters this logic. A family-status mention is
    // REJECT unless the code is itself a family-history code (Z80-Z84);
    // this is a separate branch from past/negated (REJECT unless Z85-Z87),
    // not a variant exemption inside it.
    let family_status = status_claimed == "family";
    let uncertain = status_claimed == "uncertain";

    // A status the quote's own terms contradict is itself a reason to
    // list, independent of which verdict (if any) the status branches
    // below decide.
    if status["consistent"] == json!(false) {
        reasons.push(json!({
            "facet": "status",
            "rule": "status_inconsistent",
            "detail": format!(
                "mention status is claimed {status_claimed:?} but the quote's own terms disagree"
            ),
        }));
    }

    if history_or_negated {
        if !is_history_code {
            verdict = Some("REJECT");
            reasons.push(json!({
                "facet": "status",
                "rule": "status_reject",
                "detail": format!(
                    "mention status is {status_claimed:?}: not a current problem, and {code} \
                     is not itself a history code"
                ),
            }));
        }
    } else if family_status {
        if !is_family_history_code {
            verdict = Some("REJECT");
            reasons.push(json!({
                "facet": "status",
                "rule": "status_reject",
                "detail": format!(
                    "mention status is \"family\": a family-history condition, and {code} is not \
                     itself a family-history code (Z80-Z84)"
                ),
            }));
        }
    } else if uncertain && !is_symptom_code {
        verdict = Some("REJECT");
        reasons.push(json!({
            "facet": "status",
            "rule": "status_reject",
            "detail": "outpatient rule IV.H: uncertain-language diagnoses (probable/suspected/\
                questionable/rule out/consistent with/compatible with/working diagnosis) are not \
                coded except as R00-R99 symptom codes",
        }));
    }

    // ── 1. REJECT from the graph-set check (R1/R3/R4/R5/R6) ────────────────
    if verdict.is_none() {
        if let Some(cv) = check_verdict {
            let rule = cv["rule"].as_str().unwrap_or("");
            let detail = cv["detail"].as_str().unwrap_or("").to_string();
            if rule.starts_with("R1") {
                verdict = Some("REJECT");
                reasons.push(json!({"facet": "conflicts", "rule": rule, "detail": "unknown code"}));
            } else if rule.starts_with("R5") || rule.starts_with("R6") || rule.starts_with("R3") {
                verdict = Some("REJECT");
                reasons.push(json!({"facet": "conflicts", "rule": rule, "detail": detail}));
            } else if rule.starts_with("R4") {
                verdict = Some("REJECT");
                let with = excludes1_pairs.get(code).cloned();
                reasons.push(
                    json!({"facet": "conflicts", "rule": rule, "with": with, "detail": detail}),
                );
            }
        }
    }

    // ── 1b. REJECT from an unmet reporting_condition guideline rule ────────
    // Same precedence bucket as the R-rule REJECTs above:
    // a `reporting_condition` rule (e.g. Z68 needs an E66 hypothesis) whose
    // required companion is absent from the hypothesis set REJECTs, same as
    // an R1/R3/R4/R5/R6 conflict would.
    if verdict.is_none() {
        if let Some(unmet) = guideline.reject_by_code.get(code) {
            if !unmet.is_empty() {
                verdict = Some("REJECT");
                for u in unmet {
                    reasons.push(json!({
                        "facet": "guideline",
                        "rule": "reporting_condition_unmet",
                        "rule_id": u["rule_id"],
                        "section": u["section"],
                        "detail": u["quote"],
                    }));
                }
            }
        }
    }

    // ── 2. REVISE (not billable / 7th char repair, then axis) ──────────────
    if verdict.is_none() {
        if let Some(cv) = check_verdict {
            let rule = cv["rule"].as_str().unwrap_or("");
            let action = cv["action"].as_str().unwrap_or("");
            if rule.starts_with("R2") || rule.starts_with("R7") {
                if action == "replace" {
                    verdict = Some("REVISE");
                    revise_to = cv["replacement"].clone();
                    reasons.push(json!({
                        "facet": "conflicts",
                        "rule": rule,
                        "detail": cv["detail"].as_str().unwrap_or(""),
                    }));
                } else {
                    // action == "drop": no unique billable/unspecified repair
                    // exists — R2/R7 offer no `revise_to`, so this cannot be
                    // a REVISE; treat as a REJECT (the code as hypothesized
                    // cannot be billed and the graph has no single repair).
                    verdict = Some("REJECT");
                    reasons.push(json!({
                        "facet": "conflicts",
                        "rule": rule,
                        "detail": cv["detail"].as_str().unwrap_or(""),
                    }));
                }
            }
        }
    }
    if verdict.is_none() && reachable.category_reached && axis["settled"] == json!(false) {
        if let Some(cp) = axis["counter_proposal"].as_str() {
            verdict = Some("REVISE");
            revise_to = json!(cp);
            reasons.push(json!({
                "facet": "axis",
                "rule": "axis_unsettled",
                "detail": format!(
                    "{}: quote carries no {} term",
                    axis["axis"].as_str().unwrap_or("axis"),
                    axis["terms_needed"]
                ),
            }));
        } else {
            // The axis rule fired (it left the axis unsettled) even though
            // there is no unspecified sibling to revise to, and every rule
            // that fires must be listed in `reasons` — the verdict stays
            // whatever precedence decides next (CONFIRMED, most likely,
            // since nothing above overrode it).
            reasons.push(json!({
                "facet": "axis",
                "rule": "axis_unsettled_no_unspecified_sibling",
                "detail": format!(
                    "{}: quote carries no {} term and no unspecified sibling exists to revise to",
                    axis["axis"].as_str().unwrap_or("axis"),
                    axis["terms_needed"]
                ),
            }));
        }
    }

    // ── 3. NEEDS_COMPANION ───────────────────────────────────────────────
    if verdict.is_none() {
        let mut needed: Vec<String> = Vec::new();
        if let Some(obs) = obligations.as_array() {
            for o in obs {
                let present = o["present"] == json!(true);
                if present {
                    continue;
                }
                let kind = o["kind"].as_str().unwrap_or("");
                let hard = kind == "code_first";
                let soft_triggers =
                    kind == "use_additional" && o["supported_by_mention"] == json!(true);
                if hard || soft_triggers {
                    if let Some(t) = o["target"].as_str() {
                        needed.push(t.to_string());
                    }
                    reasons.push(json!({
                        "facet": "obligations",
                        "rule": if hard { "code_first_unmet" } else { "use_additional_unmet" },
                        "detail": format!("{kind} partner {} is not in the hypothesis set", o["target"]),
                    }));
                }
            }
        }
        if !needed.is_empty() {
            verdict = Some("NEEDS_COMPANION");
            companions = json!(needed);
        }
    }

    // ── 4. CONFIRMED ─────────────────────────────────────────────────────
    if verdict.is_none() && reachable.ok {
        verdict = Some("CONFIRMED");
    }

    // ── 5. UNSUPPORTED ──────────────────────────────────────────────────
    if verdict.is_none() {
        verdict = Some("UNSUPPORTED");
        reasons.push(json!({
            "facet": "reachable",
            "rule": "not_reachable",
            "detail": "no Index or Tabular path from this mention's name or quote to this code",
        }));
    }

    // Support (medication-path evidence) never alone confirms —
    // `reachable.ok` (Index/Tabular text) is the only input to the CONFIRMED
    // branch above, so a code that only has `support` evidence lands here,
    // UNSUPPORTED, and is additionally tagged so a caller can tell "no
    // evidence at all" apart from "evidence exists but isn't the kind that
    // confirms".
    let support_only = verdict == Some("UNSUPPORTED") && !support.is_empty();
    if support_only {
        reasons.push(json!({
            "facet": "support",
            "rule": "support_only",
            "detail": "medication-path evidence exists for this code, but support alone never \
                confirms a hypothesis — only Index/Tabular text reachability does",
        }));
    }

    let proposals = guideline
        .proposals_by_code
        .get(code)
        .cloned()
        .unwrap_or_default();

    let mut edges: Vec<Value> = Vec::new();
    edges.extend(reachable.edges.clone());
    edges.extend(conflict_edges(code, check_verdict, excludes1_pairs));
    edges.extend(obligation_edges(code, &obligations));
    // Support is drawn for every verdict. It is display only: the verdict
    // above was decided before this line and never reads these edges.
    edges.extend(support_edges(code, &support));

    Ok(json!({
        "code": code,
        "verdict": verdict,
        "revise_to": revise_to,
        "companions": companions,
        "support_only": support_only,
        "reasons": reasons,
        "facets": {
            "reachable": {
                "ok": reachable.ok,
                "paths": reachable.paths,
                "nearest_reachable": reachable.nearest_reachable,
            },
            "axis": axis,
            "status": status,
            "conflicts": conflicts_facet(code, check_verdict, excludes1_pairs),
            "obligations": obligations,
            "support": support,
            "support_abstentions": support_abstentions,
            "coherence": coherence,
        },
        "proposals": proposals,
        "edges": edges,
    }))
}

// ── reachable ────────────────────────────────────────────────────────────

struct Reachable {
    ok: bool,
    paths: Vec<Value>,
    nearest_reachable: Option<String>,
    /// Every distinct `term_path` of a reaching Index-arm match (not
    /// `via: family`) — fed into the axis facet's subterm-on-term-path
    /// settlement rule for non-laterality axes. A code can be reached by
    /// more than one Index arm (e.g. a main-term default entry and a more
    /// specific subterm entry), and the axis is settled if ANY reaching
    /// path settles it — so every path is kept, not just the first.
    index_term_paths: Vec<String>,
    edges: Vec<Value>,
    /// Whether the mention's own resolve (name or quote, any arm/via) found
    /// ANY code in the hypothesis code's own 3-character category — a
    /// weaker signal than `ok` (which requires the exact code, not-family).
    /// Gates axis-driven REVISE: axis settlement answers "which sibling",
    /// and that question is only meaningful once the mention has pointed at
    /// the family at all — an entirely unrelated mention must not REVISE a
    /// code it never touched just because that code happens to carry an
    /// axis (the REVISE-on-axis check in `build_one`'s precedence order).
    category_reached: bool,
}

async fn compute_reachable(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    code: &str,
    mention: &MentionIn,
    per_arm_limit: Option<u32>,
) -> Result<Reachable, RuntimeError> {
    let name_result = resolve_any_term(runtime, token, &mention.name, per_arm_limit).await?;
    let quote_result = resolve_any_term(runtime, token, &mention.quote, per_arm_limit).await?;

    let mut ok = false;
    let mut paths: Vec<Value> = Vec::new();
    let mut edges: Vec<Value> = Vec::new();
    let mut index_term_paths: Vec<String> = Vec::new();
    let mut all_reached_codes: HashSet<String> = HashSet::new();

    for resolved in [&name_result, &quote_result] {
        let Some(candidates) = resolved["mentions"][0]["candidates"].as_array() else {
            continue;
        };
        for c in candidates {
            let Some(c_code) = c["code"].as_str() else {
                continue;
            };
            all_reached_codes.insert(c_code.to_string());
            if c_code != code {
                continue;
            }
            // `icd_code` path/edge nodes must label with the code's own
            // description, not the bare code string.
            let c_desc = c["desc"].as_str().filter(|d| !d.is_empty()).unwrap_or(code);
            let Some(matched) = c["matched"].as_array() else {
                continue;
            };
            for m in matched {
                if m["via"] == json!("family") {
                    continue;
                }
                ok = true;
                let arm = m["arm"].as_str().unwrap_or("");
                let via = m["via"].as_str().unwrap_or("");
                let term_path = m["term_path"].as_str();
                if arm == "index" {
                    if let Some(tp) = term_path {
                        if !index_term_paths.iter().any(|existing| existing == tp) {
                            index_term_paths.push(tp.to_string());
                        }
                    }
                }
                let (node, relation, source) = match (arm, term_path) {
                    ("index", Some(tp)) => (
                        json!({"kind": "icd_term", "key": tp, "label": tp}),
                        "variant_of",
                        "CMS ICD-10-CM Index FY2026",
                    ),
                    _ => (
                        json!({"kind": "icd_code", "key": code, "label": c_desc}),
                        "text_match",
                        "CMS ICD-10-CM Tabular FY2026",
                    ),
                };
                paths.push(json!([{
                    "node": node,
                    "relation": relation,
                    "edge": {"icd": format!("{arm}_{via}"), "source": source},
                }]));
                if arm == "index" {
                    if let Some(tp) = term_path {
                        edges.push(json!({
                            "src": format!("icd_term:{tp}"),
                            "dst": format!("icd_code:{code}"),
                            "rel": "variant_of",
                            "icd": format!("index_{via}"),
                            "source": "CMS ICD-10-CM Index FY2026",
                            "props": {},
                        }));
                    }
                }
            }
        }
    }

    let nearest_reachable = if ok {
        None
    } else {
        nearest_in_category(code, &all_reached_codes)
    };
    let category = category_of(code);
    let category_reached = ok || all_reached_codes.iter().any(|c| category_of(c) == category);

    Ok(Reachable {
        ok,
        paths,
        nearest_reachable,
        index_term_paths,
        edges,
        category_reached,
    })
}

async fn resolve_any_term(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    text: &str,
    per_arm_limit: Option<u32>,
) -> Result<Value, RuntimeError> {
    if text.trim().is_empty() {
        return Ok(json!({"mentions": [{"candidates": []}]}));
    }
    // The limit is part of the key: one request never mixes limits, but the
    // key says what was resolved, not what the caller happened to pass.
    crate::memo::resolved(
        format!("any_term:{per_arm_limit:?}:{text}"),
        resolve::resolve_for_reachability(runtime, token, text, per_arm_limit),
    )
    .await
}

/// The 3-character ICD-10-CM category (everything before the first `.`).
fn category_of(code: &str) -> &str {
    code.split('.').next().unwrap_or(code)
}

fn common_prefix_len(a: &str, b: &str) -> usize {
    a.chars().zip(b.chars()).take_while(|(x, y)| x == y).count()
}

/// The reached code (any arm, any via) sharing `code`'s 3-character category
/// with the longest common prefix against `code` — `reachable`'s
/// `nearest_reachable`, computed only when `ok` is false.
fn nearest_in_category(code: &str, reached: &HashSet<String>) -> Option<String> {
    let category = category_of(code);
    reached
        .iter()
        .filter(|c| category_of(c) == category && c.as_str() != code)
        .max_by_key(|c| (common_prefix_len(c, code), std::cmp::Reverse(c.to_string())))
        .cloned()
}

// ── axis ─────────────────────────────────────────────────────────────────

async fn compute_axis(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    code: &str,
    mention: &MentionIn,
    index_term_paths: &[String],
) -> Result<Value, RuntimeError> {
    let nb = neighborhood::handle_neighborhood(runtime, token, json!({"code": code})).await;
    let nb = match nb {
        Ok(v) => v,
        Err(_) => {
            // Unknown code (e.g. it failed R1 above) — no siblings to reason
            // about.
            return Ok(json!({
                "axis": null, "settled": null, "terms_needed": [],
                "terms_found_in_quote": [], "counter_proposal": null,
                "settled_by": null, "settled_path": null,
            }));
        }
    };
    let siblings = nb["siblings"].as_array().cloned().unwrap_or_default();
    if siblings.is_empty() {
        return Ok(json!({
            "axis": null, "settled": null, "terms_needed": [],
            "terms_found_in_quote": [], "counter_proposal": null,
            "settled_by": null, "settled_path": null,
        }));
    }

    let axis = pick_dominant_axis(&siblings);
    let quote_words = tokenize(&mention.quote);
    let hyp_desc = desc_of(&nb["record"]);

    match axis {
        "laterality" => {
            let own_laterality = nb["record"]["properties"]["laterality"].as_str();
            let Some(own) = own_laterality else {
                // The hypothesis IS the (unlateralized) unspecified code —
                // nothing to settle.
                return Ok(json!({
                    "axis": "laterality", "settled": true, "terms_needed": [],
                    "terms_found_in_quote": [], "counter_proposal": null,
                    "settled_by": "code_already_unspecified", "settled_path": null,
                }));
            };
            let terms_needed: Vec<&str> = match own {
                "right" => vec!["right"],
                "left" => vec!["left"],
                "bilateral" => vec!["bilateral", "both"],
                _ => vec![],
            };
            let terms_found: Vec<&str> = terms_needed
                .iter()
                .copied()
                .filter(|t| contains_phrase(&quote_words, t))
                .collect();
            let settled = terms_needed.is_empty() || !terms_found.is_empty();
            let settled_by = if terms_needed.is_empty() {
                Some("no_terms_needed")
            } else if !terms_found.is_empty() {
                Some("quote_match")
            } else {
                None
            };
            let counter_proposal = if settled {
                None
            } else {
                axis_only_unspecified_sibling(&hyp_desc, &siblings)
            };
            Ok(json!({
                "axis": "laterality",
                "settled": settled,
                "terms_needed": terms_needed,
                "terms_found_in_quote": terms_found,
                "counter_proposal": counter_proposal,
                "settled_by": settled_by,
                "settled_path": Value::Null,
            }))
        }
        other => {
            // General rule: a subterm on a reaching Index term_path (the
            // leaf segment not shared with the parent term)
            // must appear in the quote, with parenthesized nonessential
            // modifiers stripped before matching AND before reporting. A
            // reaching path with no subterm at all (a bare main term) is the
            // Index's default entry for that term — it carries no
            // specificity requirement of its own and auto-settles. The axis
            // is settled if ANY reaching path settles it; we report which.
            let mut terms_needed: Vec<String> = Vec::new();
            let mut terms_found: Vec<String> = Vec::new();
            let mut settled = false;
            let mut settled_by: Option<&'static str> = None;
            let mut settled_path: Option<String> = None;

            for tp in index_term_paths {
                if !tp.contains(" > ") {
                    // Bare main term reached with no subterm below it: the
                    // Index default, no specificity constraint.
                    if !settled {
                        settled = true;
                        settled_by = Some("index_default");
                        settled_path = Some(tp.clone());
                    }
                    continue;
                }
                let raw_subterm = tp.rsplit(" > ").next().unwrap_or(tp.as_str());
                let stripped = strip_nonessential_modifiers(raw_subterm).to_lowercase();
                if stripped.is_empty() {
                    continue;
                }
                if !terms_needed.contains(&stripped) {
                    terms_needed.push(stripped.clone());
                }
                if contains_phrase(&quote_words, &stripped) {
                    if !terms_found.contains(&stripped) {
                        terms_found.push(stripped.clone());
                    }
                    if !settled {
                        settled = true;
                        settled_by = Some("quote_match");
                        settled_path = Some(tp.clone());
                    }
                }
            }

            let counter_proposal = if settled {
                None
            } else {
                axis_only_unspecified_sibling(&hyp_desc, &siblings)
            };
            Ok(json!({
                "axis": other,
                "settled": settled,
                "terms_needed": terms_needed,
                "terms_found_in_quote": terms_found,
                "counter_proposal": counter_proposal,
                "settled_by": settled_by,
                "settled_path": settled_path,
            }))
        }
    }
}

/// Strip parenthesized nonessential-modifier text — e.g.
/// `"osteoporosis (female) (male)"` -> `"osteoporosis"`, `"gerd
/// (gastroesophageal reflux disease)"` -> `"gerd"` — per Index convention:
/// text in parentheses after a main term or subterm does not affect the
/// code and must not be required to appear in the quote.
fn strip_nonessential_modifiers(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut depth: u32 = 0;
    for ch in s.chars() {
        match ch {
            '(' => depth += 1,
            ')' => depth = depth.saturating_sub(1),
            _ if depth == 0 => out.push(ch),
            _ => {}
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn pick_dominant_axis(siblings: &[Value]) -> &'static str {
    let mut seen: HashSet<&str> = HashSet::new();
    for s in siblings {
        if let Some(d) = s["differs_on"].as_str() {
            seen.insert(d);
        }
    }
    for candidate in ["laterality", "specificity", "7th_character"] {
        if seen.contains(candidate) {
            return candidate;
        }
    }
    "other"
}

/// The sibling whose description differs from the hypothesis code's own
/// description in exactly one token position, with the sibling's token at
/// that position reading "unspecified" — the axis-only-unspecified sibling.
/// Compared token by token, not by `differs_on` or the
/// sibling's own properties: the old property-based check
/// (`differs_on == axis` + no own axis property + "unspecified" anywhere in
/// the description) picked M79.676 "Pain in unspecified toe(s)" as the
/// counter-proposal for M79.671 "Pain in right foot", because M79.676 also
/// carries no laterality of its own and its description also contains
/// "unspecified" — even though it differs from the hypothesis on TWO things
/// (laterality AND site), not one. A sibling whose description has a
/// different token count than the hypothesis's own description (e.g.
/// "toe(s)" tokenizing to two words against "foot"'s one) cannot be an
/// axis-only difference and is rejected on that basis alone.
fn axis_only_unspecified_sibling(hyp_desc: &str, siblings: &[Value]) -> Option<String> {
    let hyp_tokens = tokenize(hyp_desc);
    siblings.iter().find_map(|s| {
        let sib_desc = desc_of(s);
        let sib_tokens = tokenize(&sib_desc);
        if sib_tokens.len() != hyp_tokens.len() {
            return None;
        }
        let mut diff_at: Option<usize> = None;
        for (i, (h, sv)) in hyp_tokens.iter().zip(sib_tokens.iter()).enumerate() {
            if h != sv {
                if diff_at.is_some() {
                    return None; // differs in more than one position
                }
                diff_at = Some(i);
            }
        }
        match diff_at {
            Some(i) if sib_tokens[i] == "unspecified" => s["name"].as_str().map(String::from),
            _ => None,
        }
    })
}

fn desc_of(entity: &Value) -> String {
    entity["properties"]["desc"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}

// ── status ───────────────────────────────────────────────────────────────

const HISTORY_TERMS: &[&str] = &["history of", "past", "previous", "resolved"];
const NEGATION_TERMS: &[&str] = &["denies", "denied", "no", "negative for", "ruled out"];
const UNCERTAIN_TERMS: &[&str] = &[
    "probable",
    "suspected",
    "questionable",
    "rule out",
    "consistent with",
    "compatible with",
    "working diagnosis",
];

fn compute_status(mention: &MentionIn) -> Value {
    let words = tokenize(&mention.quote);
    let mut found: Vec<&str> = Vec::new();
    for t in HISTORY_TERMS
        .iter()
        .chain(NEGATION_TERMS)
        .chain(UNCERTAIN_TERMS)
    {
        if contains_phrase(&words, t) {
            found.push(t);
        }
    }
    let claimed = mention.status.as_str();
    let history_found = HISTORY_TERMS.iter().any(|t| found.contains(t));
    let negation_found = NEGATION_TERMS.iter().any(|t| found.contains(t));
    let uncertain_found = UNCERTAIN_TERMS.iter().any(|t| found.contains(t));
    let quote_implies_past_or_negated = history_found || negation_found;
    let consistent = match claimed {
        "past" | "negated" => quote_implies_past_or_negated || found.is_empty(),
        "uncertain" => uncertain_found || found.is_empty(),
        _ => !quote_implies_past_or_negated,
    };
    json!({
        "claimed": claimed,
        "quote_terms_found": found,
        "consistent": consistent,
    })
}

fn is_personal_history_code(code: &str) -> bool {
    code.starts_with("Z85") || code.starts_with("Z86") || code.starts_with("Z87")
}

fn is_family_history_code(code: &str) -> bool {
    code.starts_with("Z80")
        || code.starts_with("Z81")
        || code.starts_with("Z82")
        || code.starts_with("Z83")
        || code.starts_with("Z84")
}

fn is_symptom_range_code(code: &str) -> bool {
    code.starts_with('R')
        && code
            .chars()
            .nth(1)
            .map(|c| c.is_ascii_digit())
            .unwrap_or(false)
}

// ── obligations ──────────────────────────────────────────────────────────

async fn compute_obligations(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    code: &str,
    all_mentions: &[MentionIn],
    hypothesis_codes: &[String],
    per_arm_limit: Option<u32>,
) -> Result<Value, RuntimeError> {
    let nb = match neighborhood::handle_neighborhood(runtime, token, json!({"code": code})).await {
        Ok(v) => v,
        Err(_) => return Ok(json!([])),
    };
    let hyp_set: HashSet<&str> = hypothesis_codes.iter().map(String::as_str).collect();

    let mut out: Vec<Value> = Vec::new();

    if let Some(code_first) = nb["code_first"].as_array() {
        for partner in code_first {
            let Some(target) = partner["name"].as_str() else {
                continue;
            };
            let present = hyp_set.contains(target);
            out.push(json!({
                "kind": "code_first",
                "target": target,
                "present": present,
                "supported_by_mention": Value::Null,
            }));
        }
    }

    if let Some(use_additional) = nb["use_additional"].as_array() {
        for partner in use_additional {
            let Some(target) = partner["name"].as_str() else {
                continue;
            };
            let present = hyp_set.contains(target);
            let supported_by_mention = if present {
                false
            } else {
                mention_reaches_category(
                    runtime,
                    token,
                    all_mentions,
                    category_of(target),
                    per_arm_limit,
                )
                .await?
            };
            out.push(json!({
                "kind": "use_additional",
                "target": target,
                "present": present,
                "supported_by_mention": supported_by_mention,
            }));
        }
    }

    Ok(json!(out))
}

/// Whether any visit mention's own resolve (name or quote, `any_term`,
/// direct/see — never `family`) reaches a code in `category` — the check
/// `icd10.verify`'s NEEDS_COMPANION rule uses to decide whether an unmet
/// `use_additional` obligation is hard (a visit mention supports it) or
/// merely advisory.
async fn mention_reaches_category(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    mentions: &[MentionIn],
    category: &str,
    per_arm_limit: Option<u32>,
) -> Result<bool, RuntimeError> {
    for m in mentions {
        for text in [m.name.as_str(), m.quote.as_str()] {
            let resolved = resolve_any_term(runtime, token, text, per_arm_limit).await?;
            let Some(candidates) = resolved["mentions"][0]["candidates"].as_array() else {
                continue;
            };
            for c in candidates {
                let Some(c_code) = c["code"].as_str() else {
                    continue;
                };
                if category_of(c_code) != category {
                    continue;
                }
                let Some(matched) = c["matched"].as_array() else {
                    continue;
                };
                if matched.iter().any(|mm| mm["via"] != json!("family")) {
                    return Ok(true);
                }
            }
        }
    }
    Ok(false)
}

// ── support (medication-path evidence) ────────────────────────────────

/// Medication paths and lookup abstentions from current `type == "medication"`
/// mentions in this visit, via the same drug -[enables]-> mesh_disease
/// -[variant_of]-> icd category/code walk `icd10.evidence` uses. Only paths
/// contribute to `support_only` and `supports` edges; abstentions are trace
/// metadata and never change a verdict.
async fn compute_support(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    code: &str,
    all_mentions: &[MentionIn],
) -> Result<evidence::MedicationEvidence, RuntimeError> {
    let mut result = evidence::MedicationEvidence {
        paths: Vec::new(),
        abstentions: Vec::new(),
    };
    let Some(target) = neighborhood::find_by_code(runtime, token, code).await? else {
        return Ok(result);
    };
    for m in all_mentions {
        // A past, negated, family, planned, or uncertain medication mention
        // cannot support a present-disease hypothesis. Unknown statuses also
        // abstain; the request schema defaults an omitted status to current.
        if m.r#type != "medication" || !m.status.eq_ignore_ascii_case("current") {
            continue;
        }
        let mut mention_evidence =
            evidence::medication_evidence_for_mention(runtime, token, &m.name, target.id, code)
                .await?;
        result.paths.append(&mut mention_evidence.paths);
        result.abstentions.append(&mut mention_evidence.abstentions);
    }
    Ok(result)
}

// ── coherence (CCSR) ────────────────────────────────────────────────────

/// Does `code` itself carry an AHRQ DXCCSR category (a
/// `-[PartOf]-> ccsr_category` edge, `icd10.ingest_external`'s `ccsr_csv`
/// arm)? When it does, `code_ccsr`/`mention_ccsr` are populated from it (this
/// crate's fixture pair does not ingest CCSR, so `mention_ccsr` — which
/// would need a per-mention CCSR lookup this crate has no mention-side
/// source for — is documented here as mirroring `code_ccsr` rather than
/// independently derived) and `consistent` is `true`. This is a narrower
/// reading than a full mention-vs-code CCSR mismatch check.
async fn compute_coherence(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    code: &str,
) -> Result<Value, RuntimeError> {
    let not_ingested = json!({
        "code_ccsr": [],
        "mention_ccsr": [],
        "consistent": null,
        "note": "no CCSR category is reachable from this code in this graph; \
            icd10.ingest_external loads CCSR categories from its ccsr_csv source",
    });
    let Some(target) = neighborhood::find_by_code(runtime, token, code).await? else {
        return Ok(not_ingested);
    };
    let ccsr_edges = crate::memo::list_edges(
        runtime,
        token,
        EdgeListFilter {
            source_id: Some(target.id),
            relations: vec![EdgeRelation::PartOf],
            ..Default::default()
        },
        1000,
        0,
    )
    .await?;
    if ccsr_edges.is_empty() {
        return Ok(not_ingested);
    }
    let mut cats: Vec<Value> = Vec::new();
    for e in &ccsr_edges {
        let hit = crate::memo::entity_by_id(runtime, token, e.target_id)
            .await?
            .filter(|c| c.entity_type.as_deref() == Some("ccsr_category"));
        if let Some(cat) = hit {
            cats.push(json!({"code": cat.name, "desc": cat.description}));
        }
    }
    if cats.is_empty() {
        return Ok(not_ingested);
    }
    Ok(json!({
        "code_ccsr": cats.clone(),
        "mention_ccsr": cats,
        "consistent": true,
        "note": "coherence is narrowed to \"does this code itself carry a CCSR category\", \
            not a full mention-vs-code CCSR match",
    }))
}

// ── guideline (combination + reporting_condition rules) ────────────────

/// Set-relative over the whole hypothesis set, computed once by
/// `handle_verify` and handed to every `build_one` call (same reasoning as
/// `icd10.check`'s own set-relative rules: a `combination` rule's premises
/// may be satisfied by two different hypotheses).
#[derive(Debug, Default)]
struct GuidelineFacts {
    /// hypothesis code -> `{to, rule_id, section, quote}` proposals from
    /// `combination` rules whose premises that code helped satisfy. A
    /// `combination` rule's premises being present never changes the
    /// verdict of any hypothesis — this is advisory only.
    proposals_by_code: HashMap<String, Vec<Value>>,
    /// hypothesis code -> `{rule_id, section, quote}` for each
    /// `reporting_condition` rule whose required companion is absent from
    /// the hypothesis set. Unlike `proposals_by_code`, this DOES drive a
    /// REJECT verdict (`build_one`'s precedence step 1b), same bucket as the
    /// R-rule REJECTs.
    reject_by_code: HashMap<String, Vec<Value>>,
}

async fn compute_guideline(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    hypothesis_codes: &[String],
) -> Result<GuidelineFacts, RuntimeError> {
    let mut facts = GuidelineFacts::default();

    // Resolve every hypothesis code to its own entity once; codes that
    // don't resolve to a real entity can't satisfy or need anything below.
    let mut hyp_ids: Vec<(String, Uuid)> = Vec::new();
    for code in hypothesis_codes {
        if let Some(e) = neighborhood::find_by_code(runtime, token, code).await? {
            hyp_ids.push((code.clone(), e.id));
        }
    }
    if hyp_ids.is_empty() {
        return Ok(facts);
    }

    let rules = crate::memo::entities_of_type(runtime, token, "guideline_rule", 10_000, 0).await?;

    for rule in &rules {
        let kind = rule
            .properties
            .as_ref()
            .and_then(|p| p.get("kind"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let section = rule
            .properties
            .as_ref()
            .and_then(|p| p.get("section"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let quote = rule
            .properties
            .as_ref()
            .and_then(|p| p.get("quote"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        // Out of scope here (`use_additional` and any other kind): the
        // obligations facet already covers `use_additional` via each code's
        // own `code_also`/`code_first`/`use_additional` instructional-note
        // edges (`compute_obligations`), so a guideline-rule row of that
        // kind is deliberately not read here.
        if kind != "combination" && kind != "reporting_condition" {
            continue;
        }

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
        if premise_edges.is_empty() || conclusion_edges.is_empty() {
            continue;
        }

        if kind == "combination" {
            // Every premise must be satisfied (category-or-below) by SOME
            // hypothesis code; if all are, every satisfying hypothesis gets
            // a proposal per conclusion.
            let mut satisfying: Vec<String> = Vec::new();
            let mut all_present = true;
            for pe in &premise_edges {
                let mut hit: Option<String> = None;
                for (hcode, hid) in &hyp_ids {
                    if evidence::is_self_or_descendant(runtime, token, pe.target_id, *hid).await? {
                        hit = Some(hcode.clone());
                        break;
                    }
                }
                match hit {
                    Some(hcode) => {
                        if !satisfying.contains(&hcode) {
                            satisfying.push(hcode);
                        }
                    }
                    None => {
                        all_present = false;
                        break;
                    }
                }
            }
            if !all_present {
                continue;
            }
            for ce in &conclusion_edges {
                let concl_name = crate::memo::entity_by_id(runtime, token, ce.target_id)
                    .await?
                    .map(|e| e.name);
                let Some(concl_name) = concl_name else {
                    continue;
                };
                for hcode in &satisfying {
                    facts
                        .proposals_by_code
                        .entry(hcode.clone())
                        .or_default()
                        .push(json!({
                            "to": concl_name,
                            "rule_id": rule.name,
                            "section": section,
                            "quote": quote,
                        }));
                }
            }
        } else {
            // `reporting_condition`: `conclusions` names the code that
            // NEEDS a companion (e.g. Z68), `premises` names the required
            // companion(s) (e.g. E66) — reversed roles from `combination`.
            // Any hypothesis that is (self-or-descendant of) a conclusion
            // target REJECTs unless at least one premise is present
            // (category-or-below) among the hypothesis set.
            let mut any_premise_present = false;
            for pe in &premise_edges {
                for (_, hid) in &hyp_ids {
                    if evidence::is_self_or_descendant(runtime, token, pe.target_id, *hid).await? {
                        any_premise_present = true;
                        break;
                    }
                }
                if any_premise_present {
                    break;
                }
            }
            if any_premise_present {
                continue;
            }
            for ce in &conclusion_edges {
                for (hcode, hid) in &hyp_ids {
                    if evidence::is_self_or_descendant(runtime, token, ce.target_id, *hid).await? {
                        facts
                            .reject_by_code
                            .entry(hcode.clone())
                            .or_default()
                            .push(json!({
                                "rule_id": rule.name,
                                "section": section,
                                "quote": quote,
                            }));
                    }
                }
            }
        }
    }

    Ok(facts)
}

// ── conflicts (rendered) + edges ────────────────────────────────────────

fn conflicts_facet(
    code: &str,
    check_verdict: Option<&Value>,
    excludes1_pairs: &HashMap<String, String>,
) -> Value {
    let Some(cv) = check_verdict else {
        return json!([]);
    };
    let rule = cv["rule"].as_str().unwrap_or("");
    if rule.is_empty() || rule == "ok" {
        return json!([]);
    }
    let with = excludes1_pairs.get(code).cloned();
    json!([{
        "rule": rule,
        "with": with,
        "detail": cv["detail"].as_str().unwrap_or(""),
    }])
}

fn conflict_edges(
    code: &str,
    check_verdict: Option<&Value>,
    excludes1_pairs: &HashMap<String, String>,
) -> Vec<Value> {
    let mut out = Vec::new();
    if let Some(cv) = check_verdict {
        let rule = cv["rule"].as_str().unwrap_or("");
        if rule.starts_with("R4") {
            if let Some(with) = excludes1_pairs.get(code) {
                out.push(json!({
                    "src": format!("icd_code:{code}"),
                    "dst": format!("icd_code:{with}"),
                    "rel": "competes_with",
                    "icd": "excludes1",
                    "source": "CMS ICD-10-CM Tabular FY2026",
                    "props": {},
                }));
            }
        }
    }
    out
}

fn obligation_edges(code: &str, obligations: &Value) -> Vec<Value> {
    let mut out = Vec::new();
    let Some(obs) = obligations.as_array() else {
        return out;
    };
    for o in obs {
        let Some(target) = o["target"].as_str() else {
            continue;
        };
        let kind = o["kind"].as_str().unwrap_or("");
        let (src, dst) = if kind == "code_first" {
            (format!("icd_code:{target}"), format!("icd_code:{code}"))
        } else {
            (format!("icd_code:{code}"), format!("icd_code:{target}"))
        };
        out.push(json!({
            "src": src,
            "dst": dst,
            "rel": "precedes",
            "icd": kind,
            "source": "CMS ICD-10-CM Tabular FY2026",
            "props": {},
        }));
    }
    out
}

/// One `supports` edge per medication-support path: the medication mention
/// (the path's first step, keyed by the mention name) to the hypothesis
/// code. `icd` names the medication relation carried on the path's
/// drug -> mesh_disease hop (e.g. `may_treat`), and `props.path` keeps the
/// full hop list so a viewer can show drug, disease and category.
fn support_edges(code: &str, support: &[Value]) -> Vec<Value> {
    let mut out = Vec::new();
    for path in support {
        let Some(steps) = path.as_array() else {
            continue;
        };
        let Some(mention) = steps.first().and_then(|s| s["node"]["key"].as_str()) else {
            continue;
        };
        let find_kind = |kind: &str| steps.iter().find(|s| s["node"]["kind"] == json!(kind));
        let mesh_step = find_kind("mesh_disease");
        let relation = match mesh_step.map(|s| &s["edge"]["icd"]) {
            Some(Value::String(r)) => r.clone(),
            Some(Value::Array(rs)) => rs
                .iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(","),
            _ => String::new(),
        };
        let relation = if relation.is_empty() {
            "medication".to_string()
        } else {
            relation
        };
        let source = mesh_step
            .and_then(|s| s["edge"]["source"].as_str())
            .map_or(Value::Null, |s| json!(s));
        out.push(json!({
            "src": format!("mention:{mention}"),
            "dst": format!("icd_code:{code}"),
            "rel": "supports",
            "icd": relation,
            "source": source,
            "props": {
                "via": "medication",
                "drug": find_kind("drug").map(|s| s["node"]["label"].clone()),
                "mesh_disease": mesh_step.map(|s| s["node"]["label"].clone()),
                "path": path,
            },
        }));
    }
    out
}

// ── shared text helpers ────────────────────────────────────────────────

/// Lowercase, whitespace-normalized quote, split into alphanumeric words —
/// used for phrase matching with real word boundaries (a bare substring
/// search would let "right" match inside "copyright", or "no" match inside
/// "normal").
fn tokenize(text: &str) -> Vec<String> {
    text.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(String::from)
        .collect()
}

/// Whether `phrase` (one or more whitespace-separated words) appears in
/// `words` as a contiguous, word-boundary-respecting run.
fn contains_phrase(words: &[String], phrase: &str) -> bool {
    let p: Vec<&str> = phrase.split_whitespace().collect();
    if p.is_empty() {
        return false;
    }
    if p.len() == 1 {
        return words.iter().any(|w| w == p[0]);
    }
    words
        .windows(p.len())
        .any(|w| w.iter().map(String::as_str).collect::<Vec<_>>() == p)
}

#[cfg(test)]
mod tests {
    //! Unit-level coverage for `axis_only_unspecified_sibling` against a
    //! hand-built `Vec<Value>` in a fixed literal order
    //! — deliberately NOT going through `icd10.neighborhood`/the storage
    //! backend, because the in-memory test runtime's `list_edges` sibling
    //! order was measured to vary between separate `cargo test` process
    //! invocations on identical fixture content (HashMap-seed-dependent),
    //! which makes any must-fail control keyed on that order unreliable.
    //! This test controls order directly via the Vec literal instead.

    use super::*;

    fn sibling(name: &str, desc: &str, laterality: Option<&str>) -> Value {
        json!({
            "name": name,
            "entity_type": "icd_code",
            "properties": {
                "desc": desc,
                "laterality": laterality,
            },
        })
    }

    #[test]
    fn axis_only_unspecified_sibling_prefers_the_single_token_match_over_the_wrong_site() {
        // The unspecified-TOE sibling is listed FIRST (matching the real
        // M79.671 defect: whichever "no laterality of its own + desc says
        // unspecified" sibling iteration meets first was the old bug's
        // pick), and the unspecified-FOOT sibling (the only one differing
        // from the hypothesis in exactly the laterality token) is listed
        // after it.
        let siblings = vec![
            sibling("M79.674", "Pain in right toe(s)", Some("right")),
            sibling("M79.675", "Pain in left toe(s)", Some("left")),
            sibling("M79.676", "Pain in unspecified toe(s)", None),
            sibling("M79.672", "Pain in left foot", Some("left")),
            sibling("M79.673", "Pain in unspecified foot", None),
        ];
        let got = axis_only_unspecified_sibling("Pain in right foot", &siblings);
        assert_eq!(
            got,
            Some("M79.673".to_string()),
            "expected the site-matching unspecified sibling (M79.673), not the first \
             property-only match (M79.676): {got:?}"
        );
    }

    #[test]
    fn axis_only_unspecified_sibling_returns_none_when_no_sibling_differs_by_only_one_token() {
        // Neither sibling here differs from the hypothesis in exactly one
        // token: the "toe" sibling differs in both site and laterality
        // (length mismatch after tokenizing), and there is no unspecified
        // sibling for the same site at all.
        let siblings = vec![sibling("M79.674", "Pain in right toe(s)", Some("right"))];
        let got = axis_only_unspecified_sibling("Pain in right foot", &siblings);
        assert_eq!(got, None, "expected no counter-proposal: {got:?}");
    }
}
