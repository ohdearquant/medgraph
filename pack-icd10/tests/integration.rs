//! Fixture-driven integration tests for the `icd10` pack: ingest twice (id
//! stability across a repeat ingest), every relation role the pack declares
//! produced at least once, a merged two-role edge, `.-` resolving to
//! the category, `icd10.neighborhood`, and `icd10.check` R1-R7.
//!
//! Everything here goes through `registry.dispatch`, the same surface a
//! caller of the pack has: `NamespaceToken` is sealed outside
//! `khive-runtime` and cannot be minted directly in a pack's own tests.

use khive_pack_icd10::Icd10Pack;
use khive_runtime::{KhiveRuntime, VerbRegistry, VerbRegistryBuilder};
use khive_types::Pack;
use serde_json::json;

fn fixture(name: &str) -> String {
    format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"))
}

fn build_registry() -> (VerbRegistry, KhiveRuntime) {
    let runtime = KhiveRuntime::memory().expect("in-memory runtime");
    let mut builder = VerbRegistryBuilder::new();
    builder.register(khive_pack_kg::KgPack::new(runtime.clone()));
    builder.register(Icd10Pack::new(runtime.clone()));
    let registry = builder.build().expect("registry builds");
    // `VerbRegistryBuilder::build()` alone does not push the aggregated pack
    // edge rules onto the runtime's endpoint validator — every real boot
    // path (`build_ingest_registry`, the MCP server) does this extra step,
    // and skipping it here made every icd10-declared relation beyond the
    // base concept->concept contract (precedes, links_to) fail
    // validation even though the pack's own EDGE_RULES were correct.
    runtime.install_edge_rules(registry.all_edge_rules());
    (registry, runtime)
}

async fn ingest(registry: &VerbRegistry) -> serde_json::Value {
    registry
        .dispatch(
            "icd10.ingest",
            json!({
                "tabular_path": fixture("tabular.xml"),
                "index_path": fixture("index.xml"),
            }),
        )
        .await
        .expect("icd10.ingest dispatches")
}

async fn neighborhood(registry: &VerbRegistry, code: &str) -> serde_json::Value {
    registry
        .dispatch("icd10.neighborhood", json!({"code": code}))
        .await
        .unwrap_or_else(|e| panic!("neighborhood({code}) dispatches: {e}"))
}

#[test]
fn icd10_pack_name_is_stable() {
    assert_eq!(Icd10Pack::NAME, "icd10");
}

#[test]
fn icd10_pack_requires_kg() {
    assert_eq!(Icd10Pack::REQUIRES, &["kg"]);
}

#[tokio::test]
async fn ingest_dry_run_reports_counts_without_writing() {
    let (registry, _rt) = build_registry();
    let result = registry
        .dispatch(
            "icd10.ingest",
            json!({
                "tabular_path": fixture("tabular.xml"),
                "index_path": fixture("index.xml"),
                "dry_run": true,
            }),
        )
        .await
        .expect("dry-run dispatches");
    assert_eq!(result["dry_run"], true);
    assert!(result["counts"]["icd_code"].as_u64().unwrap() > 0);

    // A dry run must not have written anything: a real ingest that follows
    // still reports the same nonzero creation counts (nothing pre-existed).
    let real = ingest(&registry).await;
    assert_eq!(real["dry_run"], false);
    assert!(real["created"]["icd_code"].as_u64().unwrap() > 0);
}

#[tokio::test]
async fn ids_are_stable_across_a_repeat_ingest() {
    let (registry, _rt) = build_registry();

    let first = ingest(&registry).await;
    assert!(first["created"]["icd_code"].as_u64().unwrap() > 0);
    let before_id = neighborhood(&registry, "K82.4").await["record"]["id"].clone();

    // Repeat ingest of the identical fixture: `created` counts must be zero
    // (ensure_entities finds every name already present via a
    // list-then-create-if-missing lookup, since ids here are not derived
    // deterministically from source data) and the id for a known code must
    // be unchanged.
    let second = ingest(&registry).await;
    assert_eq!(second["created"]["icd_code"], json!(0));
    assert_eq!(second["created"]["icd_chapter"], json!(0));

    let after_id = neighborhood(&registry, "K82.4").await["record"]["id"].clone();
    assert_eq!(
        before_id, after_id,
        "K82.4's id must be stable across a repeat ingest"
    );
}

#[tokio::test]
async fn seven_char_expansion_produces_billable_children() {
    let (registry, _rt) = build_registry();
    ingest(&registry).await;

    let result = neighborhood(&registry, "S06.0X0").await;
    let children = result["children"].as_array().expect("children array");
    for suffix in ["A", "D", "S"] {
        let code = format!("S06.0X0{suffix}");
        let child = children
            .iter()
            .find(|c| c["name"] == json!(code))
            .unwrap_or_else(|| {
                panic!("{code} was not synthesized as a child of S06.0X0; children={children:?}")
            });
        assert_eq!(child["properties"]["billable"], json!(true));
    }
}

