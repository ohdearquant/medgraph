//! Handler table, inventory registration, and runtime dispatch for the
//! `icd10` pack.

use async_trait::async_trait;
use serde_json::Value;

use khive_runtime::pack::PackRuntime;
use khive_runtime::{KhiveRuntime, NamespaceToken, RuntimeError, VerbRegistry};
use khive_types::{
    EdgeEndpointRule, EntityTypeDef, HandlerDef, IdResolutionMode, ParamDef, Visibility,
};

use crate::vocab::{EDGE_RULES, ENTITY_TYPES};
use crate::{
    ann, check, embed, evidence, exclude, ingest, ingest_external, memo, neighborhood, resolve,
    verify,
};
use crate::{Icd10Pack, PACK_NAME};

pub(crate) static ICD10_HANDLERS: [HandlerDef; 10] = [
    HandlerDef {
        name: "icd10.ingest",
        description: "Parse the CMS ICD-10-CM tabular + Alphabetic Index XML files and write \
            icd_chapter/icd_block/icd_category/icd_code/icd_term entities and their relations.",
        visibility: Visibility::Verb,
        category: khive_types::VerbCategory::Commissive,
        params: &[
            ParamDef {
                name: "tabular_path",
                param_type: "string",
                required: true,
                description: "Path to the CMS tabular XML file (e.g. cms2026_tabular.xml).",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "index_path",
                param_type: "string",
                required: true,
                description:
                    "Path to the CMS Alphabetic Index XML file (e.g. icd10cm_index_2026.xml).",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "dry_run",
                param_type: "boolean",
                required: false,
                description: "Parse and count without writing (default false).",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
        ],
    },
    HandlerDef {
        name: "icd10.neighborhood",
        description: "Parent, children, siblings (with the axis they differ on), excludes1/2, \
            code-also, code-first/use-additional partners, and Index terms pointing at a code.",
        visibility: Visibility::Verb,
        category: khive_types::VerbCategory::Assertive,
        params: &[ParamDef {
            name: "code",
            param_type: "string",
            required: true,
            description: "An icd_code or icd_category code, e.g. \"K82.4\".",
            resolution_mode: IdResolutionMode::NotApplicable,
        }],
    },
    HandlerDef {
        name: "icd10.check",
        description: "Logical checker over the ICD-10-CM graph (R1-R7): unknown, not-billable \
            repair, ancestor redundancy, excludes1 conflict, sex, age, missing 7th character.",
        visibility: Visibility::Verb,
        category: khive_types::VerbCategory::Assertive,
        params: &[
            ParamDef {
                name: "codes",
                param_type: "array of string",
                required: true,
                description: "Predicted ICD-10-CM codes to check.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "sex",
                param_type: "string",
                required: false,
                description: "Patient sex (\"M\"/\"F\"), for the R5 sex rule.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "age_years",
                param_type: "number",
                required: false,
                description: "Patient age in years, for the R6 age rule.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
        ],
    },
    HandlerDef {
        name: "icd10.resolve",
        description: "Candidate ICD-10-CM codes for free-text problem mentions: Index term \
            search, tabular description search, an optional vector arm (the mention's nearest \
            Index terms, codes and categories by embedding), and family expansion. No \
            language-model calls.",
        visibility: Visibility::Verb,
        category: khive_types::VerbCategory::Assertive,
        params: &[
            ParamDef {
                name: "mentions",
                param_type: "array of object",
                required: true,
                description: "List of {text, evidence?} problem mentions to resolve.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "sex",
                param_type: "string",
                required: false,
                description: "Patient sex; accepted for signature parity, not applied here \
                    (icd10.exclude applies sex/age).",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "age_years",
                param_type: "number",
                required: false,
                description: "Patient age in years; accepted for signature parity, not applied \
                    here (icd10.exclude applies sex/age).",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "per_arm_limit",
                param_type: "integer",
                required: false,
                description: "Max hits fetched per search arm (default 20).",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "expand_family",
                param_type: "boolean",
                required: false,
                description: "Add each hit code's category's billable siblings (default true).",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "text_mode",
                param_type: "string",
                required: false,
                description: "\"all_terms\" (default; implicit AND over every mention word) or \
                    \"any_term\" (OR-join; a term matching any one word can surface). Applies to \
                    both the index and tabular arms.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "text_arms",
                param_type: "boolean",
                required: false,
                description: "Run the index and tabular text arms (default true). False with \
                    dense true leaves the vector arm alone.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "dense",
                param_type: "boolean",
                required: false,
                description: "Run the vector arm (default false): the mention embedded once, \
                    then its nearest icd_term, icd_code and billable icd_category entities, \
                    per_arm_limit each. Needs icd10.embed and icd10.index.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "dense_mode",
                param_type: "string",
                required: false,
                description: "\"exact\" (default; every vector of the family) or \"ann\" \
                    (the Vamana graph).",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
        ],
    },
    HandlerDef {
        name: "icd10.exclude",
        description: "Logical exclusion over resolved candidates (E1-E6): non-billable, sex, \
            age, Excludes1 against chosen codes, ancestor-of-chosen, and out-of-chapter. No \
            model calls.",
        visibility: Visibility::Verb,
        category: khive_types::VerbCategory::Assertive,
        params: &[
            ParamDef {
                name: "candidates",
                param_type: "array of string",
                required: true,
                description: "Candidate ICD-10-CM codes to check.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "sex",
                param_type: "string",
                required: false,
                description: "Patient sex (\"M\"/\"F\"), for the E2 sex rule.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "age_years",
                param_type: "number",
                required: false,
                description: "Patient age in years, for the E3 age rule.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "chosen",
                param_type: "array of string",
                required: false,
                description: "Codes already selected, for the E4/E5 rules.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "mention_chapters",
                param_type: "array of string",
                required: false,
                description: "Chapter ranges the mention's index-arm hits reached, for E6. \
                    E6 is inert when this is empty.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "disable_rules",
                param_type: "array of string",
                required: false,
                description: "Rule names (e.g. \"E4\") to skip entirely, as though that check \
                    never ran. Debug/measurement knob for isolating one rule's effect; a \
                    disabled rule's candidates fall through to the next rule in order, not \
                    guaranteed kept. Empty (the default) runs every rule.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
        ],
    },
    HandlerDef {
        name: "icd10.verify",
        description: "The graph checks a model's ICD-10-CM hypotheses: per hypothesis, a \
            deterministic verdict (CONFIRMED / REVISE / REJECT / NEEDS_COMPANION / UNSUPPORTED) \
            from eight facets — reachable (Index/Tabular text paths from the mention to the \
            code), axis (which sibling the quote settles), status (history/negated/uncertain \
            language), conflicts (R1-R7 over the hypothesis set, and Excludes1), obligations \
            (code_first/use_additional), support (medication-path evidence, which never alone \
            confirms a hypothesis), guideline (combination-rule matches reported as `proposals` \
            that never change a verdict, plus reporting_condition rules that can REJECT), and \
            coherence (whether the code itself carries a CCSR category). No model calls.",
        visibility: Visibility::Verb,
        category: khive_types::VerbCategory::Assertive,
        params: &[
            ParamDef {
                name: "hypotheses",
                param_type: "array of object",
                required: true,
                description: "List of {code, mention: {type?, name, quote, status?}} — the \
                    model's candidate ICD-10-CM codes, each with the mention it came from.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "mentions",
                param_type: "array of object",
                required: false,
                description: "Every mention of the visit (same shape as a hypothesis's own \
                    mention), used to decide whether an unmet use_additional obligation is \
                    supported by some other mention in the visit.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "sex",
                param_type: "string",
                required: false,
                description: "Patient sex (\"M\"/\"F\"), for the R5 sex rule.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "age_years",
                param_type: "number",
                required: false,
                description: "Patient age in years, for the R6 age rule.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "per_arm_limit",
                param_type: "integer",
                required: false,
                description: "Max hits per search arm when each mention's name and quote are \
                    resolved for reachability (default 20, the icd10.resolve default). Smaller = \
                    fewer candidates walked, so a faster verdict and a narrower `reachable`.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
        ],
    },
    HandlerDef {
        name: "icd10.ingest_external",
        description: "Load MED-RT (drug -> disease), Human Disease Ontology (disease -> \
            ICD10CM xref), AHRQ DXCCSR (code -> clinical category) and CMS-HCC (code -> HCC) \
            into the drug/mesh_disease/ccsr_category/hcc/guideline_rule subtypes. Every source \
            is independently optional; deterministic ids, idempotent re-run.",
        visibility: Visibility::Verb,
        category: khive_types::VerbCategory::Commissive,
        params: &[
            ParamDef {
                name: "medrt_jsonl",
                param_type: "string",
                required: false,
                description: "Path to a MED-RT disease/drug JSONL file, one row per \
                    {mesh_id, mesh_name, rela, rxcui, drug, tty}.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "doid_obo",
                param_type: "string",
                required: false,
                description: "Path to a Human Disease Ontology .obo file.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "ccsr_csv",
                param_type: "string",
                required: false,
                description: "Path to the unzipped AHRQ DXCCSR CSV file.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "hcc_csv",
                param_type: "string",
                required: false,
                description: "Path to the unzipped CMS-HCC ICD-10-CM mappings CSV file.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "rules_jsonl",
                param_type: "string",
                required: false,
                description: "Path to a guideline-rules JSONL file, one row per \
                    {id, section, quote, premises, conclusions} (extra columns ignored).",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
        ],
    },
    HandlerDef {
        name: "icd10.evidence",
        description: "For each requested code, the sourced paths from this visit's mentions to \
            it and a verdict: direct | medication | guideline | category_only | unsupported. \
            Negated and family-history mentions never support anything.",
        visibility: Visibility::Verb,
        category: khive_types::VerbCategory::Assertive,
        params: &[
            ParamDef {
                name: "mentions",
                param_type: "array of object",
                required: true,
                description: "List of {type, name, quote, status?} mentions from this visit.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "codes",
                param_type: "array of string",
                required: true,
                description: "Candidate ICD-10-CM codes to evaluate.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "sex",
                param_type: "string",
                required: false,
                description: "Accepted for signature parity; not applied here (icd10.exclude \
                    applies sex/age).",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "age_years",
                param_type: "number",
                required: false,
                description: "Accepted for signature parity; not applied here (icd10.exclude \
                    applies sex/age).",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
        ],
    },
    HandlerDef {
        name: "icd10.embed",
        description: "Embed one slice of one ICD-10-CM entity type (icd_code, icd_category or \
            icd_term) with the store's default model and write the vectors as the runtime \
            writes an entity's. Entities that already have a vector are skipped, so slices \
            can be repeated.",
        visibility: Visibility::Verb,
        category: khive_types::VerbCategory::Commissive,
        params: &[
            ParamDef {
                name: "entity_type",
                param_type: "string",
                required: true,
                description: "\"icd_code\", \"icd_category\" or \"icd_term\".",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "after",
                param_type: "string",
                required: false,
                description: "Entity id to continue after: the previous slice's next_after \
                    (absent = from the start of the type's list).",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "limit",
                param_type: "integer",
                required: false,
                description: "Entities in the slice (default 20000, at most 50000).",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "batch",
                param_type: "integer",
                required: false,
                description: "Texts per embedding call (default 32, at most 256).",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
        ],
    },
    HandlerDef {
        name: "icd10.index",
        description: "The nearest-neighbour index of an ICD-10-CM entity family (icd_code, \
            icd_category, icd_term): status, build (from the store's vectors, saved beside \
            the database), or parity (the index against an exact scan of the same vectors, \
            for the given queries).",
        visibility: Visibility::Verb,
        category: khive_types::VerbCategory::Commissive,
        params: &[
            ParamDef {
                name: "action",
                param_type: "string",
                required: true,
                description: "\"status\", \"build\" or \"parity\".",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "family",
                param_type: "string",
                required: false,
                description: "The family (required for build and parity; status without it \
                    lists every family).",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "queries",
                param_type: "array of string",
                required: false,
                description: "Texts to search with, for parity.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "k",
                param_type: "integer",
                required: false,
                description: "Neighbours compared per query, for parity (default 10).",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
        ],
    },
];

struct Icd10PackFactory;

impl khive_runtime::PackFactory for Icd10PackFactory {
    fn name(&self) -> &'static str {
        PACK_NAME
    }
    fn requires(&self) -> &'static [&'static str] {
        &["kg"]
    }
    fn create(&self, runtime: KhiveRuntime) -> Box<dyn khive_runtime::PackRuntime> {
        Box::new(Icd10Pack::new(runtime))
    }
}

inventory::submit! { khive_runtime::PackRegistration(&Icd10PackFactory) }

#[async_trait]
impl PackRuntime for Icd10Pack {
    fn name(&self) -> &str {
        <Icd10Pack as khive_types::Pack>::NAME
    }
    fn note_kinds(&self) -> &'static [&'static str] {
        <Icd10Pack as khive_types::Pack>::NOTE_KINDS
    }
    fn entity_kinds(&self) -> &'static [&'static str] {
        <Icd10Pack as khive_types::Pack>::ENTITY_KINDS
    }
    fn handlers(&self) -> &'static [HandlerDef] {
        &ICD10_HANDLERS
    }
    fn edge_rules(&self) -> &'static [EdgeEndpointRule] {
        EDGE_RULES
    }
    fn entity_types(&self) -> &'static [EntityTypeDef] {
        &ENTITY_TYPES
    }
    fn requires(&self) -> &'static [&'static str] {
        <Icd10Pack as khive_types::Pack>::REQUIRES
    }

    async fn dispatch(
        &self,
        verb: &str,
        params: Value,
        _registry: &VerbRegistry,
        token: &NamespaceToken,
    ) -> Result<Value, RuntimeError> {
        match verb {
            "icd10.ingest" => ingest::handle_ingest(self.runtime(), token, params).await,
            "icd10.neighborhood" => {
                memo::scoped(neighborhood::handle_neighborhood(
                    self.runtime(),
                    token,
                    params,
                ))
                .await
            }
            "icd10.check" => memo::scoped(check::handle_check(self.runtime(), token, params)).await,
            "icd10.resolve" => {
                memo::scoped(resolve::handle_resolve(self.runtime(), token, params)).await
            }
            "icd10.exclude" => {
                memo::scoped(exclude::handle_exclude(self.runtime(), token, params)).await
            }
            "icd10.verify" => {
                memo::scoped(verify::handle_verify(self.runtime(), token, params)).await
            }
            "icd10.ingest_external" => {
                ingest_external::handle_ingest_external(self.runtime(), token, params).await
            }
            "icd10.evidence" => {
                memo::scoped(evidence::handle_evidence(self.runtime(), token, params)).await
            }
            "icd10.embed" => embed::handle_embed(self.runtime(), token, params).await,
            "icd10.index" => {
                ann::handle_index(self.runtime(), token, verb, &ann::ICD_FAMILIES, params).await
            }
            _ => Err(RuntimeError::InvalidInput(format!(
                "{PACK_NAME} pack does not handle verb {verb:?}"
            ))),
        }
    }
}
