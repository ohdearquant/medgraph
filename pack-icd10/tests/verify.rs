//! Fixture-driven integration tests for `icd10.verify`, one per verdict:
//! CONFIRMED; REVISE via unsettled laterality; REJECT via sex; REJECT via
//! status (past mention, non-history code); NEEDS_COMPANION via code_first;
//! UNSUPPORTED.
//!
//! Uses its own fixture pair (`verify_tabular.xml`/`verify_index.xml`) rather
//! than the shared `tabular.xml`/`index.xml`: the shared fixture has no
//! laterality family, sex-specific *reachable* code, or code_first-bearing
//! billable leaf, and extending it risked coupling these tests to
//! `integration.rs`'s existing assertions.

use khive_pack_icd10::Icd10Pack;
use khive_runtime::{KhiveRuntime, VerbRegistry, VerbRegistryBuilder};
use serde_json::{json, Value};

fn fixture(name: &str) -> String {
    format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"))
}

fn build_registry() -> (VerbRegistry, KhiveRuntime) {
    let runtime = KhiveRuntime::memory().expect("in-memory runtime");
    let mut builder = VerbRegistryBuilder::new();
    builder.register(khive_pack_kg::KgPack::new(runtime.clone()));
    builder.register(Icd10Pack::new(runtime.clone()));
    let registry = builder.build().expect("registry builds");
    runtime.install_edge_rules(registry.all_edge_rules());
    (registry, runtime)
}

async fn ingest(registry: &VerbRegistry) {
    registry
        .dispatch(
            "icd10.ingest",
            json!({
                "tabular_path": fixture("verify_tabular.xml"),
                "index_path": fixture("verify_index.xml"),
            }),
        )
        .await
        .expect("icd10.ingest dispatches");
}

async fn verify(registry: &VerbRegistry, args: Value) -> Value {
    registry
        .dispatch("icd10.verify", args)
        .await
        .expect("icd10.verify dispatches")
}

fn one(result: &Value) -> &Value {
    let arr = result["verdicts"].as_array().expect("verdicts array");
    assert_eq!(arr.len(), 1, "expected exactly one verdict entry: {result}");
    &arr[0]
}

// ── CONFIRMED ────────────────────────────────────────────────────────────

#[tokio::test]
async fn confirmed_bilateral_otitis_media_reachable_and_settled() {
    let (registry, _rt) = build_registry();
    ingest(&registry).await;

    let result = verify(
        &registry,
        json!({
            "hypotheses": [{
                "code": "H66.93",
                "mention": {
                    "type": "condition",
                    "name": "bilateral otitis media",
                    "quote": "bilateral ear infection noted, tympanic membranes red",
                    "status": "current",
                },
            }],
            "mentions": [],
        }),
    )
    .await;
    let entry = one(&result);
    assert_eq!(
        entry["verdict"],
        json!("CONFIRMED"),
        "expected CONFIRMED: {entry}"
    );
    assert_eq!(entry["facets"]["reachable"]["ok"], json!(true));
    assert_eq!(entry["facets"]["axis"]["settled"], json!(true));
    assert_eq!(entry["revise_to"], Value::Null);
    assert_eq!(entry["companions"], Value::Null);
}

// ── REVISE via unsettled laterality ─────────────────────────────────────

#[tokio::test]
async fn revise_unsettled_laterality_proposes_the_unspecified_sibling() {
    let (registry, _rt) = build_registry();
    ingest(&registry).await;

    // "ear infection" reaches H66.91 (right ear) through neither the mention
    // name nor the quote naming a side — the quote must settle it, and it
    // doesn't.
    let result = verify(
        &registry,
        json!({
            "hypotheses": [{
                "code": "H66.91",
                "mention": {
                    "type": "condition",
                    "name": "otitis media, right ear",
                    "quote": "ear is red and bulging, patient uncomfortable",
                    "status": "current",
                },
            }],
            "mentions": [],
        }),
    )
    .await;
    let entry = one(&result);
    assert_eq!(
        entry["verdict"],
        json!("REVISE"),
        "expected REVISE: {entry}"
    );
    assert_eq!(entry["revise_to"], json!("H66.90"));
    assert_eq!(entry["facets"]["axis"]["settled"], json!(false));
    assert_eq!(entry["facets"]["axis"]["axis"], json!("laterality"));
    let reasons = entry["reasons"].as_array().unwrap();
    assert!(
        reasons
            .iter()
            .any(|r| r["facet"] == json!("axis") && r["rule"] == json!("axis_unsettled")),
        "expected an axis_unsettled reason: {reasons:?}"
    );
}