#[tokio::test]
async fn every_relation_role_appears_at_least_once() {
    let (registry, _rt) = build_registry();
    let result = ingest(&registry).await;
    let edges = result["edges"].as_object().expect("edges object");
    for role in [
        "hierarchy",
        "excludes1",
        "excludes2",
        "code_also",
        "code_first",
        "use_additional",
        "index_hierarchy",
        "index",
        "see",
        "see_also",
    ] {
        let n = edges.get(role).and_then(|v| v.as_u64()).unwrap_or(0);
        assert!(
            n > 0,
            "expected role {role:?} to appear at least once, edges={edges:?}"
        );
    }
}

#[tokio::test]
async fn family_ref_resolves_to_the_category() {
    let (registry, _rt) = build_registry();
    ingest(&registry).await;

    // K82.0's excludes1 note "(K80.-)" must resolve to the K80 *category*
    // (family ref), not fail to resolve.
    let result = neighborhood(&registry, "K82.0").await;
    let excludes1 = result["excludes1"].as_array().expect("excludes1 array");
    assert!(
        excludes1
            .iter()
            .any(|v| v["name"] == json!("K80") && v["entity_type"] == json!("icd_category")),
        "expected an excludes1 partner named K80 with entity_type icd_category; got {excludes1:?}"
    );
}

#[tokio::test]
async fn two_roles_on_the_same_pair_merge_onto_one_edge() {
    // K82.4 carries BOTH an excludes2 note and a codeAlso note pointing at
    // the same target (K81.9): both roles must be visible from the SAME
    // underlying (K82.4, K81.9, ComposedWith) edge — storage's "a second
    // link replaces the first" would otherwise silently drop one role.
    let (registry, _rt) = build_registry();
    ingest(&registry).await;

    let result = neighborhood(&registry, "K82.4").await;
    let excludes2 = result["excludes2"].as_array().expect("excludes2 array");
    let code_also = result["code_also"].as_array().expect("code_also array");
    assert!(
        excludes2.iter().any(|v| v["name"] == json!("K81.9")),
        "expected K81.9 in excludes2; got {excludes2:?}"
    );
    assert!(
        code_also.iter().any(|v| v["name"] == json!("K81.9")),
        "expected K81.9 in code_also; got {code_also:?}"
    );
}

#[tokio::test]
async fn neighborhood_k82_4_reports_parent_and_excludes1() {
    let (registry, _rt) = build_registry();
    ingest(&registry).await;

    let result = neighborhood(&registry, "K82.4").await;
    assert_eq!(result["parent"]["name"], json!("K82"));
    let excludes1 = result["excludes1"].as_array().expect("excludes1 array");
    let names: Vec<&str> = excludes1
        .iter()
        .filter_map(|v| v["name"].as_str())
        .collect();
    assert!(
        names.contains(&"K80"),
        "K82.4 excludes1 should include K80; got {names:?}"
    );
    assert!(
        names.contains(&"K81"),
        "K82.4 excludes1 should include K81; got {names:?}"
    );

    let index_terms = result["index_terms"].as_array().expect("index_terms array");
    assert!(
        index_terms
            .iter()
            .any(|v| v["name"].as_str() == Some("Cholesterolosis (-osis) > gallbladder")),
        "expected the index term pointing at K82.4; got {index_terms:?}"
    );
}

#[tokio::test]
async fn check_unknown_code_is_dropped_r1() {
    let (registry, _rt) = build_registry();
    ingest(&registry).await;

    let result = registry
        .dispatch("icd10.check", json!({"codes": ["Z999.99"]}))
        .await
        .expect("check dispatches");
    let verdicts = result["verdicts"].as_array().unwrap();
    assert_eq!(verdicts[0]["action"], json!("drop"));
    assert_eq!(verdicts[0]["rule"], json!("R1 unknown"));
}

#[tokio::test]
async fn check_not_billable_category_is_replaced_r2() {
    let (registry, _rt) = build_registry();
    ingest(&registry).await;

    // K81 is a non-billable category with exactly one billable child whose
    // description contains "unspecified" (K81.9), so R2 should replace it.
    let result = registry
        .dispatch("icd10.check", json!({"codes": ["K81"]}))
        .await
        .expect("check dispatches");
    let verdicts = result["verdicts"].as_array().unwrap();
    assert_eq!(verdicts[0]["rule"], json!("R2 not_billable"));
    assert_eq!(verdicts[0]["replacement"], json!("K81.9"));
}

#[tokio::test]
async fn check_r3_does_not_drop_siblings() {
    // A non-billable ancestor is handled by R2 before R3; R3 covers billable
    // ancestor pairs (rare). A category
    // ancestor never reaches R3 (R2 intercepts it first, either replacing
    // or dropping it) and two sibling billable codes must both survive —
    // R3 must not mistake siblings for an ancestor/descendant pair.
    let (registry, _rt) = build_registry();
    ingest(&registry).await;

    let result = registry
        .dispatch("icd10.check", json!({"codes": ["K80.0", "K81.9"]}))
        .await
        .expect("check dispatches");
    assert_eq!(result["kept"], json!(["K80.0", "K81.9"]));
}

