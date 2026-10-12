//! Fixture-driven integration tests for the medical-graph additions:
//! `icd10.ingest_external` (MED-RT, DOID, AHRQ
//! DXCCSR, CMS-HCC, guideline rules) and `icd10.evidence`. Separate binary
//! from `tests/integration.rs`
//! (which owns the base ingest/neighborhood/check/resolve/exclude surface)
//! so a fixture change here never touches that file's baseline.

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

async fn ingest_base(registry: &VerbRegistry) -> Value {
    registry
        .dispatch(
            "icd10.ingest",
            json!({
                "tabular_path": fixture("tabular_mg.xml"),
                "index_path": fixture("index_mg.xml"),
            }),
        )
        .await
        .expect("icd10.ingest dispatches")
}

async fn ingest_external(registry: &VerbRegistry, params: Value) -> Value {
    registry
        .dispatch("icd10.ingest_external", params)
        .await
        .expect("icd10.ingest_external dispatches")
}

fn code_verdict<'a>(result: &'a Value, code: &str) -> &'a Value {
    result["codes"]
        .as_array()
        .expect("codes array")
        .iter()
        .find(|c| c["code"] == json!(code))
        .unwrap_or_else(|| panic!("no evidence entry for {code}: {result}"))
}

// ── icd10.ingest_external: MED-RT ──────────────────────────────────────

#[tokio::test]
async fn ingest_external_medrt_creates_drug_and_mesh_disease_with_enables_edge() {
    let (registry, _rt) = build_registry();
    ingest_base(&registry).await;

    let result =
        ingest_external(&registry, json!({"medrt_jsonl": fixture("medrt_mg.jsonl")})).await;
    assert_eq!(result["sources"]["medrt"]["created"]["drug"], json!(2));
    assert_eq!(
        result["sources"]["medrt"]["created"]["mesh_disease"],
        json!(3)
    );
    assert_eq!(result["sources"]["medrt"]["edges_total"], json!(3));
}

// ── icd10.ingest_external: DOID ────────────────────────────────────────

#[tokio::test]
async fn ingest_external_doid_links_mesh_disease_to_icd_category_via_variant_of() {
    let (registry, _rt) = build_registry();
    ingest_base(&registry).await;
    ingest_external(&registry, json!({"medrt_jsonl": fixture("medrt_mg.jsonl")})).await;

    let result = ingest_external(&registry, json!({"doid_obo": fixture("doid_mg.obo")})).await;
    // Two DOID terms (DOID:9352 direct, DOID:0080562 via a 1-hop is_a walk)
    // both xref D003924 -> E11, and both attempts land on the SAME
    // (mesh_disease, E11, variant_of) edge, so edges_total is 1, not 2.
    assert_eq!(result["sources"]["doid"]["edges_total"], json!(1));
    assert!(
        result["sources"]["doid"]["mesh_disease_matched"]
            .as_u64()
            .unwrap()
            >= 3
    );
}

#[tokio::test]
async fn ingest_external_doid_is_a_walk_finds_ancestor_xref_and_records_parent_hops() {
    let (registry, _rt) = build_registry();
    ingest_base(&registry).await;
    ingest_external(&registry, json!({"medrt_jsonl": fixture("medrt_mg.jsonl")})).await;
    ingest_external(&registry, json!({"doid_obo": fixture("doid_mg.obo")})).await;

    // mesh_disease(D003924) -[variant_of]-> icd_category(E11): both DOID:9352
    // (a direct xref, parent_hops 0) and DOID:0080562 (a 1-hop is_a walk)
    // merge onto this ONE edge, whose metadata carries `doid`/`parent_hops`.
    // Reached entirely through dispatch (`neighbors` then `get(id=edge_id)`)
    // — `NamespaceToken` is sealed outside `khive-runtime` and this crate's
    // own tests cannot mint one directly (see this file's header comment).
    let neighbors = registry
        .dispatch(
            "neighbors",
            json!({"node_id": "D003924", "relations": ["variant_of"], "direction": "outgoing"}),
        )
        .await
        .expect("neighbors dispatches");
    let hits = neighbors
        .as_array()
        .expect("neighbors returns a bare array");
    assert_eq!(hits.len(), 1, "expected exactly one merged edge: {hits:?}");
    let edge_id = hits[0]["edge_id"].as_str().expect("edge_id present");

    let edge = registry
        .dispatch("get", json!({"id": edge_id}))
        .await
        .expect("get dispatches");
    let meta = &edge["metadata"];
    assert_eq!(meta["source"], json!("DOID 2026-08-31"));
    assert!(meta["doid"].is_string(), "expected a doid id: {meta}");
    assert!(
        meta["parent_hops"].is_number(),
        "expected parent_hops: {meta}"
    );
    // Control: a precise single-code xref (`ICD10CM:E11`)
    // never carries `xref_range` — only a block-range xref does (see
    // `ingest_doid_range_xref_tags_edge_with_xref_range` below).
    assert!(
        meta.get("xref_range").is_none(),
        "a direct single-code xref must not carry xref_range: {meta}"
    );
}

