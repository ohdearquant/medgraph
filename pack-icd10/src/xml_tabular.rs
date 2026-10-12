//! Streaming parser for the CMS ICD-10-CM tabular XML
//! (`data/raw/cms2026_tabular.xml`) into the `model::TabularParse` records.
//!
//! One event-driven pass (`quick_xml::Reader`) — no DOM is ever fully
//! materialized. The walk is recursive over `<diag>` and also collects
//! excludes2/codeFirst/useAdditionalCode/codeAlso notes.

use anyhow::{bail, Context, Result};
use quick_xml::events::{BytesStart, Event};
use quick_xml::name::QName;
use quick_xml::reader::Reader;

use crate::model::{
    expand_seven_char, laterality_of, specificity_of, BlockRec, CategoryRec, ChapterRec, CodeRec,
    NoteRefs, SevenChr, TabularParse,
};

pub fn parse_tabular(xml: &str) -> Result<TabularParse> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    let mut out = TabularParse::default();
    loop {
        match reader.read_event().context("reading tabular XML")? {
            Event::Start(e) if local_name(&e) == "chapter" => {
                parse_chapter(&mut reader, &mut out)?;
            }
            Event::Eof => break,
            _ => {}
        }
    }
    Ok(out)
}

fn local_name(e: &BytesStart) -> String {
    String::from_utf8_lossy(e.local_name().as_ref()).into_owned()
}

fn get_attr(e: &BytesStart, key: &str) -> Result<Option<String>> {
    for attr in e.attributes() {
        let attr = attr?;
        if attr.key.as_ref() == key.as_bytes() {
            return Ok(Some(
                attr.normalized_value(quick_xml::XmlVersion::Implicit1_0)?
                    .into_owned(),
            ));
        }
    }
    Ok(None)
}

fn read_text_content(reader: &mut Reader<&[u8]>, end: QName) -> Result<String> {
    let text = reader.read_text(end)?;
    Ok(text.decode()?.trim().to_string())
}

fn read_notes(reader: &mut Reader<&[u8]>, end: QName) -> Result<Vec<String>> {
    let mut notes = Vec::new();
    loop {
        match reader.read_event()? {
            Event::Start(e) if local_name(&e) == "note" => {
                let text = read_text_content(reader, e.name())?;
                if !text.is_empty() {
                    notes.push(text);
                }
            }
            Event::Start(e) => {
                reader.read_to_end(e.name())?;
            }
            Event::End(e) if e.name() == end => break,
            Event::Eof => bail!("unexpected eof reading notes"),
            _ => {}
        }
    }
    Ok(notes)
}

fn read_seven_chr_def(reader: &mut Reader<&[u8]>, end: QName) -> Result<SevenChr> {
    let mut map = SevenChr::new();
    loop {
        match reader.read_event()? {
            Event::Start(e) if local_name(&e) == "extension" => {
                let ch = get_attr(&e, "char")?.unwrap_or_default();
                let text = read_text_content(reader, e.name())?;
                map.insert(ch, text);
            }
            Event::Start(e) => {
                reader.read_to_end(e.name())?;
            }
            Event::End(e) if e.name() == end => break,
            Event::Eof => bail!("unexpected eof reading sevenChrDef"),
            _ => {}
        }
    }
    Ok(map)
}

/// "Title text (RANGE)" -> (title, range). Used for chapter/section `<desc>`.
fn split_title_range(full: &str) -> (String, String) {
    let full = full.trim();
    if let Some(open) = full.rfind('(') {
        if let Some(stripped) = full.strip_suffix(')') {
            if open < stripped.len() {
                let title = full[..open].trim().to_string();
                let range = full[open + 1..full.len() - 1].trim().to_string();
                return (title, range);
            }
        }
    }
    (full.to_string(), String::new())
}