// ── REJECT via sex ───────────────────────────────────────────────────────

#[tokio::test]
async fn reject_sex_conflict_for_a_female_only_code() {
    let (registry, _rt) = build_registry();
    ingest(&registry).await;

    let result = verify(
        &registry,
        json!({
            "hypotheses": [{
                "code": "N70",
                "mention": {
                    "type": "condition",
                    "name": "salpingitis",
                    "quote": "pelvic pain consistent with salpingitis",
                    "status": "current",
                },
            }],
            "mentions": [],
            "sex": "M",
        }),
    )
    .await;
    let entry = one(&result);
    assert_eq!(
        entry["verdict"],
        json!("REJECT"),
        "expected REJECT: {entry}"
    );
    let reasons = entry["reasons"].as_array().unwrap();
    assert!(
        reasons.iter().any(|r| r["rule"] == json!("R5 sex")),
        "expected an R5 sex reason: {reasons:?}"
    );
}

// ── REJECT via status (past mention, non-history code) ──────────────────

#[tokio::test]
async fn reject_past_status_mention_of_a_non_history_code() {
    let (registry, _rt) = build_registry();
    ingest(&registry).await;

    let result = verify(
        &registry,
        json!({
            "hypotheses": [{
                "code": "K82.4",
                "mention": {
                    "type": "condition",
                    "name": "gallbladder cholesterolosis",
                    "quote": "history of gallbladder cholesterolosis noted",
                    "status": "past",
                },
            }],
            "mentions": [],
        }),
    )
    .await;
    let entry = one(&result);
    assert_eq!(
        entry["verdict"],
        json!("REJECT"),
        "expected REJECT: {entry}"
    );
    let reasons = entry["reasons"].as_array().unwrap();
    assert!(
        reasons
            .iter()
            .any(|r| r["facet"] == json!("status") && r["rule"] == json!("status_reject")),
        "expected a status_reject reason: {reasons:?}"
    );
    assert_eq!(entry["facets"]["status"]["claimed"], json!("past"));
    let terms_found = entry["facets"]["status"]["quote_terms_found"]
        .as_array()
        .unwrap();
    assert!(
        terms_found.iter().any(|t| t == "history of"),
        "expected \"history of\" among quote_terms_found: {terms_found:?}"
    );
}

// ── NEEDS_COMPANION via code_first ───────────────────────────────────────

#[tokio::test]
async fn needs_companion_unmet_code_first() {
    let (registry, _rt) = build_registry();
    ingest(&registry).await;

    let result = verify(
        &registry,
        json!({
            "hypotheses": [{
                "code": "F02.80",
                "mention": {
                    "type": "condition",
                    "name": "dementia",
                    "quote": "progressive dementia, likely due to Alzheimer's disease",
                    "status": "current",
                },
            }],
            "mentions": [],
        }),
    )
    .await;
    let entry = one(&result);
    assert_eq!(
        entry["verdict"],
        json!("NEEDS_COMPANION"),
        "expected NEEDS_COMPANION: {entry}"
    );
    assert_eq!(entry["companions"], json!(["G30.9"]));
    let obligations = entry["facets"]["obligations"].as_array().unwrap();
    assert!(
        obligations.iter().any(|o| o["kind"] == json!("code_first")
            && o["target"] == json!("G30.9")
            && o["present"] == json!(false)),
        "expected an unmet code_first obligation naming G30.9: {obligations:?}"
    );
}

#[tokio::test]
async fn code_first_met_when_the_companion_is_also_a_hypothesis() {
    let (registry, _rt) = build_registry();
    ingest(&registry).await;

    let result = verify(
        &registry,
        json!({
            "hypotheses": [
                {
                    "code": "F02.80",
                    "mention": {
                        "type": "condition",
                        "name": "dementia",
                        "quote": "progressive dementia, likely due to Alzheimer's disease",
                        "status": "current",
                    },
                },
                {
                    "code": "G30.9",
                    "mention": {
                        "type": "condition",
                        "name": "Alzheimer's disease",
                        "quote": "confirmed Alzheimer's disease, unspecified type",
                        "status": "current",
                    },
                },
            ],
            "mentions": [],
        }),
    )
    .await;
    let arr = result["verdicts"].as_array().unwrap();
    let f02 = arr.iter().find(|v| v["code"] == json!("F02.80")).unwrap();
    assert_ne!(
        f02["verdict"],
        json!("NEEDS_COMPANION"),
        "code_first should be met once G30.9 is also hypothesized: {f02}"
    );
}

