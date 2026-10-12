//! Resolve a code-reference token found in note text (or an Index `code`
//! pointer) to the most specific existing record: a code, the category for
//! a `.-` family, or the block for a range.

use std::collections::HashSet;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefKind {
    Code,
    Category,
    Block,
}

#[derive(Debug, Clone)]
pub struct ResolvedRef {
    pub kind: RefKind,
    pub key: String,
}

/// Extract every parenthesized code-shaped token from a note's text.
/// Handles a single code `(K80.-)`, a range `(A00-A09)`, and a
/// comma-separated list of codes in one group, e.g.
/// `(O03.37, O03.87, O04.87)`.
pub fn extract_code_refs(note_text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut i = 0usize;
    while let Some(rel_start) = note_text[i..].find('(') {
        let abs_start = i + rel_start;
        let Some(rel_end) = note_text[abs_start..].find(')') else {
            break;
        };
        let abs_end = abs_start + rel_end;
        let inner = &note_text[abs_start + 1..abs_end];
        for tok in inner.split(',') {
            let tok = tok.trim();
            if looks_like_code_ref(tok) {
                out.push(tok.to_string());
            }
        }
        i = abs_end + 1;
    }
    out
}

fn looks_like_code_ref(tok: &str) -> bool {
    let mut chars = tok.chars();
    match chars.next() {
        Some(c) if c.is_ascii_uppercase() => {}
        _ => return false,
    }
    let rest: Vec<char> = chars.collect();
    rest.len() >= 2 && rest[0].is_ascii_digit() && rest[1].is_ascii_digit()
}

/// Uppercase, strip, and insert the dot after the 3rd character when
/// missing.
pub fn normalize_code(raw: &str) -> String {
    let s = raw.trim().to_uppercase();
    let chars: Vec<char> = s.chars().collect();
    if chars.len() > 3 && chars[3] != '.' {
        let head: String = chars[..3].iter().collect();
        let tail: String = chars[3..].iter().collect();
        format!("{head}.{tail}")
    } else {
        s
    }
}

fn shrink(code: &str) -> Option<String> {
    if code.len() <= 3 {
        return None;
    }
    let mut c = code.to_string();
    c.pop();
    if c.ends_with('.') {
        c.pop();
    }
    if c.len() < 3 {
        return None;
    }
    Some(c)
}

/// Walk from the most specific normalized form of `token` up toward its
/// category, returning the first level that exists as a code or category.
fn resolve_family(
    token: &str,
    categories: &HashSet<String>,
    codes: &HashSet<String>,
) -> Option<ResolvedRef> {
    let mut candidate = normalize_code(token.trim_end_matches('-').trim_end_matches('.'));
    loop {
        if codes.contains(&candidate) {
            return Some(ResolvedRef {
                kind: RefKind::Code,
                key: candidate,
            });
        }
        if categories.contains(&candidate) {
            return Some(ResolvedRef {
                kind: RefKind::Category,
                key: candidate,
            });
        }
        candidate = shrink(&candidate)?;
    }
}

/// Resolve one extracted token to the most specific existing record.
pub fn resolve_ref(
    token: &str,
    categories: &HashSet<String>,
    codes: &HashSet<String>,
    blocks: &HashSet<String>,
) -> Option<ResolvedRef> {
    let token = token.trim();
    if let Some((a, b)) = token.split_once('-') {
        if b.is_empty() {
            return resolve_family(a, categories, codes);
        }
        if looks_like_code_ref(a) && looks_like_code_ref(b) {
            let block_id = format!("{a}-{b}");
            if blocks.contains(&block_id) {
                return Some(ResolvedRef {
                    kind: RefKind::Block,
                    key: block_id,
                });
            }
            return resolve_family(a, categories, codes);
        }
        None
    } else {
        resolve_family(token, categories, codes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sets() -> (HashSet<String>, HashSet<String>, HashSet<String>) {
        let categories: HashSet<String> = ["K80", "K81", "K82"].map(String::from).into();
        let codes: HashSet<String> = ["K80.0", "K81.9", "K82.0", "K82.4"]
            .map(String::from)
            .into();
        let blocks: HashSet<String> = ["K80-K87"].map(String::from).into();
        (categories, codes, blocks)
    }

    #[test]
    fn a_plain_code_resolves_to_the_code() {
        let (categories, codes, blocks) = sets();
        let r = resolve_ref("K82.4", &categories, &codes, &blocks).expect("resolves");
        assert_eq!(r.kind, RefKind::Code);
        assert_eq!(r.key, "K82.4");
    }

    #[test]
    fn a_family_ref_resolves_to_the_category() {
        let (categories, codes, blocks) = sets();
        let r = resolve_ref("K80.-", &categories, &codes, &blocks).expect("resolves");
        assert_eq!(r.kind, RefKind::Category);
        assert_eq!(r.key, "K80");
    }

    #[test]
    fn a_bare_category_family_ref_resolves_to_the_category() {
        let (categories, codes, blocks) = sets();
        let r = resolve_ref("K81-", &categories, &codes, &blocks).expect("resolves");
        assert_eq!(r.kind, RefKind::Category);
        assert_eq!(r.key, "K81");
    }

    #[test]
    fn a_range_ref_resolves_to_the_block() {
        let (categories, codes, blocks) = sets();
        let r = resolve_ref("K80-K87", &categories, &codes, &blocks).expect("resolves");
        assert_eq!(r.kind, RefKind::Block);
        assert_eq!(r.key, "K80-K87");
    }

    #[test]
    fn an_unknown_range_falls_back_to_the_first_endpoint_family() {
        // A range whose block id isn't in the known set (e.g. a chapter
        // range like the note text sometimes carries) still resolves via
        // the first endpoint's own family walk.
        let (categories, codes, blocks) = sets();
        let r = resolve_ref("K80-K85", &categories, &codes, &blocks).expect("resolves");
        assert_eq!(r.kind, RefKind::Category);
        assert_eq!(r.key, "K80");
    }

    #[test]
    fn normalize_code_inserts_the_dot() {
        assert_eq!(normalize_code("k824"), "K82.4");
        assert_eq!(normalize_code("K82.4"), "K82.4");
        assert_eq!(normalize_code(" k82 "), "K82");
    }

    #[test]
    fn extract_code_refs_handles_family_range_and_list() {
        let refs = extract_code_refs(
            "cholesterolosis of gallbladder with cholelithiasis (K80.-), a range (A00-A09), and a list (O03.37, O03.87)",
        );
        assert_eq!(refs, vec!["K80.-", "A00-A09", "O03.37", "O03.87"]);
    }
}
