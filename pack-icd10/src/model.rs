//! In-memory record model produced by the CMS XML parsers (`xml_tabular`,
//! `xml_index`) and consumed by `ingest` to build khive entities and edges.
//!
//! The billable/7th-character expansion logic follows the CMS tabular
//! conventions for 7th-character extensions (`build_code_table`,
//! `_process_diag`, `expand_seven_char`).

use std::collections::BTreeMap;

/// `{extension-char: meaning}`, e.g. `{"A": "initial encounter", ...}`.
pub type SevenChr = BTreeMap<String, String>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Specificity {
    Unspecified,
    OtherSpecified,
    Specified,
}

impl Specificity {
    pub fn as_str(self) -> &'static str {
        match self {
            Specificity::Unspecified => "unspecified",
            Specificity::OtherSpecified => "other_specified",
            Specificity::Specified => "specified",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Laterality {
    Right,
    Left,
    Bilateral,
    Unspecified,
}

impl Laterality {
    pub fn as_str(self) -> &'static str {
        match self {
            Laterality::Right => "right",
            Laterality::Left => "left",
            Laterality::Bilateral => "bilateral",
            Laterality::Unspecified => "unspecified",
        }
    }
}

/// Derive `specificity` from a code's description text: a small, auditable
/// heuristic (an open design question — the CMS tabular XML carries no
/// dedicated specificity axis field to derive this from directly).
pub fn specificity_of(desc: &str) -> Specificity {
    let d = desc.to_lowercase();
    if d.contains("unspecified") {
        Specificity::Unspecified
    } else if d.contains("other specified") || d.contains(" nec") || d.ends_with("nec") {
        Specificity::OtherSpecified
    } else {
        Specificity::Specified
    }
}

/// Derive `laterality` from a code's description text: same status as
/// `specificity_of` — a documented heuristic, not a CMS-provided field.
pub fn laterality_of(desc: &str) -> Option<Laterality> {
    let d = desc.to_lowercase();
    if d.contains("bilateral") {
        Some(Laterality::Bilateral)
    } else if d.contains("right") {
        Some(Laterality::Right)
    } else if d.contains("left") {
        Some(Laterality::Left)
    } else if d.contains("unspecified side") || d.contains("unspecified laterality") {
        Some(Laterality::Unspecified)
    } else {
        None
    }
}

/// `S06.0X0` + `"A"` -> `S06.0X0A`: strip the dot, pad to 6 with `X`, append
/// the extension char, re-dot after the 3rd character.
pub fn expand_seven_char(base: &str, ext_char: &str) -> String {
    let mut stripped: String = base.chars().filter(|&c| c != '.').collect();
    while stripped.chars().count() < 6 {
        stripped.push('X');
    }
    stripped.push_str(ext_char);
    let chars: Vec<char> = stripped.chars().collect();
    let split = 3.min(chars.len());
    let (a, b) = chars.split_at(split);
    format!(
        "{}.{}",
        a.iter().collect::<String>(),
        b.iter().collect::<String>()
    )
}

/// Raw instructional-note text, kept per role for later relation resolution.
#[derive(Debug, Clone, Default)]
pub struct NoteRefs {
    pub inclusion_terms: Vec<String>,
    pub excludes1: Vec<String>,
    pub excludes2: Vec<String>,
    pub code_also: Vec<String>,
    pub code_first: Vec<String>,
    pub use_additional: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct ChapterRec {
    pub number: String,
    pub title: String,
    pub range: String,
}

#[derive(Debug, Clone)]
pub struct BlockRec {
    pub id: String,
    pub title: String,
    pub chapter_range: String,
}

#[derive(Debug, Clone)]
pub struct CategoryRec {
    pub code: String,
    pub desc: String,
    pub block_id: String,
    pub seven_chr_def: Option<SevenChr>,
    /// True exactly when the tabular XML's `<diag>` element for this category
    /// has no nested `<diag>` children and no `sevenChrDef` of its own: the
    /// category IS the leaf, the same rule applied uniformly at every depth
    /// (a childless diag is billable). A category with children, or one whose
    /// own children come
    /// only from expanding its `sevenChrDef` (see `requires_7th`), stays
    /// non-billable.
    pub billable: bool,
    /// True when this category carries its own `sevenChrDef` (whether or not
    /// it also has real nested `<diag>` children) — mirrors `CodeRec::requires_7th`.
    pub requires_7th: bool,
}

#[derive(Debug, Clone)]
pub struct CodeRec {
    pub code: String,
    pub desc: String,
    /// Code of the immediate parent (a category code or another icd_code).
    pub parent_code: String,
    pub parent_is_category: bool,
    pub category_code: String,
    pub block_id: String,
    pub chapter_range: String,
    pub billable: bool,
    pub specificity: Specificity,
    pub laterality: Option<Laterality>,
    pub requires_7th: bool,
    pub notes: NoteRefs,
}

#[derive(Debug, Clone)]
pub struct TermRec {
    /// Main term first, levels joined with " > "; nonessential modifiers kept.
    pub path: String,
    /// This term's own (leaf) title, before joining with its ancestors.
    pub title: String,
    /// `title` without its nonessential modifiers: the words a `see` /
    /// `see also` reference cites this term by (e.g. `Examination`, where
    /// `title` is `Examination (for) (following) (general) (of) (routine)`).
    pub plain_title: String,
    pub letter: String,
    pub level: i32,
    pub nemod: Vec<String>,
    /// Raw pointer, may end with '-'.
    pub code_ptr: Option<String>,
    pub see: Option<String>,
    pub see_also: Vec<String>,
    pub parent_path: Option<String>,
}

#[derive(Debug, Default)]
pub struct TabularParse {
    pub chapters: Vec<ChapterRec>,
    pub blocks: Vec<BlockRec>,
    pub categories: Vec<CategoryRec>,
    pub codes: Vec<CodeRec>,
}

#[derive(Debug, Default)]
pub struct IndexParse {
    pub terms: Vec<TermRec>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expand_seven_char_pads_and_appends() {
        assert_eq!(expand_seven_char("S06.0X0", "A"), "S06.0X0A");
        assert_eq!(expand_seven_char("T81.44", "D"), "T81.44XD");
        // A 3-char base pads to 6 with X before the extension char.
        assert_eq!(expand_seven_char("S06", "S"), "S06.XXXS");
    }

    #[test]
    fn specificity_of_reads_the_description() {
        assert_eq!(
            specificity_of("Cholecystitis, unspecified"),
            Specificity::Unspecified
        );
        assert_eq!(
            specificity_of("Other specified diseases of gallbladder"),
            Specificity::OtherSpecified
        );
        assert_eq!(
            specificity_of("Cholesterolosis of gallbladder"),
            Specificity::Specified
        );
    }

    #[test]
    fn laterality_of_reads_the_description() {
        assert_eq!(
            laterality_of("Unspecified injury of right eye and orbit"),
            Some(Laterality::Right)
        );
        assert_eq!(
            laterality_of("Unspecified injury of left eye and orbit"),
            Some(Laterality::Left)
        );
        assert_eq!(
            laterality_of("Bilateral otitis media"),
            Some(Laterality::Bilateral)
        );
        assert_eq!(laterality_of("Cholesterolosis of gallbladder"), None);
    }
}