#[tokio::test]
async fn check_missing_seventh_character_r7_drops_the_base_code() {
    let (registry, _rt) = build_registry();
    ingest(&registry).await;

    // S06.0X0 itself (not one of its A/D/S expansions) requires a 7th
    // character and is not billable as-is.
    let result = registry
        .dispatch("icd10.check", json!({"codes": ["S06.0X0"]}))
        .await
        .expect("check dispatches");
    let verdicts = result["verdicts"].as_array().unwrap();
    assert_eq!(verdicts[0]["rule"], json!("R7 missing_7th_character"));
    assert_eq!(verdicts[0]["action"], json!("drop"));
    assert_eq!(result["kept"], json!([]));
}

#[tokio::test]
async fn check_r7_repairs_to_a_unique_unspecified_expansion_instead_of_dropping() {
    // K82.5 requires a 7th character and has exactly one seventh-character
    // extension (A), whose expanded description ("Unspecified disorder of
    // gallbladder, initial encounter") contains "unspecified" — the same
    // repair condition R2 already applies to a non-billable category/code
    // with a unique billable "unspecified" child. Regression test (E09.321 ->
    // E09.3219): a missing-7th-character code takes the same repair path as any
    // other non-billable code and is replaced; before this fix, check.rs's R7
    // branch dropped unconditionally without ever trying the repair.
    let (registry, _rt) = build_registry();
    ingest(&registry).await;

    let result = registry
        .dispatch("icd10.check", json!({"codes": ["K82.5"]}))
        .await
        .expect("check dispatches");
    let verdicts = result["verdicts"].as_array().unwrap();
    assert_eq!(
        verdicts[0]["rule"],
        json!("R7 missing_7th_character"),
        "the R7 label must be kept even when the repair succeeds: {result}"
    );
    assert_eq!(
        verdicts[0]["action"],
        json!("replace"),
        "a unique unspecified expansion must be a replace, not a drop: {result}"
    );
    assert_eq!(verdicts[0]["replacement"], json!("K82.5XXA"));
    assert_eq!(result["kept"], json!(["K82.5XXA"]));
}

#[tokio::test]
async fn leaf_category_is_billable_and_checked_directly() {
    // I10 is a real ICD-10-CM category (Essential hypertension) with no
    // nested <diag> children in the tabular XML — a category-level leaf.
    // A childless diag is billable at any
    // depth; before the fix, this pack's ingest only ever gave a category
    // its own `icd_code` row for its NESTED children, so a childless
    // category like I10 had no billable representation at all and R2
    // dropped it ("pick a billable child" when there were no children to
    // pick from).
    let (registry, _rt) = build_registry();
    ingest(&registry).await;

    let record = neighborhood(&registry, "I10").await;
    assert_eq!(
        record["record"]["properties"]["billable"],
        json!(true),
        "I10 must be billable in the graph: {record}"
    );

    let result = registry
        .dispatch("icd10.check", json!({"codes": ["I10"]}))
        .await
        .expect("check dispatches");
    let verdicts = result["verdicts"].as_array().unwrap();
    assert_eq!(
        verdicts[0]["rule"],
        json!("ok"),
        "I10 must be kept directly, not routed through R2: {result}"
    );
    assert_eq!(verdicts[0]["action"], json!("keep"));
    assert_eq!(result["kept"], json!(["I10"]));
}

#[tokio::test]
async fn check_excludes1_conflict_r4_drops_the_later_code() {
    let (registry, _rt) = build_registry();
    ingest(&registry).await;

    let result = registry
        .dispatch("icd10.check", json!({"codes": ["K82.4", "K81.9"]}))
        .await
        .expect("check dispatches");
    assert_eq!(result["kept"], json!(["K82.4"]));
    let dropped = result["verdicts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["code"] == json!("K81.9"))
        .expect("K81.9 verdict present");
    assert_eq!(dropped["rule"], json!("R4 excludes1"));
}

// ── icd10.resolve ────────────────────────────────────────────────────────

async fn resolve(registry: &VerbRegistry, mentions: serde_json::Value) -> serde_json::Value {
    registry
        .dispatch("icd10.resolve", json!({"mentions": mentions}))
        .await
        .expect("icd10.resolve dispatches")
}

fn matched_of<'a>(
    candidates: &'a [serde_json::Value],
    code: &str,
) -> Option<&'a serde_json::Value> {
    candidates.iter().find(|c| c["code"] == json!(code))
}

fn has_match(candidate: &serde_json::Value, arm: &str, via: &str) -> bool {
    candidate["matched"]
        .as_array()
        .expect("matched array")
        .iter()
        .any(|m| m["arm"] == json!(arm) && m["via"] == json!(via))
}