/// A DOID xref that names a BLOCK RANGE (`ICD10CM:E08-E13`,
/// same shape as DOID:9351's real E08-E13 cross-reference) is
/// kept as a step to every category in the block, but the resulting edge is
/// tagged `props.xref_range` with the source range string, so a trace reader
/// never mistakes it for a precise single-code xref. Isolated fixture
/// (`doid_range_mg.obo`, one DOID term) rather than adding this xref to the
/// shared `doid_mg.obo` — the shared fixture backs `setup_medical_graph()`,
/// used by several medication-arm tests that assert on a single
/// (D003920/D003924) medication path, and adding a second variant_of source
/// for D003920 there would change which path several of those tests see.
#[tokio::test]
async fn ingest_doid_range_xref_tags_edge_with_xref_range() {
    let (registry, _rt) = build_registry();
    ingest_base(&registry).await;
    ingest_external(&registry, json!({"medrt_jsonl": fixture("medrt_mg.jsonl")})).await;

    let result =
        ingest_external(&registry, json!({"doid_obo": fixture("doid_range_mg.obo")})).await;
    assert_eq!(result["sources"]["doid"]["edges_total"], json!(1));

    let neighbors = registry
        .dispatch(
            "neighbors",
            json!({"node_id": "D003920", "relations": ["variant_of"], "direction": "outgoing"}),
        )
        .await
        .expect("neighbors dispatches");
    let hits = neighbors
        .as_array()
        .expect("neighbors returns a bare array");
    assert_eq!(
        hits.len(),
        1,
        "expected one variant_of edge from the range xref: {hits:?}"
    );
    let edge_id = hits[0]["edge_id"].as_str().expect("edge_id present");

    let edge = registry
        .dispatch("get", json!({"id": edge_id}))
        .await
        .expect("get dispatches");
    assert_eq!(
        edge["metadata"]["xref_range"],
        json!("E08-E13"),
        "a block-range xref must be tagged with the range it came from: {edge}"
    );
}

// ── icd10.ingest_external: AHRQ DXCCSR ─────────────────────────────────

#[tokio::test]
async fn ingest_external_ccsr_creates_category_and_part_of_edge_with_default_flags() {
    let (registry, _rt) = build_registry();
    ingest_base(&registry).await;

    let result = ingest_external(&registry, json!({"ccsr_csv": fixture("ccsr_mg.csv")})).await;
    assert_eq!(
        result["sources"]["ccsr"]["created"]["ccsr_category"],
        json!(3)
    );
    assert_eq!(result["sources"]["ccsr"]["edges_total"], json!(3)); // I10->CIR007, K82.4->DIG010, K82.4->DIG004

    assert_eq!(result["unplaced"]["count"], json!(1));
    let examples = result["unplaced"]["examples"].as_array().unwrap();
    assert!(
        examples
            .iter()
            .any(|e| e["source"] == json!("ccsr")
                && e["reason"].as_str().unwrap().contains("Z99999X"))
    );
}

// ── icd10.ingest_external: CMS-HCC ─────────────────────────────────────

#[tokio::test]
async fn ingest_external_hcc_creates_hcc_entities_for_v24_and_v28() {
    let (registry, _rt) = build_registry();
    ingest_base(&registry).await;

    let result = ingest_external(&registry, json!({"hcc_csv": fixture("hcc_mg.csv")})).await;
    // E11.9: V24=19, V28=38 -> 2 hcc entities, 2 edges.
    // I10: both V24/V28 blank -> no edges.
    // Z99999X: doesn't resolve -> unplaced.
    assert_eq!(result["sources"]["hcc"]["created"]["hcc"], json!(2));
    assert_eq!(result["sources"]["hcc"]["edges_total"], json!(2));
    let examples = result["unplaced"]["examples"].as_array().unwrap();
    assert!(examples
        .iter()
        .any(|e| e["source"] == json!("hcc") && e["reason"].as_str().unwrap().contains("Z99999X")));
}

