//! Entity subtypes and edge endpoint rules for the `icd10` pack.

use khive_types::{EdgeEndpointRule, EdgeRelation, EndpointKind, EntityKind, EntityTypeDef};

macro_rules! icd_ep {
    ($t:literal) => {
        EndpointKind::EntityOfType {
            kind: "concept",
            entity_type: $t,
        }
    };
}

pub const ENTITY_TYPES: [EntityTypeDef; 10] = [
    EntityTypeDef {
        kind: EntityKind::Concept,
        type_name: "icd_chapter",
        aliases: &[],
    },
    EntityTypeDef {
        kind: EntityKind::Concept,
        type_name: "icd_block",
        aliases: &[],
    },
    EntityTypeDef {
        kind: EntityKind::Concept,
        type_name: "icd_category",
        aliases: &[],
    },
    EntityTypeDef {
        kind: EntityKind::Concept,
        type_name: "icd_code",
        aliases: &[],
    },
    EntityTypeDef {
        kind: EntityKind::Concept,
        type_name: "icd_term",
        aliases: &[],
    },
    // Medical-graph additions: drug / symptom / condition relations sourced from
    // MED-RT, DOID, AHRQ DXCCSR
    // and CMS-HCC, plus guideline rules extracted from the FY2026 Guidelines
    // PDF. Every one of these is `kind: concept` per the base entity
    // registry invariant, so the base entity endpoint allowlist (which keys
    // on the coarse `concept` kind, not `entity_type`) already accepts the
    // relations this pack uses between them; `EDGE_RULES` below still
    // declares the specific `entity_type` pairs explicitly, matching this
    // file's existing pattern for every other subtype pair.
    EntityTypeDef {
        kind: EntityKind::Concept,
        type_name: "drug",
        aliases: &[],
    },
    EntityTypeDef {
        kind: EntityKind::Concept,
        type_name: "mesh_disease",
        aliases: &[],
    },
    EntityTypeDef {
        kind: EntityKind::Concept,
        type_name: "ccsr_category",
        aliases: &[],
    },
    EntityTypeDef {
        kind: EntityKind::Concept,
        type_name: "hcc",
        aliases: &[],
    },
    EntityTypeDef {
        kind: EntityKind::Concept,
        type_name: "guideline_rule",
        aliases: &[],
    },
];