/// The fixture's code-first note sits on F02.80 and names G30.9, and G30.9's use-additional note
/// names F02.80. Each instruction belongs to the code that carries it: G30.9 alone owes no code-first
/// companion, and F02.80 alone owes no use-additional code.
#[tokio::test]
async fn instructions_belong_to_the_code_that_carries_the_note() {
    let (registry, _rt) = build_registry();
    ingest(&registry).await;

    let g30 = verify(
        &registry,
        json!({
            "hypotheses": [{
                "code": "G30.9",
                "mention": {
                    "type": "condition",
                    "name": "Alzheimer's disease",
                    "quote": "Alzheimer's disease",
                    "status": "current",
                },
            }],
            "mentions": [],
        }),
    )
    .await;
    let g30 = one(&g30);
    assert_eq!(
        g30["verdict"],
        json!("CONFIRMED"),
        "G30.9 carries no code-first note, so it needs no companion: {g30}"
    );
    let g30_obligations = g30["facets"]["obligations"].as_array().unwrap();
    assert!(
        !g30_obligations
            .iter()
            .any(|o| o["kind"] == json!("code_first")),
        "a code named by another code's code-first note has no code-first obligation: {g30_obligations:?}"
    );
    assert!(
        g30_obligations
            .iter()
            .any(|o| o["kind"] == json!("use_additional")
                && o["target"] == json!("F02.80")
                && o["present"] == json!(false)),
        "G30.9's own use-additional note must still be listed: {g30_obligations:?}"
    );

    let f02 = verify(
        &registry,
        json!({
            "hypotheses": [{
                "code": "F02.80",
                "mention": {
                    "type": "condition",
                    "name": "dementia",
                    "quote": "progressive dementia, likely due to Alzheimer's disease",
                    "status": "current",
                },
            }],
            "mentions": [],
        }),
    )
    .await;
    let f02 = one(&f02);
    let f02_obligations = f02["facets"]["obligations"].as_array().unwrap();
    assert!(
        !f02_obligations
            .iter()
            .any(|o| o["kind"] == json!("use_additional")),
        "a code named by another code's use-additional note has no use-additional obligation: {f02_obligations:?}"
    );
    assert!(
        f02_obligations
            .iter()
            .any(|o| o["kind"] == json!("code_first") && o["target"] == json!("G30.9")),
        "F02.80's own code-first note must still be listed: {f02_obligations:?}"
    );
}

// ── UNSUPPORTED ───────────────────────────────────────────────────────────

// ── axis-only-unspecified sibling ────────────────────────────────────────

#[tokio::test]
async fn revise_unsettled_laterality_picks_the_site_matching_unspecified_sibling() {
    let (registry, _rt) = build_registry();
    ingest(&registry).await;

    // M79.671 "Pain in right foot" has both an unspecified-TOE sibling
    // (M79.676, declared first in the fixture) and an unspecified-FOOT
    // sibling (M79.673). The old `find_unspecified_sibling` picked whichever
    // "differs_on: laterality, no laterality of its own, desc says
    // unspecified" sibling it met first in iteration order — M79.676 — even
    // though it also differs on site. The fix must pick M79.673: the only
    // sibling whose description differs from the hypothesis in exactly one
    // token, and that token is "unspecified".
    let result = verify(
        &registry,
        json!({
            "hypotheses": [{
                "code": "M79.671",
                "mention": {
                    "type": "condition",
                    "name": "pain in right foot",
                    "quote": "foot pain, unspecified side",
                    "status": "current",
                },
            }],
            "mentions": [],
        }),
    )
    .await;
    let entry = one(&result);
    assert_eq!(
        entry["facets"]["axis"]["settled"],
        json!(false),
        "expected the laterality axis to stay unsettled: {entry}"
    );
    assert_eq!(
        entry["facets"]["axis"]["counter_proposal"],
        json!("M79.673"),
        "expected the counter-proposal to be M79.673 (unspecified FOOT), not the unspecified-toe \
         sibling: {entry}"
    );
    assert_eq!(
        entry["verdict"],
        json!("REVISE"),
        "expected REVISE: {entry}"
    );
    assert_eq!(entry["revise_to"], json!("M79.673"));
}