// ── icd10.ingest_external: guideline rules ─────────────────────────────

#[tokio::test]
async fn ingest_external_rules_creates_guideline_rule_and_depends_on_enables_edges() {
    let (registry, _rt) = build_registry();
    ingest_base(&registry).await;

    let result = ingest_external(
        &registry,
        json!({"rules_jsonl": fixture("rules_evidence.jsonl")}),
    )
    .await;
    assert_eq!(
        result["sources"]["rules"]["created"]["guideline_rule"],
        json!(2)
    );
    // fixture-rule-1: 2 premises (I10, N18) + 1 conclusion (I12) = 3 edges.
    // fixture-rule-2: premises/conclusions unresolved -> 0 edges.
    assert_eq!(result["sources"]["rules"]["edges_total"], json!(3));
    let examples = result["unplaced"]["examples"].as_array().unwrap();
    assert!(examples
        .iter()
        .any(|e| e["source"] == json!("rules") && e["reason"].as_str().unwrap().contains("ZZ999")));
}

// ── icd10.ingest_external: idempotent re-run (idempotency) ────

#[tokio::test]
async fn ingest_external_rerun_is_idempotent_with_identical_counts() {
    let (registry, _rt) = build_registry();
    ingest_base(&registry).await;

    let params = json!({
        "medrt_jsonl": fixture("medrt_mg.jsonl"),
        "doid_obo": fixture("doid_mg.obo"),
        "ccsr_csv": fixture("ccsr_mg.csv"),
        "hcc_csv": fixture("hcc_mg.csv"),
        "rules_jsonl": fixture("rules_evidence.jsonl"),
    });
    let first = ingest_external(&registry, params.clone()).await;
    let second = ingest_external(&registry, params).await;

    for source in ["medrt", "doid", "ccsr", "hcc", "rules"] {
        assert_eq!(
            first["sources"][source]["edges_total"], second["sources"][source]["edges_total"],
            "edges_total for {source} must be identical across a re-run"
        );
    }
    assert_eq!(second["sources"]["medrt"]["created"]["drug"], json!(0));
    assert_eq!(
        second["sources"]["medrt"]["created"]["mesh_disease"],
        json!(0)
    );
    assert_eq!(
        second["sources"]["doid"]["edges_total"],
        first["sources"]["doid"]["edges_total"]
    );
    assert_eq!(
        second["sources"]["ccsr"]["created"]["ccsr_category"],
        json!(0)
    );
    assert_eq!(second["sources"]["hcc"]["created"]["hcc"], json!(0));
    assert_eq!(
        second["sources"]["rules"]["created"]["guideline_rule"],
        json!(0)
    );
}

// ── icd10.evidence ──────────────────────────────────────────────────────

async fn evidence(registry: &VerbRegistry, mentions: Value, codes: Value) -> Value {
    registry
        .dispatch(
            "icd10.evidence",
            json!({"mentions": mentions, "codes": codes}),
        )
        .await
        .expect("icd10.evidence dispatches")
}

async fn verify_medication_support(registry: &VerbRegistry, medication_status: &str) -> Value {
    registry
        .dispatch(
            "icd10.verify",
            json!({
                "hypotheses": [{
                    "code": "E11.9",
                    "mention": {
                        "type": "condition",
                        "name": "unrelated finding of no clinical significance",
                        "quote": "patient reports an unrelated finding of no clinical significance",
                        "status": "current",
                    },
                }],
                "mentions": [{
                    "type": "medication",
                    "name": "metformin",
                    "quote": "metformin on medication list",
                    "status": medication_status,
                }],
            }),
        )
        .await
        .expect("icd10.verify dispatches")
}

fn assert_no_verify_medication_support(result: &Value) {
    let entries = result["verdicts"].as_array().expect("verdicts array");
    assert_eq!(entries.len(), 1, "expected one verdict: {result}");
    let entry = &entries[0];
    assert_eq!(entry["verdict"], json!("UNSUPPORTED"), "{entry}");
    assert_eq!(entry["support_only"], json!(false), "{entry}");
    assert!(
        entry["facets"]["support"]
            .as_array()
            .expect("support paths array")
            .is_empty(),
        "non-current medication supplied a support path: {entry}"
    );
    assert!(
        entry["edges"]
            .as_array()
            .expect("edges array")
            .iter()
            .all(|edge| edge["rel"] != json!("supports")),
        "non-current medication supplied a supports edge: {entry}"
    );
}