#[tokio::test]
async fn resolve_hits_k82_4_through_index_and_tabular_arms() {
    let (registry, _rt) = build_registry();
    ingest(&registry).await;

    let result = resolve(
        &registry,
        json!([{"text": "Cholesterolosis of gallbladder"}]),
    )
    .await;
    let mention = &result["mentions"][0];
    let candidates = mention["candidates"].as_array().expect("candidates array");
    let k82_4 = matched_of(candidates, "K82.4").expect("K82.4 is a candidate");
    assert!(
        has_match(k82_4, "index", "direct"),
        "expected K82.4 reached via the index arm (its own Index term \
         \"Cholesterolosis (-osis) > gallbladder\"); matched={:?}",
        k82_4["matched"]
    );
    assert!(
        has_match(k82_4, "tabular", "direct"),
        "expected K82.4 reached via the tabular arm (its own description); matched={:?}",
        k82_4["matched"]
    );
    assert_eq!(k82_4["billable"], json!(true));
    assert_eq!(k82_4["desc"], json!("Cholesterolosis of gallbladder"));

    let arm_hit_counts = &mention["arm_hit_counts"];
    assert!(arm_hit_counts["index"].as_u64().unwrap() > 0);
    assert!(arm_hit_counts["tabular"].as_u64().unwrap() > 0);
    assert_eq!(arm_hit_counts["vector"], json!(0));
    // The vector arm runs only when the caller asks for it (`dense: true`).
    assert_eq!(result["vector_arm"], json!("off"));
}

#[tokio::test]
async fn resolve_category_pointer_expands_to_billable_descendants() {
    let (registry, _rt) = build_registry();
    ingest(&registry).await;

    // The Index term "Gallbladder disorder NEC" points at K82, a
    // non-billable *category* with real children: a pointer ending in `-`,
    // or pointing at a category, expands to that record's billable
    // descendants (K82.0, K82.4 directly, plus K82.5XXA two
    // levels down through K82.5's own 7th-character expansion).
    let result = resolve(&registry, json!([{"text": "Gallbladder disorder NEC"}])).await;
    let candidates = result["mentions"][0]["candidates"]
        .as_array()
        .expect("candidates array");

    for code in ["K82.0", "K82.4", "K82.5XXA"] {
        let c = matched_of(candidates, code).unwrap_or_else(|| {
            panic!(
                "{code} was not expanded from the K82 category pointer; candidates={candidates:?}"
            )
        });
        assert!(
            has_match(c, "index", "family"),
            "expected {code} via arm=index via=family; matched={:?}",
            c["matched"]
        );
        assert_eq!(c["billable"], json!(true));
    }
    // K82 itself (the non-billable pointer target) must not appear as a
    // candidate — only its billable descendants do.
    assert!(matched_of(candidates, "K82").is_none());
}

#[tokio::test]
async fn resolve_follows_a_see_hop() {
    let (registry, _rt) = build_registry();
    ingest(&registry).await;

    // The Index term "Concussive" carries only a `<see>Concussion</see>` —
    // no code pointer of its own — so every candidate it contributes must
    // come from following that one hop to "Concussion" (code S06.0X0, which
    // requires a 7th character and so expands to its billable A/D/S
    // extensions).
    let result = resolve(&registry, json!([{"text": "Concussive"}])).await;
    let candidates = result["mentions"][0]["candidates"]
        .as_array()
        .expect("candidates array");

    for suffix in ["A", "D", "S"] {
        let code = format!("S06.0X0{suffix}");
        let c = matched_of(candidates, &code).unwrap_or_else(|| {
            panic!("{code} was not reached through the see-hop; candidates={candidates:?}")
        });
        assert!(
            has_match(c, "index", "see"),
            "expected {code} via arm=index via=see; matched={:?}",
            c["matched"]
        );
    }
}

#[tokio::test]
async fn resolve_follows_a_see_reference_into_the_targets_subterms() {
    let (registry, _rt) = build_registry();
    ingest(&registry).await;

    // "Cholesterol deposit" carries only `<see>Cholesterolosis</see>`, and the
    // main term it names is titled "Cholesterolosis (-osis)": the reference
    // cites the title without its nonessential modifier. The rest of the
    // mention ("gallbladder") then selects the target's subterm
    // "Cholesterolosis (-osis) > gallbladder" (K82.4). A direct Index hit on
    // that subterm would be `via: direct`; only the see reference followed
    // into the subterm yields `via: see` with the subterm's own path.
    let result = registry
        .dispatch(
            "icd10.resolve",
            json!({"mentions": [{"text": "cholesterol deposit gallbladder"}], "text_mode": "any_term"}),
        )
        .await
        .expect("icd10.resolve dispatches");
    let candidates = result["mentions"][0]["candidates"]
        .as_array()
        .expect("candidates array");
    let k82_4 = matched_of(candidates, "K82.4")
        .unwrap_or_else(|| panic!("K82.4 not reached; candidates={candidates:?}"));
    let via_see_subterm = k82_4["matched"]
        .as_array()
        .expect("matched array")
        .iter()
        .any(|m| {
            m["arm"] == json!("index")
                && m["via"] == json!("see")
                && m["term_path"] == json!("Cholesterolosis (-osis) > gallbladder")
        });
    assert!(
        via_see_subterm,
        "expected K82.4 via the see reference into its subterm; matched={:?}",
        k82_4["matched"]
    );
}