// ── missing reason when axis is unsettled with no unspecified sibling ───

#[tokio::test]
async fn axis_unsettled_with_no_unspecified_sibling_still_reports_a_reason() {
    let (registry, _rt) = build_registry();
    ingest(&registry).await;

    // M54.59 "Other back pain" has one sibling, M54.50 "Low back pain" —
    // neither reads as "unspecified", so there is no counter-proposal. The
    // axis rule still fired (it left the axis unsettled) and every rule that
    // fires must be listed, even when it doesn't change the verdict.
    let result = verify(
        &registry,
        json!({
            "hypotheses": [{
                "code": "M54.59",
                "mention": {
                    "type": "condition",
                    "name": "other back pain",
                    "quote": "patient reports back discomfort",
                    "status": "current",
                },
            }],
            "mentions": [],
        }),
    )
    .await;
    let entry = one(&result);
    assert_eq!(entry["facets"]["axis"]["settled"], json!(false));
    assert_eq!(entry["facets"]["axis"]["counter_proposal"], Value::Null);
    let reasons = entry["reasons"].as_array().unwrap();
    assert!(
        reasons.iter().any(|r| r["facet"] == json!("axis")
            && r["rule"] == json!("axis_unsettled_no_unspecified_sibling")),
        "expected an axis_unsettled_no_unspecified_sibling reason: {reasons:?}"
    );
}

// ── nonessential modifiers + Index default ──────────────────────────────

#[tokio::test]
async fn axis_settles_via_index_default_or_a_stripped_nonessential_modifier() {
    let (registry, _rt) = build_registry();
    ingest(&registry).await;

    let result = verify(
        &registry,
        json!({
            "hypotheses": [
                {
                    // K21.9 is reached only via a BARE Index main term
                    // ("GERD (gastroesophageal reflux disease)", no nested
                    // <term>) — a reaching path with no subterm below its
                    // main term is the Index default and carries no
                    // specificity requirement, so it auto-settles.
                    "code": "K21.9",
                    "mention": {
                        "type": "condition",
                        "name": "GERD",
                        "quote": "reflux disease noted, no esophagitis on endoscopy",
                        "status": "current",
                    },
                },
                {
                    // M54.50 is reached via a subterm carrying an inline
                    // parenthesized modifier, "low (postural)". The
                    // modifier must be stripped before matching AND before
                    // being reported in terms_needed — the quote below
                    // carries "low" but never "postural".
                    "code": "M54.50",
                    "mention": {
                        "type": "condition",
                        "name": "low back pain",
                        "quote": "patient reports low back pain that worsens when standing",
                        "status": "current",
                    },
                },
            ],
            "mentions": [],
        }),
    )
    .await;
    let arr = result["verdicts"].as_array().unwrap();

    let k21 = arr.iter().find(|v| v["code"] == json!("K21.9")).unwrap();
    assert_eq!(
        k21["facets"]["axis"]["settled"],
        json!(true),
        "expected the bare-main-term (Index default) path to auto-settle: {k21}"
    );
    assert_eq!(
        k21["facets"]["axis"]["settled_by"],
        json!("index_default"),
        "{k21}"
    );

    let m54 = arr.iter().find(|v| v["code"] == json!("M54.50")).unwrap();
    assert_eq!(
        m54["facets"]["axis"]["settled"],
        json!(true),
        "expected the subterm, stripped of its parenthesized modifier, to settle from \"low\" \
         in the quote: {m54}"
    );
    assert_eq!(
        m54["facets"]["axis"]["settled_by"],
        json!("quote_match"),
        "{m54}"
    );
    let terms_needed = m54["facets"]["axis"]["terms_needed"].as_array().unwrap();
    assert!(
        terms_needed.iter().any(|t| t == "low"),
        "expected the stripped form \"low\" in terms_needed: {terms_needed:?}"
    );
    assert!(
        terms_needed
            .iter()
            .all(|t| !t.as_str().unwrap_or("").contains('(')),
        "terms_needed must not carry the parenthesized modifier: {terms_needed:?}"
    );
}