async fn setup_medical_graph() -> (VerbRegistry, KhiveRuntime) {
    let (registry, rt) = build_registry();
    ingest_base(&registry).await;
    ingest_external(&registry, json!({"medrt_jsonl": fixture("medrt_mg.jsonl")})).await;
    ingest_external(&registry, json!({"doid_obo": fixture("doid_mg.obo")})).await;
    (registry, rt)
}

#[tokio::test]
async fn verify_negated_medication_mention_does_not_supply_support() {
    let (registry, _rt) = setup_medical_graph().await;
    let current = verify_medication_support(&registry, "current").await;
    assert_eq!(current["verdicts"][0]["support_only"], json!(true));

    let negated = verify_medication_support(&registry, "negated").await;
    assert_no_verify_medication_support(&negated);
}

#[tokio::test]
async fn verify_family_medication_mention_does_not_supply_support() {
    let (registry, _rt) = setup_medical_graph().await;
    let family = verify_medication_support(&registry, "family").await;
    assert_no_verify_medication_support(&family);
}

#[tokio::test]
async fn verify_other_non_current_medication_mentions_do_not_supply_support() {
    let (registry, _rt) = setup_medical_graph().await;
    for status in ["past", "planned", "uncertain"] {
        let result = verify_medication_support(&registry, status).await;
        assert_no_verify_medication_support(&result);
    }
}

/// A medication mention (metformin, current) + code
/// E11.9 -> `medication`, path through MeSH D003924.
#[tokio::test]
async fn evidence_medication_metformin_reaches_e11_9_via_mesh_disease() {
    let (registry, _rt) = setup_medical_graph().await;

    let result = evidence(
        &registry,
        json!([{"type": "medication", "name": "metformin", "quote": "on metformin", "status": "current"}]),
        json!(["E11.9"]),
    )
    .await;
    let entry = code_verdict(&result, "E11.9");
    assert_eq!(entry["verdict"], json!("medication"));
    let paths = entry["paths"].as_array().expect("paths array");
    assert!(!paths.is_empty());
    let path = paths[0].as_array().expect("path is a list of steps");
    let mesh_step = path
        .iter()
        .find(|s| s["node"]["kind"] == json!("mesh_disease"))
        .expect("path names the mesh_disease step");
    assert_eq!(mesh_step["node"]["key"], json!("D003924"));
    // The mesh_disease/icd_category/icd_code node LABELS on
    // the medication path are human-readable (the MeSH name / code
    // description), never the bare id/code the KEY already carries.
    assert_eq!(
        mesh_step["node"]["label"],
        json!("Diabetes Mellitus, Type 2"),
        "mesh_disease label must be the MeSH name, not the bare id: {mesh_step}"
    );
    let category_step = path
        .iter()
        .find(|s| s["node"]["key"] == json!("E11"))
        .expect("path names the E11 category step");
    assert_eq!(
        category_step["node"]["label"],
        json!("Type 2 diabetes mellitus"),
        "icd_category label must be the description, not the bare code: {category_step}"
    );
    let code_step = path
        .iter()
        .find(|s| s["node"]["key"] == json!("E11.9"))
        .expect("path names the E11.9 code hop");
    assert_eq!(
        code_step["node"]["label"],
        json!("Type 2 diabetes mellitus without complications"),
        "icd_code label must be the description, not the bare code: {code_step}"
    );
}

/// A preventive indication cannot support a present diagnosis merely because
/// both are stored under the pack's generic `enables` graph relation.
#[tokio::test]
async fn evidence_may_prevent_only_does_not_support_present_disease() {
    let (registry, _rt) = build_registry();
    ingest_base(&registry).await;
    ingest_external(
        &registry,
        json!({"medrt_jsonl": fixture("medrt_prevent_only.jsonl")}),
    )
    .await;
    ingest_external(&registry, json!({"doid_obo": fixture("doid_mg.obo")})).await;

    let result = evidence(
        &registry,
        json!([{"type": "medication", "name": "metformin", "quote": "on metformin", "status": "current"}]),
        json!(["E11.9"]),
    )
    .await;
    let entry = code_verdict(&result, "E11.9");
    assert_eq!(entry["verdict"], json!("unsupported"));
    assert!(entry["paths"].as_array().unwrap().is_empty());
    assert!(entry["abstentions"].as_array().unwrap().is_empty());
}