/// Additive endpoint rules for every (subtype, relation, subtype) triple the
/// pack uses. `competes_with`/`composed_with` are symmetric relations in
/// the base edge contract; rules are declared in both directions so
/// endpoint validation accepts either stored order.
pub const EDGE_RULES: &[EdgeEndpointRule] = &[
    // hierarchy: chapter ⊃ block ⊃ category ⊃ code ⊃ code(7th)
    EdgeEndpointRule {
        relation: EdgeRelation::Contains,
        source: icd_ep!("icd_chapter"),
        target: icd_ep!("icd_block"),
    },
    EdgeEndpointRule {
        relation: EdgeRelation::Contains,
        source: icd_ep!("icd_block"),
        target: icd_ep!("icd_category"),
    },
    EdgeEndpointRule {
        relation: EdgeRelation::Contains,
        source: icd_ep!("icd_category"),
        target: icd_ep!("icd_code"),
    },
    EdgeEndpointRule {
        relation: EdgeRelation::Contains,
        source: icd_ep!("icd_code"),
        target: icd_ep!("icd_code"),
    },
    // Index subterm hierarchy: term -> subterm.
    EdgeEndpointRule {
        relation: EdgeRelation::Contains,
        source: icd_ep!("icd_term"),
        target: icd_ep!("icd_term"),
    },
    // Excludes1: this <-> referenced (code or category).
    EdgeEndpointRule {
        relation: EdgeRelation::CompetesWith,
        source: icd_ep!("icd_code"),
        target: icd_ep!("icd_code"),
    },
    EdgeEndpointRule {
        relation: EdgeRelation::CompetesWith,
        source: icd_ep!("icd_code"),
        target: icd_ep!("icd_category"),
    },
    EdgeEndpointRule {
        relation: EdgeRelation::CompetesWith,
        source: icd_ep!("icd_category"),
        target: icd_ep!("icd_code"),
    },
    EdgeEndpointRule {
        relation: EdgeRelation::CompetesWith,
        source: icd_ep!("icd_category"),
        target: icd_ep!("icd_category"),
    },
    // Excludes2 / Code also: this <-> referenced.
    EdgeEndpointRule {
        relation: EdgeRelation::ComposedWith,
        source: icd_ep!("icd_code"),
        target: icd_ep!("icd_code"),
    },
    EdgeEndpointRule {
        relation: EdgeRelation::ComposedWith,
        source: icd_ep!("icd_code"),
        target: icd_ep!("icd_category"),
    },
    EdgeEndpointRule {
        relation: EdgeRelation::ComposedWith,
        source: icd_ep!("icd_category"),
        target: icd_ep!("icd_code"),
    },
    EdgeEndpointRule {
        relation: EdgeRelation::ComposedWith,
        source: icd_ep!("icd_category"),
        target: icd_ep!("icd_category"),
    },
    // Code first (etiology -> this) / Use additional code (this -> additional).
    EdgeEndpointRule {
        relation: EdgeRelation::Precedes,
        source: icd_ep!("icd_code"),
        target: icd_ep!("icd_code"),
    },
    EdgeEndpointRule {
        relation: EdgeRelation::Precedes,
        source: icd_ep!("icd_code"),
        target: icd_ep!("icd_category"),
    },
    EdgeEndpointRule {
        relation: EdgeRelation::Precedes,
        source: icd_ep!("icd_category"),
        target: icd_ep!("icd_code"),
    },
    // Index term -> code it points to.
    EdgeEndpointRule {
        relation: EdgeRelation::VariantOf,
        source: icd_ep!("icd_term"),
        target: icd_ep!("icd_code"),
    },
    EdgeEndpointRule {
        relation: EdgeRelation::VariantOf,
        source: icd_ep!("icd_term"),
        target: icd_ep!("icd_category"),
    },
    // Index see / see also: term -> term.
    EdgeEndpointRule {
        relation: EdgeRelation::LinksTo,
        source: icd_ep!("icd_term"),
        target: icd_ep!("icd_term"),
    },
    // ── Medical-graph additions ───────────────────────────────────────────
    // drug -[enables]-> mesh_disease (MED-RT may_treat / may_prevent).
    EdgeEndpointRule {
        relation: EdgeRelation::Enables,
        source: icd_ep!("drug"),
        target: icd_ep!("mesh_disease"),
    },
    // mesh_disease -[variant_of]-> icd_category / icd_code (DOID ICD10CM xref).
    EdgeEndpointRule {
        relation: EdgeRelation::VariantOf,
        source: icd_ep!("mesh_disease"),
        target: icd_ep!("icd_category"),
    },
    EdgeEndpointRule {
        relation: EdgeRelation::VariantOf,
        source: icd_ep!("mesh_disease"),
        target: icd_ep!("icd_code"),
    },
    // icd_code / icd_category -[part_of]-> ccsr_category (AHRQ DXCCSR).
    EdgeEndpointRule {
        relation: EdgeRelation::PartOf,
        source: icd_ep!("icd_code"),
        target: icd_ep!("ccsr_category"),
    },
    EdgeEndpointRule {
        relation: EdgeRelation::PartOf,
        source: icd_ep!("icd_category"),
        target: icd_ep!("ccsr_category"),
    },
    // icd_code -[part_of]-> hcc (CMS-HCC 2026 midyear final).
    EdgeEndpointRule {
        relation: EdgeRelation::PartOf,
        source: icd_ep!("icd_code"),
        target: icd_ep!("hcc"),
    },
    // guideline_rule -[depends_on]-> premise code/category.
    EdgeEndpointRule {
        relation: EdgeRelation::DependsOn,
        source: icd_ep!("guideline_rule"),
        target: icd_ep!("icd_code"),
    },
    EdgeEndpointRule {
        relation: EdgeRelation::DependsOn,
        source: icd_ep!("guideline_rule"),
        target: icd_ep!("icd_category"),
    },
    // guideline_rule -[enables]-> conclusion code/category.
    EdgeEndpointRule {
        relation: EdgeRelation::Enables,
        source: icd_ep!("guideline_rule"),
        target: icd_ep!("icd_code"),
    },
    EdgeEndpointRule {
        relation: EdgeRelation::Enables,
        source: icd_ep!("guideline_rule"),
        target: icd_ep!("icd_category"),
    },
];