#[tokio::test]
async fn resolve_family_expansion_adds_category_siblings() {
    let (registry, _rt) = build_registry();
    ingest(&registry).await;

    // "Obstruction of gallbladder" matches ONLY K82.0's own tabular
    // description (no Index term in this fixture points at K82.0), so any
    // OTHER K82 sibling reaching the candidate list must come from the
    // top-level family-expansion step (every hit code adds its category's
    // billable leaves, marked via=family), not from a direct
    // hit of its own.
    let result = resolve(&registry, json!([{"text": "Obstruction of gallbladder"}])).await;
    let candidates = result["mentions"][0]["candidates"]
        .as_array()
        .expect("candidates array");

    let k82_0 = matched_of(candidates, "K82.0").expect("K82.0 is a direct tabular hit");
    assert!(has_match(k82_0, "tabular", "direct"));

    let k82_4 = matched_of(candidates, "K82.4")
        .expect("K82.4 must be present via family expansion of K82.0's parent category K82");
    assert!(
        has_match(k82_4, "tabular", "family"),
        "expected K82.4 via arm=tabular via=family (inherited from K82.0's origin); matched={:?}",
        k82_4["matched"]
    );
}

#[tokio::test]
async fn resolve_disabling_family_expansion_drops_the_sibling() {
    let (registry, _rt) = build_registry();
    ingest(&registry).await;

    let result = registry
        .dispatch(
            "icd10.resolve",
            json!({
                "mentions": [{"text": "Obstruction of gallbladder"}],
                "expand_family": false,
            }),
        )
        .await
        .expect("icd10.resolve dispatches");
    let candidates = result["mentions"][0]["candidates"]
        .as_array()
        .expect("candidates array");
    assert!(matched_of(candidates, "K82.0").is_some());
    assert!(
        matched_of(candidates, "K82.4").is_none(),
        "expand_family=false must not add K82.4 via family expansion; candidates={candidates:?}"
    );
}

/// The order key `icd10.resolve` sorts candidates by: best (lowest) arm rank,
/// then more distinct arms first, then code ascending.
fn order_key(candidate: &serde_json::Value) -> (u64, std::cmp::Reverse<usize>, String) {
    let matched = candidate["matched"].as_array().expect("matched array");
    let best_rank = matched
        .iter()
        .map(|m| m["rank"].as_u64().expect("rank"))
        .min()
        .expect("at least one match");
    let arms: std::collections::BTreeSet<&str> = matched
        .iter()
        .map(|m| m["arm"].as_str().expect("arm"))
        .collect();
    let code = candidate["code"].as_str().expect("code").to_string();
    (best_rank, std::cmp::Reverse(arms.len()), code)
}

#[tokio::test]
async fn resolve_orders_tied_candidates_by_code() {
    let (registry, _rt) = build_registry();
    ingest(&registry).await;

    // "Gallbladder disorder NEC" points at the K82 category, which expands to
    // K82.0, K82.4 and K82.5XXA with the pointer's own rank and one arm each:
    // a tie that rank and arm count cannot order. Candidates accumulate in a
    // HashMap whose iteration order differs per map instance, so each call
    // below is a fresh chance for an untied order to show.
    let mut first: Option<Vec<String>> = None;
    for call in 0..16 {
        let result = resolve(&registry, json!([{"text": "Gallbladder disorder NEC"}])).await;
        let candidates = result["mentions"][0]["candidates"]
            .as_array()
            .expect("candidates array");

        // Control: the tie this test is about is really present.
        let tied: Vec<_> = ["K82.0", "K82.4", "K82.5XXA"]
            .iter()
            .map(|code| {
                let c = matched_of(candidates, code)
                    .unwrap_or_else(|| panic!("{code} missing; candidates={candidates:?}"));
                let (rank, arms, _) = order_key(c);
                (rank, arms)
            })
            .collect();
        assert!(
            tied.windows(2).all(|w| w[0] == w[1]),
            "fixture no longer ties K82.0/K82.4/K82.5XXA on (rank, arm count): {tied:?}"
        );

        let keys: Vec<_> = candidates.iter().map(order_key).collect();
        for w in keys.windows(2) {
            assert!(
                w[0] <= w[1],
                "call {call}: out of (rank, arm count desc, code) order: {:?} before {:?}",
                w[0],
                w[1]
            );
        }

        let codes: Vec<String> = keys.into_iter().map(|(_, _, code)| code).collect();
        if let Some(expected) = &first {
            assert_eq!(&codes, expected, "call {call}: order changed between calls");
        } else {
            first = Some(codes);
        }
    }
}

#[tokio::test]
async fn resolve_any_term_finds_what_all_terms_misses() {
    let (registry, _rt) = build_registry();
    ingest(&registry).await;

    // "gallbladder concussion" combines a word from K82.4's own tabular
    // description ("Cholesterolosis of gallbladder") with a word from an
    // unrelated code's description ("Concussion") -- no fixture term or code
    // description contains both words, so the implicit-AND Plain mode
    // (`text_mode` default, `"all_terms"`) must return zero candidates.
    let all_terms = resolve(&registry, json!([{"text": "gallbladder concussion"}])).await;
    let all_terms_candidates = all_terms["mentions"][0]["candidates"]
        .as_array()
        .expect("candidates array");
    assert!(
        all_terms_candidates.is_empty(),
        "all_terms (implicit AND) must find nothing for a mention whose words never \
         co-occur in one term/code; candidates={all_terms_candidates:?}"
    );
    assert_eq!(
        all_terms["mentions"][0]["text_mode"]["index"],
        json!("all_terms")
    );

    let any_term = registry
        .dispatch(
            "icd10.resolve",
            json!({
                "mentions": [{"text": "gallbladder concussion"}],
                "text_mode": "any_term",
            }),
        )
        .await
        .expect("icd10.resolve dispatches under any_term");
    let any_term_candidates = any_term["mentions"][0]["candidates"]
        .as_array()
        .expect("candidates array");
    let k82_4 = matched_of(any_term_candidates, "K82.4")
        .expect("any_term (OR-join) must surface K82.4 via its own \"gallbladder\" word");
    assert_eq!(k82_4["billable"], json!(true));
    assert_eq!(
        any_term["mentions"][0]["text_mode"]["index"],
        json!("any_term")
    );
    assert_eq!(
        any_term["mentions"][0]["text_mode"]["tabular"],
        json!("any_term")
    );
}