// ── path/edge node labels ─────────────────────────────────────────────────

#[tokio::test]
async fn reachable_icd_code_node_label_is_the_description_not_the_bare_code() {
    let (registry, _rt) = build_registry();
    ingest(&registry).await;

    let result = verify(
        &registry,
        json!({
            "hypotheses": [{
                "code": "K82.4",
                "mention": {
                    "type": "condition",
                    "name": "gallbladder cholesterolosis",
                    "quote": "ultrasound consistent with gallbladder cholesterolosis",
                    "status": "current",
                },
            }],
            "mentions": [],
        }),
    )
    .await;
    let entry = one(&result);
    let paths = entry["facets"]["reachable"]["paths"].as_array().unwrap();
    let icd_code_labels: Vec<&Value> = paths
        .iter()
        .filter_map(|p| p.as_array().and_then(|steps| steps.first()))
        .filter(|step| step["node"]["kind"] == json!("icd_code"))
        .map(|step| &step["node"]["label"])
        .collect();
    assert!(
        !icd_code_labels.is_empty(),
        "expected at least one icd_code path step: {paths:?}"
    );
    for label in icd_code_labels {
        assert_eq!(
            label,
            &json!("Cholesterolosis of gallbladder"),
            "icd_code node label must be the description, not the bare code: {paths:?}"
        );
    }
}

// ── family is a status, not a type ───────────────────────────────────────

#[tokio::test]
async fn family_status_mention_rejects_unless_the_code_is_itself_family_history() {
    let (registry, _rt) = build_registry();
    ingest(&registry).await;

    let result = verify(
        &registry,
        json!({
            "hypotheses": [
                {
                    "code": "G30.9",
                    "mention": {
                        "type": "condition",
                        "name": "family history of Alzheimer's disease",
                        "quote": "mother had Alzheimer's disease",
                        "status": "family",
                    },
                },
                {
                    "code": "Z80.3",
                    "mention": {
                        "type": "condition",
                        "name": "family history of malignant neoplasm of breast",
                        "quote": "family history of malignant neoplasm of breast documented",
                        "status": "family",
                    },
                },
            ],
            "mentions": [],
        }),
    )
    .await;
    let arr = result["verdicts"].as_array().unwrap();

    let g30 = arr.iter().find(|v| v["code"] == json!("G30.9")).unwrap();
    assert_eq!(
        g30["verdict"],
        json!("REJECT"),
        "family-status mention of a non-family-history code must REJECT: {g30}"
    );
    let g30_reasons = g30["reasons"].as_array().unwrap();
    assert!(
        g30_reasons
            .iter()
            .any(|r| r["facet"] == json!("status") && r["rule"] == json!("status_reject")),
        "expected a status_reject reason: {g30_reasons:?}"
    );

    let z80 = arr.iter().find(|v| v["code"] == json!("Z80.3")).unwrap();
    let z80_reasons = z80["reasons"].as_array().unwrap();
    assert!(
        !z80_reasons
            .iter()
            .any(|r| r["facet"] == json!("status") && r["rule"] == json!("status_reject")),
        "a family-status mention of a Z80-Z84 family-history code must not carry a status_reject \
         reason: {z80_reasons:?}"
    );
}

// ── status_inconsistent reason ─────────────────────────────────────────────

#[tokio::test]
async fn status_inconsistent_with_the_quote_is_reported_as_a_reason() {
    let (registry, _rt) = build_registry();
    ingest(&registry).await;

    let result = verify(
        &registry,
        json!({
            "hypotheses": [{
                "code": "K82.4",
                "mention": {
                    "type": "condition",
                    "name": "gallbladder cholesterolosis",
                    "quote": "history of gallbladder cholesterolosis, currently asymptomatic",
                    "status": "current",
                },
            }],
            "mentions": [],
        }),
    )
    .await;
    let entry = one(&result);
    assert_eq!(entry["facets"]["status"]["consistent"], json!(false));
    let reasons = entry["reasons"].as_array().unwrap();
    assert!(
        reasons
            .iter()
            .any(|r| r["facet"] == json!("status") && r["rule"] == json!("status_inconsistent")),
        "expected a status_inconsistent reason: {reasons:?}"
    );
    assert!(
        !reasons
            .iter()
            .any(|r| r["facet"] == json!("status") && r["rule"] == json!("status_reject")),
        "a claimed-current mention must not itself be status_reject-ed by the inconsistency: \
         {reasons:?}"
    );
}