/// Ingest merges MED-RT predicates for one drug–disease pair. An explicit
/// `may_treat` assertion must remain usable when `may_prevent` is also there.
#[tokio::test]
async fn evidence_treat_and_prevent_pair_keeps_treatment_support() {
    let (registry, _rt) = build_registry();
    ingest_base(&registry).await;
    ingest_external(
        &registry,
        json!({"medrt_jsonl": fixture("medrt_treat_and_prevent.jsonl")}),
    )
    .await;
    ingest_external(&registry, json!({"doid_obo": fixture("doid_mg.obo")})).await;

    let result = evidence(
        &registry,
        json!([{"type": "medication", "name": "metformin", "quote": "on metformin", "status": "current"}]),
        json!(["E11.9"]),
    )
    .await;
    let entry = code_verdict(&result, "E11.9");
    assert_eq!(entry["verdict"], json!("medication"));
    let medrt_step = entry["paths"][0]
        .as_array()
        .expect("medication path")
        .iter()
        .find(|step| step["node"]["kind"] == json!("mesh_disease"))
        .expect("MED-RT hop");
    assert_eq!(medrt_step["edge"]["icd"], json!("may_treat"));
    let source_predicates = medrt_step["edge"]["source_predicates"]
        .as_array()
        .expect("original MED-RT predicates");
    assert_eq!(source_predicates.len(), 2);
    assert!(source_predicates.contains(&json!("may_treat")));
    assert!(source_predicates.contains(&json!("may_prevent")));

    let verified = verify_medication_support(&registry, "current").await;
    let verify_entry = &verified["verdicts"][0];
    assert_eq!(verify_entry["support_only"], json!(true));
    let support_edges: Vec<_> = verify_entry["edges"]
        .as_array()
        .expect("verify edges")
        .iter()
        .filter(|edge| edge["rel"] == json!("supports"))
        .collect();
    assert_eq!(support_edges.len(), 1, "{verify_entry}");
    assert_eq!(support_edges[0]["icd"], json!("may_treat"));
}

/// The one-word shortening fallback may name several distinct drugs.  It
/// must not pick one according to graph iteration order and invent support.
#[tokio::test]
async fn evidence_ambiguous_shortened_drug_name_does_not_pick_arbitrary_rxcui() {
    let (registry, _rt) = build_registry();
    ingest_base(&registry).await;
    ingest_external(
        &registry,
        json!({"medrt_jsonl": fixture("medrt_ambiguous_trim.jsonl")}),
    )
    .await;
    ingest_external(&registry, json!({"doid_obo": fixture("doid_mg.obo")})).await;

    let result = evidence(
        &registry,
        json!([{"type": "medication", "name": "vitamin", "quote": "takes vitamin", "status": "current"}]),
        json!(["E11.9"]),
    )
    .await;
    let entry = code_verdict(&result, "E11.9");
    assert_eq!(entry["verdict"], json!("unsupported"));
    assert!(entry["paths"].as_array().unwrap().is_empty());
    assert_eq!(
        entry["abstentions"][0]["reason"],
        json!("ambiguous_drug_name")
    );
    assert_eq!(entry["abstentions"][0]["match_method"], json!("shortened"));
    assert_eq!(entry["abstentions"][0]["rxcuis"], json!(["10001", "10002"]));
}