fn parse_chapter(reader: &mut Reader<&[u8]>, out: &mut TabularParse) -> Result<()> {
    let mut number = String::new();
    let mut title = String::new();
    let mut range = String::new();
    loop {
        match reader.read_event()? {
            Event::Start(e) => match local_name(&e).as_str() {
                "name" => number = read_text_content(reader, e.name())?,
                "desc" => {
                    let full = read_text_content(reader, e.name())?;
                    let (t, r) = split_title_range(&full);
                    title = t;
                    range = r;
                }
                "section" => {
                    let block_id = get_attr(&e, "id")?.unwrap_or_default();
                    parse_section(reader, &block_id, &range, out)?;
                }
                _ => {
                    reader.read_to_end(e.name())?;
                }
            },
            Event::End(e) if e.name().as_ref() == b"chapter" => break,
            Event::Eof => bail!("unexpected eof inside chapter"),
            _ => {}
        }
    }
    out.chapters.push(ChapterRec {
        number,
        title,
        range,
    });
    Ok(())
}

fn parse_section(
    reader: &mut Reader<&[u8]>,
    block_id: &str,
    chapter_range: &str,
    out: &mut TabularParse,
) -> Result<()> {
    let mut title = String::new();
    loop {
        match reader.read_event()? {
            Event::Start(e) => match local_name(&e).as_str() {
                "desc" => {
                    let full = read_text_content(reader, e.name())?;
                    let (t, _r) = split_title_range(&full);
                    title = t;
                }
                "diag" => {
                    parse_category(reader, block_id, chapter_range, out)?;
                }
                _ => {
                    reader.read_to_end(e.name())?;
                }
            },
            Event::End(e) if e.name().as_ref() == b"section" => break,
            Event::Eof => bail!("unexpected eof inside section"),
            _ => {}
        }
    }
    out.blocks.push(BlockRec {
        id: block_id.to_string(),
        title,
        chapter_range: chapter_range.to_string(),
    });
    Ok(())
}

fn parse_category(
    reader: &mut Reader<&[u8]>,
    block_id: &str,
    chapter_range: &str,
    out: &mut TabularParse,
) -> Result<()> {
    let mut code = String::new();
    let mut desc = String::new();
    let mut own_seven: Option<SevenChr> = None;
    let mut had_children = false;
    loop {
        match reader.read_event()? {
            Event::Start(e) => match local_name(&e).as_str() {
                "name" => code = read_text_content(reader, e.name())?,
                "desc" => desc = read_text_content(reader, e.name())?,
                "sevenChrDef" => own_seven = Some(read_seven_chr_def(reader, e.name())?),
                "diag" => {
                    had_children = true;
                    parse_code(
                        reader,
                        &code,
                        block_id,
                        chapter_range,
                        &code,
                        own_seven.as_ref(),
                        out,
                    )?;
                }
                _ => {
                    reader.read_to_end(e.name())?;
                }
            },
            Event::End(e) if e.name().as_ref() == b"diag" => break,
            Event::Eof => bail!("unexpected eof inside category diag {code}"),
            _ => {}
        }
    }

    // A category-level `<diag>` gets the SAME childless-leaf treatment
    // `parse_code` gives every level below it (the walk recurses uniformly and
    // draws no distinction between a category and any other diag depth): no
    // children and no `sevenChrDef`
    // means the category itself is a billable code (e.g. real codes I10,
    // O80, A09); no children but its own `sevenChrDef` means the category is
    // a non-billable 7th-character base, and its extension codes are billable
    // icd_code children of it (e.g. real codes T07, T66, W04 each need their
    // own `.XXXA`/`.XXXD`/`.XXXS`). A category WITH children stays
    // non-billable regardless, exactly as before.
    let billable = !had_children && own_seven.is_none();
    let requires_7th = own_seven.is_some();
    if !had_children {
        if let Some(sc) = &own_seven {
            for (ch, text) in sc.iter() {
                let exp_code = expand_seven_char(&code, ch);
                let exp_desc = format!("{desc}, {text}");
                out.codes.push(CodeRec {
                    code: exp_code,
                    desc: exp_desc.clone(),
                    parent_code: code.clone(),
                    parent_is_category: true,
                    category_code: code.clone(),
                    block_id: block_id.to_string(),
                    chapter_range: chapter_range.to_string(),
                    billable: true,
                    specificity: specificity_of(&exp_desc),
                    laterality: laterality_of(&exp_desc),
                    requires_7th: false,
                    notes: NoteRefs::default(),
                });
            }
        }
    }
    out.categories.push(CategoryRec {
        code,
        desc,
        block_id: block_id.to_string(),
        seven_chr_def: own_seven,
        billable,
        requires_7th,
    });
    Ok(())
}