#[tokio::test]
async fn unsupported_when_nothing_in_the_graph_reaches_the_code() {
    let (registry, _rt) = build_registry();
    ingest(&registry).await;

    let result = verify(
        &registry,
        json!({
            "hypotheses": [{
                "code": "H66.92",
                "mention": {
                    "type": "condition",
                    "name": "unrelated finding of no clinical significance",
                    "quote": "patient reports an unrelated finding of no clinical significance",
                    "status": "current",
                },
            }],
            "mentions": [],
        }),
    )
    .await;
    let entry = one(&result);
    assert_eq!(
        entry["verdict"],
        json!("UNSUPPORTED"),
        "expected UNSUPPORTED: {entry}"
    );
    assert_eq!(entry["facets"]["reachable"]["ok"], json!(false));
}

// ── guideline / support / coherence ─────────────────────────────────────
//
// Uses this file's own `verify_tabular.xml`/`verify_index.xml` pair (I10/
// I12.9/N18.9/E11.9/E66.9/Z68.30) plus a dedicated
// `icd10.ingest_external` fixture set: `verify_rules.jsonl` (one
// `combination` rule I10+N18 -> I12, one `reporting_condition` rule Z68
// needs E66), `verify_medrt.jsonl` (metformin -[may_treat]-> type 2 diabetes
// mellitus), and the shared `doid_mg.obo` (DOID:9352 type 2 diabetes
// mellitus, xref ICD10CM:E11 and MESH:D003924 — the same mesh id
// `verify_medrt.jsonl` uses, so the medication arm's drug -> mesh_disease ->
// icd category walk resolves without a bespoke `.obo` file).

async fn ingest_external_phase2(registry: &VerbRegistry) {
    registry
        .dispatch(
            "icd10.ingest_external",
            json!({
                "medrt_jsonl": fixture("verify_medrt.jsonl"),
                "doid_obo": fixture("doid_mg.obo"),
                "rules_jsonl": fixture("verify_rules.jsonl"),
            }),
        )
        .await
        .expect("icd10.ingest_external dispatches");
}

#[tokio::test]
async fn guideline_combination_rule_proposes_a_recode_without_changing_the_verdict() {
    let (registry, _rt) = build_registry();
    ingest(&registry).await;
    ingest_external_phase2(&registry).await;

    let hypertension = || {
        json!({
            "code": "I10",
            "mention": {
                "type": "condition",
                "name": "hypertension",
                "quote": "essential primary hypertension, blood pressure elevated",
                "status": "current",
            },
        })
    };

    let result = verify(
        &registry,
        json!({
            "hypotheses": [
                hypertension(),
                {
                    "code": "N18.9",
                    "mention": {
                        "type": "condition",
                        "name": "chronic kidney disease",
                        "quote": "chronic kidney disease, unspecified stage",
                        "status": "current",
                    },
                },
            ],
            "mentions": [],
        }),
    )
    .await;
    let arr = result["verdicts"].as_array().expect("verdicts array");
    assert_eq!(arr.len(), 2, "expected two verdict entries: {result}");
    let i10 = arr
        .iter()
        .find(|v| v["code"] == json!("I10"))
        .expect("I10 entry");
    assert_eq!(
        i10["verdict"],
        json!("CONFIRMED"),
        "a combination rule's premises being present must never change the verdict: {i10}"
    );
    let proposals = i10["proposals"].as_array().expect("proposals array");
    assert!(
        proposals
            .iter()
            .any(|p| p["to"] == json!("I12") && p["rule_id"] == json!("I.C.9.a.2-ckd")),
        "expected an I12 combination proposal on I10 once N18.9 is also hypothesized: {proposals:?}"
    );

    // MUST-FAIL CONTROL: with only the I10 hypothesis (no N18.9), the
    // combination rule's premises are not all present, so I10 must carry no
    // proposal — proving the proposal above is driven by the SECOND
    // hypothesis actually being present, not emitted unconditionally.
    let solo = verify(
        &registry,
        json!({ "hypotheses": [hypertension()], "mentions": [] }),
    )
    .await;
    let solo_entry = one(&solo);
    assert_eq!(
        solo_entry["verdict"],
        json!("CONFIRMED"),
        "I10 alone is still directly reachable and confirmed: {solo_entry}"
    );
    assert_eq!(
        solo_entry["proposals"]
            .as_array()
            .expect("proposals array")
            .len(),
        0,
        "expected no proposal when the N18 premise is absent: {solo_entry}"
    );
}

