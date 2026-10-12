//! `icd10` — ICD-10-CM FY2026 as a khive pack: ingest, neighborhood, check,
//! resolve (candidate retrieval), exclude (logical exclusion), evidence
//! (sourced paths to a code), ingest_external (the medical-graph sources),
//! and verify (deterministic verdicts on model hypotheses), all built on the
//! same data model.

pub mod ann;
pub mod check;
pub mod csv_lite;
pub mod embed;
pub mod evidence;
pub mod exclude;
mod handlers;
pub mod ingest;
pub mod ingest_external;
mod memo;
pub mod model;
pub mod neighborhood;
pub mod refs;
pub mod resolve;
pub mod search;
pub mod verify;
pub mod vocab;
pub mod xml_index;
pub mod xml_tabular;

use khive_runtime::KhiveRuntime;
use khive_types::{EdgeEndpointRule, EntityTypeDef, HandlerDef, Pack};

pub(crate) use handlers::ICD10_HANDLERS;

pub(crate) const PACK_NAME: &str = "icd10";

/// The `icd10` pack: ICD-10-CM concept subtypes, endpoint rules, and the
/// ingest/neighborhood/check verbs.
pub struct Icd10Pack {
    runtime: KhiveRuntime,
}

impl Pack for Icd10Pack {
    const NAME: &'static str = PACK_NAME;
    const NOTE_KINDS: &'static [&'static str] = &[];
    const ENTITY_KINDS: &'static [&'static str] = &[];
    const HANDLERS: &'static [HandlerDef] = &ICD10_HANDLERS;
    const EDGE_RULES: &'static [EdgeEndpointRule] = vocab::EDGE_RULES;
    const ENTITY_TYPES: &'static [EntityTypeDef] = &vocab::ENTITY_TYPES;
    const REQUIRES: &'static [&'static str] = &["kg"];
}

impl Icd10Pack {
    pub fn new(runtime: KhiveRuntime) -> Self {
        Self { runtime }
    }
    pub(crate) fn runtime(&self) -> &KhiveRuntime {
        &self.runtime
    }
}