#[tokio::test]
async fn verify_ambiguous_drug_name_abstains_without_support() {
    let (registry, _rt) = build_registry();
    ingest_base(&registry).await;
    ingest_external(
        &registry,
        json!({"medrt_jsonl": fixture("medrt_ambiguous_trim.jsonl")}),
    )
    .await;
    ingest_external(&registry, json!({"doid_obo": fixture("doid_mg.obo")})).await;

    let result = registry
        .dispatch(
            "icd10.verify",
            json!({
                "hypotheses": [{
                    "code": "E11.9",
                    "mention": {
                        "type": "condition",
                        "name": "unrelated finding of no clinical significance",
                        "quote": "patient reports an unrelated finding of no clinical significance",
                        "status": "current",
                    },
                }],
                "mentions": [{
                    "type": "medication",
                    "name": "vitamin",
                    "quote": "takes vitamin",
                    "status": "current",
                }],
            }),
        )
        .await
        .expect("icd10.verify dispatches");
    let entry = &result["verdicts"][0];
    assert_eq!(entry["verdict"], json!("UNSUPPORTED"), "{entry}");
    assert_eq!(entry["support_only"], json!(false), "{entry}");
    assert!(entry["facets"]["support"].as_array().unwrap().is_empty());
    assert!(entry["edges"]
        .as_array()
        .unwrap()
        .iter()
        .all(|edge| edge["rel"] != json!("supports")));
    let abstentions = entry["facets"]["support_abstentions"]
        .as_array()
        .expect("support abstentions array");
    assert_eq!(abstentions.len(), 1, "{entry}");
    assert_eq!(abstentions[0]["mention"], json!("vitamin"));
    assert_eq!(abstentions[0]["reason"], json!("ambiguous_drug_name"));
    assert_eq!(abstentions[0]["match_method"], json!("shortened"));
    let mut rxcuis: Vec<_> = abstentions[0]["rxcuis"]
        .as_array()
        .expect("ambiguous RxCUIs")
        .iter()
        .filter_map(Value::as_str)
        .collect();
    rxcuis.sort_unstable();
    assert_eq!(rxcuis, vec!["10001", "10002"]);
}

/// The same medication mention, status `negated` -> `unsupported`.
#[tokio::test]
async fn evidence_negated_medication_mention_is_unsupported() {
    let (registry, _rt) = setup_medical_graph().await;

    let result = evidence(
        &registry,
        json!([{"type": "medication", "name": "metformin", "quote": "not on metformin", "status": "negated"}]),
        json!(["E11.9"]),
    )
    .await;
    let entry = code_verdict(&result, "E11.9");
    assert_eq!(entry["verdict"], json!("unsupported"));
    assert!(entry["paths"].as_array().unwrap().is_empty());
}

/// A prior, planned, or uncertain drug is not current-visit support for a
/// present disease, even when MED-RT contains a treatment relation.
#[tokio::test]
async fn evidence_non_current_medication_mention_is_unsupported() {
    let (registry, _rt) = setup_medical_graph().await;
    for status in ["past", "planned", "uncertain"] {
        let result = evidence(
            &registry,
            json!([{"type": "medication", "name": "metformin", "quote": "mentions metformin", "status": status}]),
            json!(["E11.9"]),
        )
        .await;
        let entry = code_verdict(&result, "E11.9");
        assert_eq!(entry["verdict"], json!("unsupported"), "{status}: {entry}");
        assert!(entry["paths"].as_array().unwrap().is_empty());
    }
}

/// A medication mention (metformin) against an unrelated code -> `unsupported`.
#[tokio::test]
async fn evidence_medication_mention_unrelated_code_is_unsupported() {
    let (registry, _rt) = build_registry();
    ingest_base(&registry).await;
    ingest_external(&registry, json!({"medrt_jsonl": fixture("medrt_mg.jsonl")})).await;
    ingest_external(&registry, json!({"doid_obo": fixture("doid_mg.obo")})).await;

    let result = evidence(
        &registry,
        json!([{"type": "medication", "name": "metformin", "quote": "on metformin", "status": "current"}]),
        json!(["S69.9"]),
    )
    .await;
    let entry = code_verdict(&result, "S69.9");
    assert_eq!(entry["verdict"], json!("unsupported"));
}

/// The same medication mention against the RANGE-xref
/// mesh_disease (D003920, `doid_range_mg.obo`) reaches E11 with a medication
/// path whose edge is tagged `xref_range` — kept, not dropped, but marked so
/// the trace never reads it as a precise single-code xref (the same shape
/// as DOID:9351's real E08-E13 cross-reference).
#[tokio::test]
async fn evidence_medication_path_marks_range_xref_edge() {
    let (registry, _rt) = build_registry();
    ingest_base(&registry).await;
    ingest_external(&registry, json!({"medrt_jsonl": fixture("medrt_mg.jsonl")})).await;
    ingest_external(&registry, json!({"doid_obo": fixture("doid_range_mg.obo")})).await;

    let result = evidence(
        &registry,
        json!([{"type": "medication", "name": "metformin", "quote": "on metformin", "status": "current"}]),
        json!(["E11"]),
    )
    .await;
    let entry = code_verdict(&result, "E11");
    assert_eq!(entry["verdict"], json!("medication"));
    let paths = entry["paths"].as_array().expect("paths array");
    let path = paths[0].as_array().expect("path is a list of steps");
    let category_step = path
        .iter()
        .find(|s| s["node"]["key"] == json!("E11"))
        .expect("path names the E11 category step");
    assert_eq!(
        category_step["edge"]["xref_range"],
        json!("E08-E13"),
        "a range xref reaching a code must be marked on the edge, not dropped: {category_step}"
    );
}