#[tokio::test]
async fn guideline_reporting_condition_rejects_a_bmi_code_without_its_companion() {
    let (registry, _rt) = build_registry();
    ingest(&registry).await;
    ingest_external_phase2(&registry).await;

    let bmi = || {
        json!({
            "code": "Z68.30",
            "mention": {
                "type": "condition",
                "name": "BMI",
                "quote": "body mass index 30.0-30.9, adult",
                "status": "current",
            },
        })
    };

    let result = verify(&registry, json!({ "hypotheses": [bmi()], "mentions": [] })).await;
    let entry = one(&result);
    assert_eq!(
        entry["verdict"],
        json!("REJECT"),
        "expected REJECT: a BMI code without an associated reportable diagnosis (E66) is \
         unmet per I.C.21.c.3: {entry}"
    );
    let reasons = entry["reasons"].as_array().expect("reasons array");
    assert!(
        reasons
            .iter()
            .any(|r| r["rule"] == json!("reporting_condition_unmet")
                && r["rule_id"] == json!("I.C.21.c.3-bmi")),
        "expected a reporting_condition_unmet reason citing I.C.21.c.3-bmi: {reasons:?}"
    );

    // MUST-FAIL CONTROL: with the E66.9 companion also hypothesized, the
    // SAME Z68.30 hypothesis must NOT carry this reason or REJECT on it.
    let with_companion = verify(
        &registry,
        json!({
            "hypotheses": [
                bmi(),
                {
                    "code": "E66.9",
                    "mention": {
                        "type": "condition",
                        "name": "obesity",
                        "quote": "obesity, unspecified",
                        "status": "current",
                    },
                },
            ],
            "mentions": [],
        }),
    )
    .await;
    let arr = with_companion["verdicts"]
        .as_array()
        .expect("verdicts array");
    let z68 = arr
        .iter()
        .find(|v| v["code"] == json!("Z68.30"))
        .expect("Z68.30 entry");
    let reasons = z68["reasons"].as_array().expect("reasons array");
    assert!(
        !reasons
            .iter()
            .any(|r| r["rule"] == json!("reporting_condition_unmet")),
        "expected no reporting_condition_unmet reason once E66.9 is present: {z68}"
    );
}

#[tokio::test]
async fn medication_only_evidence_stays_unsupported_and_is_tagged_support_only() {
    let (registry, _rt) = build_registry();
    ingest(&registry).await;
    ingest_external_phase2(&registry).await;

    let diabetes_hypothesis_no_condition_evidence = || {
        json!({
            "code": "E11.9",
            "mention": {
                "type": "condition",
                "name": "unrelated finding of no clinical significance",
                "quote": "patient reports an unrelated finding of no clinical significance",
                "status": "current",
            },
        })
    };

    let result = verify(
        &registry,
        json!({
            "hypotheses": [diabetes_hypothesis_no_condition_evidence()],
            "mentions": [{
                "type": "medication",
                "name": "metformin",
                "quote": "metformin 500mg twice daily",
                "status": "current",
            }],
        }),
    )
    .await;
    let entry = one(&result);
    assert_eq!(
        entry["verdict"],
        json!("UNSUPPORTED"),
        "medication-path evidence alone must never confirm: {entry}"
    );
    assert_eq!(
        entry["support_only"],
        json!(true),
        "expected support_only: true when medication evidence is the only thing reaching the \
         code: {entry}"
    );
    let support = entry["facets"]["support"]
        .as_array()
        .expect("support array");
    assert!(
        !support.is_empty(),
        "expected a non-empty support facet (metformin -> mesh -> E11 -> E11.9): {entry}"
    );
    let reasons = entry["reasons"].as_array().expect("reasons array");
    assert!(
        reasons.iter().any(|r| r["rule"] == json!("support_only")),
        "expected a support_only reason: {reasons:?}"
    );

    // MUST-FAIL CONTROL: the identical hypothesis with NO medication mention
    // in the visit must stay UNSUPPORTED but with `support_only: false` and
    // an empty `support` facet — proving the tag above is driven by the
    // medication mention actually being present, not emitted unconditionally.
    let no_medication = verify(
        &registry,
        json!({
            "hypotheses": [diabetes_hypothesis_no_condition_evidence()],
            "mentions": [],
        }),
    )
    .await;
    let no_med_entry = one(&no_medication);
    assert_eq!(no_med_entry["verdict"], json!("UNSUPPORTED"));
    assert_eq!(
        no_med_entry["support_only"],
        json!(false),
        "expected support_only: false with no medication evidence: {no_med_entry}"
    );
    assert_eq!(
        no_med_entry["facets"]["support"]
            .as_array()
            .expect("support array")
            .len(),
        0,
        "expected an empty support facet with no medication mention: {no_med_entry}"
    );
}