#[tokio::test]
async fn resolve_any_term_tabular_arm_still_honors_entity_type_restriction() {
    let (registry, _rt) = build_registry();
    ingest(&registry).await;

    // "concussive" only matches the icd_term entity "Concussive" (an Index
    // mainTerm carrying just a <see>Concussion</see> pointer, no code of its
    // own) -- no icd_code description contains this exact token (only
    // "Concussion", a distinct FTS token). An AnyTerm search unrestricted by
    // entity_type would leak that icd_term entity into the tabular arm
    // (entity_type=icd_code) as though it were a code.
    let result = registry
        .dispatch(
            "icd10.resolve",
            json!({
                "mentions": [{"text": "concussive"}],
                "text_mode": "any_term",
            }),
        )
        .await
        .expect("icd10.resolve dispatches under any_term");
    let mention = &result["mentions"][0];
    assert_eq!(
        mention["arm_hit_counts"]["tabular"],
        json!(0),
        "the tabular arm (entity_type=icd_code) must not surface the icd_term entity \
         \"Concussive\" just because AnyTerm matched its indexed text; mention={mention:?}"
    );
    let candidates = mention["candidates"].as_array().expect("candidates array");
    assert!(
        matched_of(candidates, "Concussive").is_none(),
        "the icd_term entity's own path must never appear as a candidate \"code\"; \
         candidates={candidates:?}"
    );
}

// ── icd10.exclude ────────────────────────────────────────────────────────

async fn exclude(registry: &VerbRegistry, args: serde_json::Value) -> serde_json::Value {
    registry
        .dispatch("icd10.exclude", args)
        .await
        .expect("icd10.exclude dispatches")
}

fn dropped_rule<'a>(result: &'a serde_json::Value, code: &str) -> Option<&'a str> {
    result["dropped"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["code"] == json!(code))
        .and_then(|d| d["rule"].as_str())
}

#[tokio::test]
async fn exclude_e1_drops_non_billable_and_names_the_unique_unspecified_child() {
    let (registry, _rt) = build_registry();
    ingest(&registry).await;

    let result = exclude(&registry, json!({"candidates": ["K81"]})).await;
    assert_eq!(result["kept"], json!([]));
    assert_eq!(dropped_rule(&result, "K81"), Some("E1"));
    let reason = result["dropped"][0]["reason"].as_str().unwrap();
    assert!(
        reason.contains("K81.9"),
        "E1's reason should name the unique unspecified child (reuses \
         check::unique_unspecified_child): {reason:?}"
    );
}

#[tokio::test]
async fn exclude_e1_billable_leaf_stays_only_when_independently_a_candidate() {
    let (registry, _rt) = build_registry();
    ingest(&registry).await;

    let result = exclude(&registry, json!({"candidates": ["K81", "K81.9"]})).await;
    assert_eq!(result["kept"], json!(["K81.9"]));
    assert_eq!(dropped_rule(&result, "K81"), Some("E1"));
}

#[tokio::test]
async fn exclude_e2_drops_sex_conflict() {
    let (registry, _rt) = build_registry();
    ingest(&registry).await;

    // N70 (Salpingitis and oophoritis) is female-only; sex=M must drop it.
    let result = exclude(&registry, json!({"candidates": ["N70"], "sex": "M"})).await;
    assert_eq!(result["kept"], json!([]));
    assert_eq!(dropped_rule(&result, "N70"), Some("E2"));
}

#[tokio::test]
async fn exclude_e3_drops_age_conflict() {
    let (registry, _rt) = build_registry();
    ingest(&registry).await;

    // P07 is a perinatal code; a 5-year-old patient must drop it.
    let result = exclude(&registry, json!({"candidates": ["P07"], "age_years": 5.0})).await;
    assert_eq!(result["kept"], json!([]));
    assert_eq!(dropped_rule(&result, "P07"), Some("E3"));
}

#[tokio::test]
async fn exclude_e4_drops_excludes1_conflict_with_chosen() {
    let (registry, _rt) = build_registry();
    ingest(&registry).await;

    // Same K82.4/K81.9 Excludes1 pair as check.rs's own R4 test, but here
    // K82.4 arrives pre-selected via `chosen` rather than in the same list.
    let result = exclude(
        &registry,
        json!({"candidates": ["K81.9"], "chosen": ["K82.4"]}),
    )
    .await;
    assert_eq!(result["kept"], json!([]));
    assert_eq!(dropped_rule(&result, "K81.9"), Some("E4"));
    let reason = result["dropped"][0]["reason"].as_str().unwrap();
    assert!(reason.contains("K82.4"));
}