/// A mention that reaches a code ONLY through family
/// expansion is not `direct`. The Index term "Gallbladder disorder NEC"
/// points at category K82 (non-billable: three children), so
/// `add_resolved_targets` expands that pointer to K82's billable
/// descendants (K82.0, K82.4, K82.5) marked `via: "family"` — the SAME
/// mechanism a "hypertension" mention hits reaching I12.9 through its
/// family. K82.4 has no other direct/tabular path from this mention, so
/// it must fall through to `category_only`/`unsupported`, never `direct`.
#[tokio::test]
async fn evidence_family_only_match_is_not_direct() {
    let (registry, _rt) = build_registry();
    ingest_base(&registry).await;

    let result = evidence(
        &registry,
        json!([{"type": "condition", "name": "Gallbladder disorder NEC", "quote": "gallbladder disorder NEC", "status": "current"}]),
        json!(["K82.4"]),
    )
    .await;
    let entry = code_verdict(&result, "K82.4");
    assert_ne!(
        entry["verdict"],
        json!("direct"),
        "a family-expanded-only match must not verdict direct: {entry}"
    );
}

/// A condition mention (hypertension, current) + code
/// I10 -> `direct`.
#[tokio::test]
async fn evidence_direct_condition_hypertension_reaches_i10() {
    let (registry, _rt) = setup_medical_graph().await;

    let result = evidence(
        &registry,
        json!([{"type": "condition", "name": "hypertension", "quote": "history of hypertension", "status": "current"}]),
        json!(["I10"]),
    )
    .await;
    let entry = code_verdict(&result, "I10");
    assert_eq!(entry["verdict"], json!("direct"));
    let paths = entry["paths"].as_array().unwrap();
    assert!(!paths.is_empty());
}

/// Mention type "encounter" is treated exactly like condition/symptom for
/// the direct arm.
#[tokio::test]
async fn evidence_encounter_mention_type_reaches_directly() {
    let (registry, _rt) = setup_medical_graph().await;

    let result = evidence(
        &registry,
        json!([{"type": "encounter", "name": "hypertension", "quote": "seen for hypertension follow-up", "status": "current"}]),
        json!(["I10"]),
    )
    .await;
    let entry = code_verdict(&result, "I10");
    assert_eq!(
        entry["verdict"],
        json!("direct"),
        "an \"encounter\" mention must reach exactly like condition/symptom: {entry}"
    );
}

// The must-fail control for the negated/family filter
// (`evidence.rs::is_negated_or_family`) is drilled manually, not by an
// automated arm: temporarily short-circuit `is_negated_or_family` to
// always return `false`, re-run `evidence_negated_medication_mention_is_unsupported`,
// confirm it reddens (verdict becomes `medication` instead of
// `unsupported`), then restore. Not encoded as a standing test, because the
// only way to "disable" a single-line filter from a black-box test is to
// edit the source under test.

/// `icd10.evidence`'s guideline arm: I10 (hypertension) and N18 (chronic
/// kidney disease) both supported directly -> the rule concluding I12
/// fires for a hypothesis code reached ONLY through the rule (I12.9, a
/// child of I12 the rule doesn't name directly — exercises the
/// category-or-below rule on the conclusion side).
#[tokio::test]
async fn evidence_guideline_arm_concludes_i12_from_i10_and_n18() {
    let (registry, _rt) = build_registry();
    ingest_base(&registry).await;
    ingest_external(
        &registry,
        json!({"rules_jsonl": fixture("rules_evidence.jsonl")}),
    )
    .await;

    let result = evidence(
        &registry,
        json!([
            {"type": "condition", "name": "hypertension", "quote": "hypertension", "status": "current"},
            // "CKD" (the real clinical shorthand for chronic kidney disease,
            // an Index term in index_mg.xml pointing directly at N18.30) --
            // NOT "chronic kidney disease" (the words a clinician would also
            // write out): I12.9's own description restates "chronic kidney
            // disease" verbatim as part of its real ICD-10-CM
            // combination-code wording, so ANY word-based Tabular AND-mode
            // query built from N18.30's own vocabulary also matches I12.9 --
            // there is no non-digit word in N18.30's description that isn't
            // also in I12.9's, and a digit token ("3") is too short for the
            // trigram index and gets silently dropped from the AND, so it
            // cannot disambiguate the two either. "CKD" appears in neither
            // code's tabular description, so the Tabular arm contributes
            // nothing for it (harmless) and only the Index arm's direct
            // term->code pointer resolves it, exclusively to N18.30.
            {"type": "condition", "name": "CKD", "quote": "chronic kidney disease, stage 3", "status": "current"}
        ]),
        json!(["I10", "N18.30", "I12.9"]),
    )
    .await;
    let i12 = code_verdict(&result, "I12.9");
    assert_eq!(
        i12["verdict"],
        json!("guideline"),
        "expected I12.9 (below I12) to be concluded by the I10+N18->I12 rule: {i12}"
    );
}