/// One `<diag>` below a category: an icd_code row, recursing into children.
/// It collects excludes2/codeFirst/useAdditionalCode/codeAlso and emits an
/// icd_code row for every intermediate (non-leaf) node too, since the pack
/// models the full below-category tree, not only billable leaves.
#[allow(clippy::too_many_arguments)]
fn parse_code(
    reader: &mut Reader<&[u8]>,
    category_code: &str,
    block_id: &str,
    chapter_range: &str,
    parent_code: &str,
    inherited_seven: Option<&SevenChr>,
    out: &mut TabularParse,
) -> Result<()> {
    let mut code = String::new();
    let mut desc = String::new();
    let mut notes = NoteRefs::default();
    let mut own_seven: Option<SevenChr> = None;
    let mut had_children = false;

    loop {
        match reader.read_event()? {
            Event::Start(e) => match local_name(&e).as_str() {
                "name" => code = read_text_content(reader, e.name())?,
                "desc" => desc = read_text_content(reader, e.name())?,
                "inclusionTerm" => notes.inclusion_terms = read_notes(reader, e.name())?,
                "excludes1" => notes.excludes1 = read_notes(reader, e.name())?,
                "excludes2" => notes.excludes2 = read_notes(reader, e.name())?,
                "codeFirst" => notes.code_first = read_notes(reader, e.name())?,
                "useAdditionalCode" => notes.use_additional = read_notes(reader, e.name())?,
                "codeAlso" => notes.code_also = read_notes(reader, e.name())?,
                "sevenChrDef" => own_seven = Some(read_seven_chr_def(reader, e.name())?),
                "diag" => {
                    had_children = true;
                    let effective = own_seven.as_ref().or(inherited_seven);
                    parse_code(
                        reader,
                        category_code,
                        block_id,
                        chapter_range,
                        &code,
                        effective,
                        out,
                    )?;
                }
                _ => {
                    reader.read_to_end(e.name())?;
                }
            },
            Event::End(e) if e.name().as_ref() == b"diag" => break,
            Event::Eof => bail!("unexpected eof inside diag {code}"),
            _ => {}
        }
    }

    let effective_seven = own_seven.as_ref().or(inherited_seven);
    if had_children {
        out.codes.push(CodeRec {
            code: code.clone(),
            desc: desc.clone(),
            parent_code: parent_code.to_string(),
            parent_is_category: parent_code == category_code,
            category_code: category_code.to_string(),
            block_id: block_id.to_string(),
            chapter_range: chapter_range.to_string(),
            billable: false,
            specificity: specificity_of(&desc),
            laterality: laterality_of(&desc),
            requires_7th: effective_seven.is_some(),
            notes,
        });
    } else if let Some(sc) = effective_seven {
        out.codes.push(CodeRec {
            code: code.clone(),
            desc: desc.clone(),
            parent_code: parent_code.to_string(),
            parent_is_category: parent_code == category_code,
            category_code: category_code.to_string(),
            block_id: block_id.to_string(),
            chapter_range: chapter_range.to_string(),
            billable: false,
            specificity: specificity_of(&desc),
            laterality: laterality_of(&desc),
            requires_7th: true,
            notes,
        });
        for (ch, text) in sc.iter() {
            let exp_code = expand_seven_char(&code, ch);
            let exp_desc = format!("{desc}, {text}");
            out.codes.push(CodeRec {
                code: exp_code,
                desc: exp_desc.clone(),
                parent_code: code.clone(),
                parent_is_category: false,
                category_code: category_code.to_string(),
                block_id: block_id.to_string(),
                chapter_range: chapter_range.to_string(),
                billable: true,
                specificity: specificity_of(&exp_desc),
                laterality: laterality_of(&exp_desc),
                requires_7th: false,
                notes: NoteRefs::default(),
            });
        }
    } else {
        out.codes.push(CodeRec {
            code: code.clone(),
            desc: desc.clone(),
            parent_code: parent_code.to_string(),
            parent_is_category: parent_code == category_code,
            category_code: category_code.to_string(),
            block_id: block_id.to_string(),
            chapter_range: chapter_range.to_string(),
            billable: true,
            specificity: specificity_of(&desc),
            laterality: laterality_of(&desc),
            requires_7th: false,
            notes,
        });
    }
    Ok(())
}