fn supports_edges(entry: &Value) -> Vec<Value> {
    entry["edges"]
        .as_array()
        .expect("edges array")
        .iter()
        .filter(|e| e["rel"] == json!("supports"))
        .cloned()
        .collect()
}

#[tokio::test]
async fn medication_support_is_drawn_as_a_supports_edge_without_changing_the_verdict() {
    let (registry, _rt) = build_registry();
    ingest(&registry).await;
    ingest_external_phase2(&registry).await;

    let metformin = json!({
        "type": "medication",
        "name": "metformin",
        "quote": "metformin 500mg twice daily",
        "status": "current",
    });
    // Two hypotheses for the same code: one the mention never reaches (the
    // support-only UNSUPPORTED case) and one the mention reaches through the
    // Tabular text, so the edge is shown on more than one verdict.
    let support_only = json!({
        "code": "E11.9",
        "mention": {
            "type": "condition",
            "name": "unrelated finding of no clinical significance",
            "quote": "patient reports an unrelated finding of no clinical significance",
            "status": "current",
        },
    });
    let reachable = json!({
        "code": "E11.9",
        "mention": {
            "type": "condition",
            "name": "type 2 diabetes mellitus",
            "quote": "type 2 diabetes mellitus without complications",
            "status": "current",
        },
    });

    for (label, hypothesis) in [("support_only", &support_only), ("reachable", &reachable)] {
        let with_med = verify(
            &registry,
            json!({ "hypotheses": [hypothesis], "mentions": [metformin.clone()] }),
        )
        .await;
        let with_med = one(&with_med);
        let without_med = verify(
            &registry,
            json!({ "hypotheses": [hypothesis], "mentions": [] }),
        )
        .await;
        let without_med = one(&without_med);

        assert_eq!(
            with_med["verdict"], without_med["verdict"],
            "{label}: medication support must never change the verdict: \
             with={with_med} without={without_med}"
        );

        let support = with_med["facets"]["support"]
            .as_array()
            .expect("support array");
        assert!(
            !support.is_empty(),
            "{label}: expected a medication path: {with_med}"
        );
        let edges = supports_edges(with_med);
        assert_eq!(
            edges.len(),
            support.len(),
            "{label}: expected one supports edge per medication path: {with_med}"
        );
        for e in &edges {
            assert_eq!(e["src"], json!("mention:metformin"), "{label}: {e}");
            assert_eq!(e["dst"], json!("icd_code:E11.9"), "{label}: {e}");
            assert_eq!(e["icd"], json!("may_treat"), "{label}: {e}");
            assert_eq!(e["props"]["via"], json!("medication"), "{label}: {e}");
        }

        // Control: the same hypothesis with no medication mention carries no
        // supports edge, so the edge above comes from the medication path.
        assert!(
            supports_edges(without_med).is_empty(),
            "{label}: expected no supports edge without a medication mention: {without_med}"
        );

        if label == "support_only" {
            assert_eq!(with_med["verdict"], json!("UNSUPPORTED"), "{with_med}");
        } else {
            assert_eq!(
                with_med["facets"]["reachable"]["ok"],
                json!(true),
                "the Tabular text should reach E11.9 from this mention: {with_med}"
            );
            assert_ne!(with_med["verdict"], json!("UNSUPPORTED"), "{with_med}");
        }
    }
}
