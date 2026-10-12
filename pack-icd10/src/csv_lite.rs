//! Minimal RFC4180 CSV reader for the two flat-file sources
//! `icd10.ingest_external` reads (AHRQ DXCCSR, CMS-HCC): comma-delimited,
//! double-quote field quoting with `""` as the escaped-quote sequence, and
//! quoted fields allowed to carry embedded newlines (the CMS-HCC file's own
//! header wraps each column name across several lines inside a quoted
//! field). No external crate: both files are small, and a hand-rolled state
//! machine keeps the pack's dependency set unchanged.
//!
//! Neither source uses single quotes as CSV quoting — DXCCSR wraps plain
//! code/category tokens in a literal leading/trailing `'` character (e.g.
//! `'A000'`) that is not a CSV quote at all, just part of the field text;
//! callers strip that with `strip_single_quotes`.

/// Parse whole CSV file content into rows of fields. Trailing blank line (if
/// any) yields no trailing empty row. Never returns an error: a truncated
/// quoted field at EOF is simply closed at end-of-input.
pub fn parse_csv(content: &str) -> Vec<Vec<String>> {
    let mut rows: Vec<Vec<String>> = Vec::new();
    let mut row: Vec<String> = Vec::new();
    let mut field = String::new();
    let mut in_quotes = false;
    let mut chars = content.chars().peekable();
    let mut row_has_content = false;

    while let Some(c) = chars.next() {
        if in_quotes {
            if c == '"' {
                if chars.peek() == Some(&'"') {
                    field.push('"');
                    chars.next();
                } else {
                    in_quotes = false;
                }
            } else {
                field.push(c);
            }
            continue;
        }
        match c {
            '"' => {
                in_quotes = true;
                row_has_content = true;
            }
            ',' => {
                row.push(std::mem::take(&mut field));
                row_has_content = true;
            }
            '\r' => {
                // Swallow bare CR; a following \n (CRLF) is handled by the \n arm.
            }
            '\n' => {
                if row_has_content || !field.is_empty() || !row.is_empty() {
                    row.push(std::mem::take(&mut field));
                    rows.push(std::mem::take(&mut row));
                }
                row_has_content = false;
            }
            _ => {
                field.push(c);
                row_has_content = true;
            }
        }
    }
    if row_has_content || !field.is_empty() || !row.is_empty() {
        row.push(field);
        rows.push(row);
    }
    rows
}

/// Strip a leading and trailing literal `'` from a field's text (DXCCSR's
/// own non-CSV quoting convention), then trim ASCII whitespace. `' '`
/// (a lone space wrapped in single quotes — DXCCSR's "no category here"
/// marker) normalizes to an empty string like a genuinely blank field.
pub fn strip_single_quotes(field: &str) -> String {
    let f = field.trim();
    let f = f.strip_prefix('\'').unwrap_or(f);
    let f = f.strip_suffix('\'').unwrap_or(f);
    f.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_plain_comma_rows() {
        let rows = parse_csv("a,b,c\n1,2,3\n");
        assert_eq!(rows, vec![vec!["a", "b", "c"], vec!["1", "2", "3"]]);
    }

    #[test]
    fn parses_quoted_field_with_embedded_comma() {
        let rows = parse_csv("'A000',\"Cholera, biovar cholerae\",'DIG001'\n");
        assert_eq!(
            rows,
            vec![vec!["'A000'", "Cholera, biovar cholerae", "'DIG001'"]]
        );
    }

    #[test]
    fn parses_quoted_field_with_embedded_newline() {
        let rows = parse_csv("\"Diagnosis\nCode\",Description\nA01,foo\n");
        assert_eq!(
            rows,
            vec![vec!["Diagnosis\nCode", "Description"], vec!["A01", "foo"]]
        );
    }

    #[test]
    fn parses_doubled_quote_as_escaped_quote() {
        let rows = parse_csv("\"she said \"\"hi\"\"\",b\n");
        assert_eq!(rows, vec![vec!["she said \"hi\"", "b"]]);
    }

    #[test]
    fn handles_missing_trailing_newline() {
        let rows = parse_csv("a,b\nc,d");
        assert_eq!(rows, vec![vec!["a", "b"], vec!["c", "d"]]);
    }

    #[test]
    fn handles_crlf_line_endings() {
        let rows = parse_csv("a,b\r\nc,d\r\n");
        assert_eq!(rows, vec![vec!["a", "b"], vec!["c", "d"]]);
    }

    #[test]
    fn strip_single_quotes_removes_wrapping_quote_and_trims() {
        assert_eq!(strip_single_quotes("'DIG001'"), "DIG001");
        assert_eq!(strip_single_quotes(" ' '"), "");
        assert_eq!(strip_single_quotes(""), "");
        assert_eq!(
            strip_single_quotes("Intestinal infection"),
            "Intestinal infection"
        );
    }
}