#[tokio::test]
async fn exclude_disable_rules_e4_stops_the_excludes1_drop() {
    let (registry, _rt) = build_registry();
    ingest(&registry).await;

    // Same K82.4/K81.9 pair as exclude_e4_drops_excludes1_conflict_with_chosen, but with E4
    // named in disable_rules: the candidate must now survive (K81.9 is not itself an ancestor
    // of K82.4, so no other rule picks it up once E4 is skipped).
    let result = exclude(
        &registry,
        json!({"candidates": ["K81.9"], "chosen": ["K82.4"], "disable_rules": ["E4"]}),
    )
    .await;
    assert_eq!(result["kept"], json!(["K81.9"]));
    assert!(result["dropped"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn exclude_e5_drops_ancestor_of_chosen() {
    let (registry, _rt) = build_registry();
    ingest(&registry).await;

    // K82 is K82.4's parent category; once K82.4 is chosen, the coarser K82
    // is redundant. (K82 is also non-billable, so this simultaneously
    // demonstrates E4/E5 must run before E1 — see the ordering comment in
    // exclude.rs — or E1 would shadow it.)
    let result = exclude(
        &registry,
        json!({"candidates": ["K82"], "chosen": ["K82.4"]}),
    )
    .await;
    assert_eq!(result["kept"], json!([]));
    assert_eq!(dropped_rule(&result, "K82"), Some("E5"));
}

#[tokio::test]
async fn exclude_e6_drops_outside_mention_chapters() {
    let (registry, _rt) = build_registry();
    ingest(&registry).await;

    // K82.4's chapter is "K00-K95"; a mention_chapters set naming only
    // "I00-I99" must drop it.
    let result = exclude(
        &registry,
        json!({"candidates": ["K82.4"], "mention_chapters": ["I00-I99"]}),
    )
    .await;
    assert_eq!(result["kept"], json!([]));
    assert_eq!(dropped_rule(&result, "K82.4"), Some("E6"));
}

#[tokio::test]
async fn exclude_e6_is_inert_when_mention_chapters_is_empty() {
    let (registry, _rt) = build_registry();
    ingest(&registry).await;

    let result = exclude(&registry, json!({"candidates": ["K82.4"]})).await;
    assert_eq!(result["kept"], json!(["K82.4"]));
    assert!(result["dropped"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn exclude_unknown_code_is_reported_not_silently_dropped() {
    let (registry, _rt) = build_registry();
    ingest(&registry).await;

    // Z99.999999 has no record anywhere in the fixture graph. Before this
    // fix it vanished from both `kept` and `dropped` with no trace; now it
    // must surface in its own `unknown` list, and a resolvable candidate
    // in the same call must still be scored normally alongside it.
    let result = exclude(&registry, json!({"candidates": ["Z99.999999", "K82.4"]})).await;
    assert_eq!(result["kept"], json!(["K82.4"]));
    assert!(result["dropped"].as_array().unwrap().is_empty());
    let unknown = result["unknown"].as_array().unwrap();
    assert_eq!(unknown.len(), 1);
    assert_eq!(unknown[0]["code"], json!("Z99.999999"));
    assert_eq!(unknown[0]["reason"], json!("no record in graph"));
}

/// The pack's own search (`khive_pack_icd10::search`) ranks exactly as
/// `KhiveRuntime::hybrid_search` does on the same store: same ids, same
/// order, same scores, same titles, for every arm's entity type. Queries are
/// the fixture's own term and code names (an exact title, so the exact-title
/// boost is exercised) and their first words (several hits, so the rank
/// arithmetic and the tie-break are).
#[tokio::test]
async fn pack_search_ranks_as_runtime_hybrid_search() {
    use khive_pack_icd10::search;
    use khive_storage::EntityFilter;

    let (registry, runtime) = build_registry();
    ingest(&registry).await;
    let token = runtime
        .authorize(khive_types::Namespace::local())
        .expect("local namespace token");

    let mut queries: Vec<String> = Vec::new();
    for entity_type in ["icd_term", "icd_code", "icd_category"] {
        let entities = runtime
            .list_entities_filtered(
                &token,
                EntityFilter {
                    entity_types: vec![entity_type.to_string()],
                    ..Default::default()
                },
                8,
                0,
            )
            .await
            .expect("fixture entities list");
        assert!(!entities.is_empty(), "fixture has no {entity_type}");
        for e in entities {
            for text in [Some(e.name.clone()), e.description.clone()]
                .into_iter()
                .flatten()
            {
                if let Some(first) = text.split_whitespace().next() {
                    queries.push(first.to_string());
                }
                queries.push(text);
            }
        }
    }
    queries.sort();
    queries.dedup();

    let limit = 20;
    let (mut compared, mut non_empty, mut boosted) = (0usize, 0usize, 0usize);
    for query in &queries {
        let ranked = match search::text_ranked(&runtime, &token, query, limit).await {
            Ok(r) => r,
            // A name the text index cannot parse fails in both paths alike.
            Err(_) => {
                assert!(runtime
                    .hybrid_search(
                        &token,
                        query,
                        None,
                        limit,
                        None,
                        Some("icd_term"),
                        &[],
                        None
                    )
                    .await
                    .is_err());
                continue;
            }
        };
        for entity_type in ["icd_term", "icd_code", "icd_category"] {
            let theirs = runtime
                .hybrid_search(
                    &token,
                    query,
                    None,
                    limit,
                    None,
                    Some(entity_type),
                    &[],
                    None,
                )
                .await
                .expect("hybrid_search");
            let ours = search::keep_typed(&runtime, &token, &ranked, None, entity_type, limit)
                .await
                .expect("keep_typed");
            let shape =
                |id, score: f64, title: &Option<String>| (id, score.to_bits(), title.clone());
            assert_eq!(
                ours.iter()
                    .map(|(h, _)| shape(h.entity_id, h.score.to_f64(), &h.title))
                    .collect::<Vec<_>>(),
                theirs
                    .iter()
                    .map(|h| shape(h.entity_id, h.score.to_f64(), &h.title))
                    .collect::<Vec<_>>(),
                "query {query:?}, type {entity_type}"
            );
            compared += 1;
            non_empty += usize::from(!theirs.is_empty());
            boosted += usize::from(theirs.iter().any(|h| h.score.to_f64() > 0.5));
        }
    }
    assert!(compared > 20, "only {compared} comparisons ran");
    assert!(non_empty > 10, "only {non_empty} comparisons had hits");
    assert!(boosted > 0, "no query exercised the exact-title boost");
}

/// The text index is trigram: a query word under three characters produces no token and drops out of the
/// match, so the digit in "type 2" never constrained it and type 1 codes ranked beside (and above) type 2.
/// A candidate whose description or Index path holds every digit of the mention now comes first.
#[tokio::test]
async fn resolve_ranks_the_candidates_that_carry_the_mentions_digit_first() {
    let (registry, _rt) = build_registry();
    registry
        .dispatch(
            "icd10.ingest",
            json!({
                "tabular_path": fixture("tabular_types.xml"),
                "index_path": fixture("index_types.xml"),
            }),
        )
        .await
        .expect("icd10.ingest dispatches");

    let codes_for = |result: &serde_json::Value| -> Vec<String> {
        result["mentions"][0]["candidates"]
            .as_array()
            .expect("candidates array")
            .iter()
            .map(|c| c["code"].as_str().expect("code").to_string())
            .collect()
    };

    let type_2 = codes_for(
        &resolve(
            &registry,
            json!([{"text": "type 2 diabetes with diabetic chronic kidney disease"}]),
        )
        .await,
    );
    assert_eq!(
        type_2.first().map(String::as_str),
        Some("E11.22"),
        "type 2 mention: E11.22 first; got {type_2:?}"
    );
    let last_type_2 = type_2.iter().rposition(|c| c.starts_with("E11")).unwrap();
    assert!(
        type_2[..=last_type_2].iter().all(|c| c.starts_with("E11")),
        "every type 2 code ahead of every code without the digit; got {type_2:?}"
    );

    let type_1 = codes_for(
        &resolve(
            &registry,
            json!([{"text": "type 1 diabetes with diabetic chronic kidney disease"}]),
        )
        .await,
    );
    assert_eq!(
        type_1.first().map(String::as_str),
        Some("E10.22"),
        "type 1 mention: E10.22 first; got {type_1:?}"
    );

    // No digit in the mention: nothing is demoted, every family is still a candidate.
    let no_digit = codes_for(
        &resolve(
            &registry,
            json!([{"text": "diabetes with diabetic chronic kidney disease"}]),
        )
        .await,
    );
    for code in ["E10.22", "E11.22", "E13.22"] {
        assert!(
            no_digit.iter().any(|c| c == code),
            "{code} missing: {no_digit:?}"
        );
    }
}

/// Same trigram drop for a stage: "stage 3" must not rank stage 1 and stage 2 beside it.
#[tokio::test]
async fn resolve_ranks_the_stage_the_mention_names_first() {
    let (registry, _rt) = build_registry();
    registry
        .dispatch(
            "icd10.ingest",
            json!({
                "tabular_path": fixture("tabular_types.xml"),
                "index_path": fixture("index_types.xml"),
            }),
        )
        .await
        .expect("icd10.ingest dispatches");

    let result = resolve(
        &registry,
        json!([{"text": "chronic kidney disease stage 3"}]),
    )
    .await;
    let codes: Vec<String> = result["mentions"][0]["candidates"]
        .as_array()
        .expect("candidates array")
        .iter()
        .map(|c| c["code"].as_str().expect("code").to_string())
        .collect();
    assert_eq!(
        codes.first().map(String::as_str),
        Some("N18.30"),
        "stage 3 mention: N18.30 first; got {codes:?}"
    );
    for other in ["N18.1", "N18.2", "N18.4"] {
        let pos = codes.iter().position(|c| c == other);
        assert!(
            pos.is_none_or(|p| p > 0),
            "{other} must not lead a stage 3 mention; got {codes:?}"
        );
    }
}
