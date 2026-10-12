//! Streaming parser for the CMS ICD-10-CM Alphabetic Index XML
//! (`data/raw/icd10cm_index_2026.xml`) into `model::TermRec` rows.

use anyhow::{bail, Context, Result};
use quick_xml::events::{BytesStart, Event};
use quick_xml::name::QName;
use quick_xml::reader::Reader;

use crate::model::{IndexParse, TermRec};

pub fn parse_index(xml: &str) -> Result<IndexParse> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    let mut out = IndexParse::default();
    loop {
        match reader.read_event().context("reading index XML")? {
            Event::Start(e) if local_name(&e) == "letter" => {
                parse_letter(&mut reader, &mut out)?;
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

/// `<title>` may hold mixed content: raw text plus a nested `<nemod>` element
/// (e.g. `Abasia<nemod>(-astasia) (hysterical)</nemod>`). The nonessential
/// modifier text is appended to the rendered title (it is kept in the
/// name) and also returned separately for the `nemod` property list. The
/// third value is the title's own text without the modifiers, which is how
/// `see` / `see also` references name a term.
fn read_title(reader: &mut Reader<&[u8]>, end: QName) -> Result<(String, Vec<String>, String)> {
    let mut title = String::new();
    let mut nemods = Vec::new();
    let mut plain = String::new();
    loop {
        match reader.read_event()? {
            Event::Text(t) => {
                let text = t.decode()?;
                title.push_str(&text);
                plain.push(' ');
                plain.push_str(&text);
            }
            Event::Start(e) if local_name(&e) == "nemod" => {
                let text = read_text_content(reader, e.name())?;
                if !text.is_empty() {
                    if !title.is_empty() && !title.ends_with(' ') {
                        title.push(' ');
                    }
                    title.push_str(&text);
                    nemods.push(text);
                }
            }
            Event::Start(e) => {
                reader.read_to_end(e.name())?;
            }
            Event::End(e) if e.name() == end => break,
            Event::Eof => bail!("unexpected eof reading title"),
            _ => {}
        }
    }
    let plain = plain.split_whitespace().collect::<Vec<_>>().join(" ");
    Ok((title.trim().to_string(), nemods, plain))
}

fn parse_letter(reader: &mut Reader<&[u8]>, out: &mut IndexParse) -> Result<()> {
    let mut letter = String::new();
    loop {
        match reader.read_event()? {
            Event::Start(e) => match local_name(&e).as_str() {
                "title" => letter = read_text_content(reader, e.name())?,
                "mainTerm" => {
                    parse_term(reader, e.name(), 0, &letter, None, out)?;
                }
                _ => {
                    reader.read_to_end(e.name())?;
                }
            },
            Event::End(e) if e.name().as_ref() == b"letter" => break,
            Event::Eof => bail!("unexpected eof inside letter {letter}"),
            _ => {}
        }
    }
    Ok(())
}

fn parse_term(
    reader: &mut Reader<&[u8]>,
    end: QName,
    level: i32,
    letter: &str,
    parent_path: Option<&str>,
    out: &mut IndexParse,
) -> Result<()> {
    let mut own_title = String::new();
    let mut plain_title = String::new();
    let mut nemod = Vec::new();
    let mut code_ptr: Option<String> = None;
    let mut see: Option<String> = None;
    let mut see_also = Vec::new();

    loop {
        match reader.read_event()? {
            Event::Start(e) => match local_name(&e).as_str() {
                "title" => {
                    let (t, n, plain) = read_title(reader, e.name())?;
                    own_title = t;
                    nemod = n;
                    plain_title = plain;
                }
                "code" => code_ptr = Some(read_text_content(reader, e.name())?),
                "see" => see = Some(read_text_content(reader, e.name())?),
                "seeAlso" => see_also.push(read_text_content(reader, e.name())?),
                "term" => {
                    let full_path = match parent_path {
                        Some(p) => format!("{p} > {own_title}"),
                        None => own_title.clone(),
                    };
                    let child_level = get_attr(&e, "level")?
                        .and_then(|s| s.parse::<i32>().ok())
                        .unwrap_or(level + 1);
                    parse_term(reader, e.name(), child_level, letter, Some(&full_path), out)?;
                }
                _ => {
                    reader.read_to_end(e.name())?;
                }
            },
            Event::End(e) if e.name() == end => break,
            Event::Eof => bail!("unexpected eof inside term {own_title}"),
            _ => {}
        }
    }

    let full_path = match parent_path {
        Some(p) => format!("{p} > {own_title}"),
        None => own_title.clone(),
    };
    out.terms.push(TermRec {
        path: full_path,
        title: own_title,
        plain_title,
        letter: letter.to_string(),
        level,
        nemod,
        code_ptr,
        see,
        see_also,
        parent_path: parent_path.map(|s| s.to_string()),
    });
    Ok(())
}