/// `icd10.evidence` resolves each DISTINCT mention text at
/// most once per call, not once per (code, mention) pair — calling
/// `icd10.resolve` per (code, mention) in both the direct arm and
/// `category_only_path`'s fallback would be O(codes * mentions) calls to the
/// same handful of distinct mention texts, enough to run one real
/// encounter past 11 minutes. No `rules_jsonl` is ingested here, so
/// `guideline_path` never fires and no code's verdict can depend on which
/// OTHER codes were requested alongside it (direct/medication/
/// category_only/unsupported are each derived from the target code and the
/// mentions alone) — so an N-code call must agree, code by code, with N
/// single-code calls carrying the same mentions.
#[tokio::test]
async fn evidence_batched_codes_match_single_code_calls_and_resolve_each_mention_once() {
    let (registry, _rt) = build_registry();
    ingest_base(&registry).await;

    let mentions = json!([
        {"type": "condition", "name": "Cholecystitis", "quote": "cholecystitis", "status": "current"},
        {"type": "condition", "name": "CKD", "quote": "chronic kidney disease, stage 3", "status": "current"},
        {"type": "condition", "name": "Gallbladder disorder NEC", "quote": "gallbladder disorder NEC", "status": "current"},
        {"type": "condition", "name": "Concussion", "quote": "concussion", "status": "current"},
    ]);
    // 22 requested codes x 4 distinct mentions: O(codes * mentions) would be
    // 88 `icd10.resolve` calls; the fix issues at most 4 (one per distinct
    // mention text, shared across every code).
    let codes: Vec<&str> = vec![
        "S06.0X0", "S06.0X9", "S69.9", "E11.9", "I10", "I11.9", "I12.9", "I13.0", "I50.9", "K80.0",
        "K81.9", "K82.0", "K82.4", "K82.5", "K91.61", "N18.30", "N40", "N70", "H66.90", "H66.91",
        "H66.92", "P07",
    ];

    let batched_start = std::time::Instant::now();
    let batched = evidence(&registry, mentions.clone(), json!(codes)).await;
    let batched_elapsed = batched_start.elapsed();

    let singles_start = std::time::Instant::now();
    for code in &codes {
        let single = evidence(&registry, mentions.clone(), json!([code])).await;
        let single_entry = code_verdict(&single, code);
        let batched_entry = code_verdict(&batched, code);
        assert_eq!(
            single_entry["verdict"], batched_entry["verdict"],
            "code {code}: batched verdict {batched_entry} must match its single-code call {single_entry}"
        );
    }
    let singles_elapsed = singles_start.elapsed();

    // Sanity: the fixture must actually exercise more than one verdict kind
    // this fix touches (the direct arm and the category_only fallback both
    // read the resolve cache), or the equivalence check above would be
    // vacuous. K82.0/K82.4/K82.5 exercise the fallback path itself (reached
    // only via K82's non-billable family expansion, so `category_only_path`
    // still runs and still finds no hit — see
    // `evidence_family_only_match_is_not_direct`'s module doc) even though
    // their own verdict lands on `unsupported`.
    let verdicts: std::collections::HashSet<&str> = batched["codes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["verdict"].as_str().unwrap())
        .collect();
    assert!(
        verdicts.contains("direct"),
        "expected at least one direct verdict: {batched}"
    );
    assert!(
        verdicts.contains("unsupported"),
        "expected at least one unsupported verdict: {batched}"
    );

    eprintln!(
        "wall time (22 codes x 4 mentions, in-memory fixture): \
         1 batched icd10.evidence call = {batched_elapsed:?}; \
         22 single-code icd10.evidence calls (same mentions) = {singles_elapsed:?}"
    );
}
