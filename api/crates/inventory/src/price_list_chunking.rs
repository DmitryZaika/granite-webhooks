//! Supplier price lists as searchable chunks (`supplier_file_chunks`).
//!
//! The AI's Markdown tables split by `###` section, CSV/TSV files turned into
//! Markdown tables, and plain text by lines. Port of the CRM's
//! `priceListChunking.ts`; lengths are counted in UTF-16 units like
//! JavaScript's, so both split at the same places.

use regex::Regex;
use std::sync::LazyLock;

/// A piece of a price list, at most [`DEFAULT_MAX_CHARS`] long except for a
/// single line that is longer by itself (kept whole: cutting a row could
/// corrupt a price).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chunk {
    pub heading: String,
    pub content: String,
}

pub const DEFAULT_MAX_CHARS: usize = 3500;
/// `supplier_file_chunks.heading` is `VARCHAR(255)`.
const MAX_HEADING_CHARS: usize = 255;

fn js_len(text: &str) -> usize {
    text.encode_utf16().count()
}

/// The first `max` UTF-16 units of `text` (whole characters).
fn js_prefix(text: &str, max: usize) -> String {
    let mut units = 0;
    text.chars()
        .take_while(|c| {
            units += c.len_utf16();
            units <= max
        })
        .collect()
}

static HEADING: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^###\s+(.*)$").unwrap());
static HEADING_START: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^###\s+").unwrap());
static SEPARATOR: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s*\|[\s\-:|]+\|?\s*$").unwrap());
static SLAB_SIZE_VALUE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)(?:jumbo|regular|standard)?\s*\d{2,3}\s*"?\s*[x×]\s*\d{2,3}"#).unwrap()
});
static SLAB_SIZE_HEADER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)slab\s*size").unwrap());
static SIZE_HEADER: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)^size$").unwrap());
static DIMENSIONS_HEADER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^dimensions?$").unwrap());
static KNOWN_HEADER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)color/product|thickness|group/level").unwrap());

struct Section {
    heading: String,
    lines: Vec<String>,
}

fn split_sections(structured: &str) -> Vec<Section> {
    let mut sections = vec![Section {
        heading: String::new(),
        lines: Vec::new(),
    }];
    for line in structured.split('\n') {
        if let Some(caps) = HEADING.captures(line) {
            sections.push(Section {
                heading: js_prefix(caps[1].trim(), MAX_HEADING_CHARS),
                lines: Vec::new(),
            });
        } else if let Some(last) = sections.last_mut() {
            last.lines.push(line.to_string());
        }
    }
    sections
        .into_iter()
        .filter(|s| !s.heading.is_empty() || !s.lines.concat().trim().is_empty())
        .collect()
}

fn table_header(lines: &[String]) -> Vec<String> {
    let Some(start) = lines
        .iter()
        .position(|line| line.trim_start().starts_with('|'))
    else {
        return Vec::new();
    };
    let header = lines[start].clone();
    let separator = lines.get(start + 1).map_or("", String::as_str);
    if SEPARATOR.is_match(separator) {
        vec![header, separator.to_string()]
    } else {
        vec![header]
    }
}

fn parse_markdown_row(line: &str) -> Option<Vec<String>> {
    let trimmed = line.trim();
    if !trimmed.starts_with('|') || SEPARATOR.is_match(trimmed) {
        return None;
    }
    let parts: Vec<&str> = trimmed.split('|').collect();
    let end = if parts.last().is_some_and(|last| last.trim().is_empty()) {
        parts.len() - 1
    } else {
        parts.len()
    };
    Some(
        parts[1..end]
            .iter()
            .map(|cell| cell.trim().to_string())
            .collect(),
    )
}

fn format_markdown_row(cells: &[String]) -> String {
    format!("| {} |", cells.join(" | "))
}

fn is_blank_slab_size(value: &str) -> bool {
    matches!(value.trim(), "" | "-" | "–" | "—")
}

fn slab_size_column_from_header(cells: &[String]) -> Option<usize> {
    cells.iter().position(|cell| {
        SLAB_SIZE_HEADER.is_match(cell)
            || SIZE_HEADER.is_match(cell)
            || DIMENSIONS_HEADER.is_match(cell)
    })
}

fn infer_slab_size_column(cells: &[String]) -> Option<usize> {
    cells
        .iter()
        .position(|cell| SLAB_SIZE_VALUE.is_match(cell))
        .or_else(|| (cells.len() == 5).then_some(3))
}

fn is_header_row(cells: &[String]) -> bool {
    slab_size_column_from_header(cells).is_some()
        || cells.iter().any(|cell| KNOWN_HEADER.is_match(cell))
}

/// Fills a section's shared slab size down its rows.
///
/// Merged slab-size cells (one size for a whole section) usually appear only
/// on the first row: copies that size onto later rows of the section that
/// show `-` or nothing. Returns the text and the last size seen.
pub fn fill_down_slab_sizes(text: &str, initial_size: &str) -> (String, String) {
    let mut last_size = initial_size.to_string();
    let mut size_column: Option<usize> = None;
    let out: Vec<String> = text
        .split('\n')
        .map(|line| {
            if HEADING_START.is_match(line.trim()) {
                last_size.clear();
                size_column = None;
                return line.to_string();
            }
            let Some(cells) = parse_markdown_row(line) else {
                return line.to_string();
            };
            if is_header_row(&cells) {
                size_column = slab_size_column_from_header(&cells);
                return line.to_string();
            }
            let Some(column) = size_column
                .or_else(|| infer_slab_size_column(&cells))
                .filter(|column| *column < cells.len())
            else {
                return line.to_string();
            };
            let value = &cells[column];
            if !is_blank_slab_size(value) {
                last_size.clone_from(value);
                size_column.get_or_insert(column);
                return line.to_string();
            }
            if last_size.is_empty() {
                return line.to_string();
            }
            let mut next = cells.clone();
            next[column].clone_from(&last_size);
            format_markdown_row(&next)
        })
        .collect();
    (out.join("\n"), last_size)
}

fn pack_lines(heading: &str, lines: &[String], header: &[String], max_chars: usize) -> Vec<Chunk> {
    let mut chunks = Vec::new();
    let mut current: Vec<String> = Vec::new();
    let mut current_len = 0;
    let header_len = js_len(&header.join("\n"));
    let flush = |current: &mut Vec<String>, current_len: &mut usize, chunks: &mut Vec<Chunk>| {
        let content = current.join("\n").trim().to_string();
        if !content.is_empty() {
            chunks.push(Chunk {
                heading: heading.to_string(),
                content,
            });
        }
        current.clear();
        *current_len = 0;
    };
    for line in lines {
        let line_len = js_len(line);
        if current_len + line_len + 1 > max_chars && !current.is_empty() {
            flush(&mut current, &mut current_len, &mut chunks);
            if !header.is_empty() && header_len + line_len < max_chars {
                current = header.to_vec();
                current_len = header_len;
            }
        }
        current.push(line.clone());
        current_len += line_len + 1;
    }
    flush(&mut current, &mut current_len, &mut chunks);
    chunks
}

/// The AI's Markdown (`### section` headings and tables) as chunks; a large
/// section is split at row boundaries, repeating its table header.
pub fn split_structured_markdown(structured: &str, max_chars: usize) -> Vec<Chunk> {
    split_sections(structured)
        .into_iter()
        .flat_map(|section| {
            let (filled, _) = fill_down_slab_sizes(&section.lines.join("\n"), "");
            let trimmed: Vec<String> = filled.trim().lines().map(str::to_string).collect();
            let joined = trimmed.join("\n");
            if js_len(&joined) <= max_chars {
                let content = joined.trim().to_string();
                return if content.is_empty() {
                    Vec::new()
                } else {
                    vec![Chunk {
                        heading: section.heading,
                        content,
                    }]
                };
            }
            pack_lines(
                &section.heading,
                &trimmed,
                &table_header(&trimmed),
                max_chars,
            )
        })
        .collect()
}

/// RFC 4180-style rows: quoted cells may hold the delimiter, `""` for a
/// quote, and newlines (folded to spaces so a record stays one table row).
fn parse_delimited_rows(text: &str, delimiter: char) -> Vec<Vec<String>> {
    let mut rows = Vec::new();
    let mut row: Vec<String> = Vec::new();
    let mut cell = String::new();
    let mut in_quotes = false;
    let end_row = |row: &mut Vec<String>, cell: &mut String, rows: &mut Vec<Vec<String>>| {
        row.push(std::mem::take(cell));
        if row.iter().any(|value| !value.trim().is_empty()) {
            rows.push(std::mem::take(row));
        } else {
            row.clear();
        }
    };
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if in_quotes {
            if c == '"' {
                if chars.get(i + 1) == Some(&'"') {
                    cell.push('"');
                    i += 1;
                } else {
                    in_quotes = false;
                }
            } else {
                cell.push(if c == '\n' || c == '\r' { ' ' } else { c });
            }
        } else if c == '"' && cell.trim().is_empty() {
            cell.clear();
            in_quotes = true;
        } else if c == delimiter {
            row.push(std::mem::take(&mut cell));
        } else if c == '\n' {
            end_row(&mut row, &mut cell, &mut rows);
        } else if c != '\r' {
            cell.push(c);
        }
        i += 1;
    }
    if !cell.is_empty() || !row.is_empty() {
        end_row(&mut row, &mut cell, &mut rows);
    }
    rows
}

/// A CSV (`,`) or TSV (`\t`) file as Markdown table chunks, each starting
/// with the header row.
pub fn csv_to_markdown_chunks(text: &str, delimiter: char, max_chars: usize) -> Vec<Chunk> {
    let rows: Vec<String> = parse_delimited_rows(text, delimiter)
        .into_iter()
        .map(|cells| {
            let cells: Vec<String> = cells
                .iter()
                .map(|cell| cell.trim().replace('|', "\\|"))
                .collect();
            format!("| {} |", cells.join(" | "))
        })
        .collect();
    let Some((header, body)) = rows.split_first() else {
        return Vec::new();
    };
    let column_count = header.split('|').count() - 2;
    let separator = format!("|{}|", vec!["---"; column_count].join("|"));
    let mut table = vec![header.clone(), separator.clone()];
    table.extend(body.iter().cloned());
    let (filled, _) = fill_down_slab_sizes(&table.join("\n"), "");
    let filled_body: Vec<String> = filled.split('\n').skip(2).map(str::to_string).collect();
    pack_lines(
        "",
        &filled_body,
        &[header.clone(), separator.clone()],
        max_chars,
    )
    .into_iter()
    .map(|chunk| {
        if chunk.content.starts_with(header.as_str()) {
            chunk
        } else {
            Chunk {
                content: format!("{header}\n{separator}\n{}", chunk.content),
                ..chunk
            }
        }
    })
    .collect()
}

/// Plain text split at line boundaries.
pub fn plain_text_chunks(text: &str, max_chars: usize) -> Vec<Chunk> {
    let lines: Vec<String> = text.split('\n').map(str::to_string).collect();
    pack_lines("", &lines, &[], max_chars)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(rows: &[&str]) -> String {
        rows.join("\n")
    }

    #[test]
    fn fills_a_merged_section_size_onto_later_dash_rows() {
        let input = lines(&[
            "| color/product | thickness | group/level | slab size | price |",
            "|---|---|---|---|---|",
            "| Calacatta Anava 2cm BM | 2 cm | Q+ | 127x64 | 56 |",
            "| Calacatta Lapiza 2cm BM | 2 cm | Q+ | - | 56 |",
        ]);
        let (text, last) = fill_down_slab_sizes(&input, "");
        assert!(text.contains("| Calacatta Lapiza 2cm BM | 2 cm | Q+ | 127x64 | 56 |"));
        assert_eq!(last, "127x64");
    }

    #[test]
    fn does_not_carry_a_size_across_a_heading() {
        let input = lines(&[
            "| color/product | thickness | group/level | slab size | price |",
            "| First | 2 cm | Q+ | 127x64 | 56 |",
            "### Other group",
            "| color/product | thickness | group/level | slab size | price |",
            "| Second | 2 cm | Q+ | - | 56 |",
        ]);
        let (text, _) = fill_down_slab_sizes(&input, "");
        assert!(text.contains("| Second | 2 cm | Q+ | - | 56 |"));
    }

    #[test]
    fn keeps_a_later_explicit_size_and_fills_after_it() {
        let input = lines(&[
            "| color/product | thickness | group/level | slab size | price |",
            "| A | 2 cm | Q+ | 127x64 | 56 |",
            "| B | 2 cm | Q+ | 130x79 | 56 |",
            "| C | 2 cm | Q+ | - | 56 |",
        ]);
        let (text, last) = fill_down_slab_sizes(&input, "");
        assert!(text.contains("| C | 2 cm | Q+ | 130x79 | 56 |"));
        assert_eq!(last, "130x79");
    }

    #[test]
    fn splits_on_headings_and_keeps_heading_text() {
        let input = "### Group 1 · pages 1–8\n| color | price |\n|---|---|\n| A | $1 |\n\n### Group 2 · pages 9–16\n| color | price |\n|---|---|\n| B | $2 |";
        let chunks = split_structured_markdown(input, DEFAULT_MAX_CHARS);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].heading, "Group 1 · pages 1–8");
        assert!(chunks[1].content.contains("| B | $2 |"));
    }

    #[test]
    fn splits_oversize_sections_repeating_the_header() {
        let rows: Vec<String> = (0..200)
            .map(|i| format!("| Color {i} | 130x65 | ${i} |"))
            .collect();
        let input = format!(
            "### Big\n| color | size | price |\n|---|---|---|\n{}",
            rows.join("\n")
        );
        let chunks = split_structured_markdown(&input, 1000);
        assert!(chunks.len() > 1);
        for chunk in &chunks {
            assert!(js_len(&chunk.content) <= 1000);
            assert!(chunk.content.contains("| color | size | price |"));
            assert_eq!(chunk.heading, "Big");
        }
        let joined: Vec<&str> = chunks.iter().map(|c| c.content.as_str()).collect();
        assert!(joined.join("\n").contains("| Color 199 | 130x65 | $199 |"));
    }

    #[test]
    fn clamps_a_runaway_heading_to_255() {
        let chunks = split_structured_markdown(
            &format!("### {}\n| A | $1 |", "H".repeat(400)),
            DEFAULT_MAX_CHARS,
        );
        assert_eq!(chunks[0].heading, "H".repeat(255));
    }

    #[test]
    fn content_before_the_first_heading_is_its_own_chunk() {
        let chunks = split_structured_markdown(
            "intro notes\n\n### A\n| x |\n|---|\n| 1 |",
            DEFAULT_MAX_CHARS,
        );
        assert_eq!(chunks[0].heading, "");
        assert_eq!(chunks[0].content, "intro notes");
    }

    #[test]
    fn fills_a_shared_size_before_splitting() {
        let mut rows = vec!["| Color 0 | 2 cm | Q+ | 127x64 | $0 |".to_string()];
        rows.extend((1..=80).map(|i| format!("| Color {i} | 2 cm | Q+ | - | ${i} |")));
        let input = format!(
            "### Q+ Premium 2cm\n| color/product | thickness | group/level | slab size | price |\n|---|---|---|---|---|\n{}",
            rows.join("\n")
        );
        let chunks = split_structured_markdown(&input, 800);
        assert!(chunks.len() > 1);
        let last = &chunks.last().unwrap().content;
        assert!(last.contains("127x64"));
        assert!(
            !Regex::new(r"\|\s*2 cm\s*\|\s*Q\+\s*\|\s*-\s*\|")
                .unwrap()
                .is_match(last)
        );
    }

    #[test]
    fn keeps_an_oversize_row_whole() {
        let huge = format!("| {} | $999 |", "x".repeat(2000));
        let input =
            format!("### Big\n| color | price |\n|---|---|\n| A | $1 |\n{huge}\n| B | $2 |");
        let chunks = split_structured_markdown(&input, 100);
        assert!(chunks.iter().any(|c| c.content.contains(&huge)));
    }

    #[test]
    fn csv_becomes_a_table_with_its_header() {
        let chunks = csv_to_markdown_chunks(
            "color,size,price\nAlaska White,130x65,$21.00\nBlack Pearl,126x63,$14.50",
            ',',
            DEFAULT_MAX_CHARS,
        );
        assert_eq!(chunks.len(), 1);
        assert!(chunks[0].content.contains("| color | size | price |"));
        assert!(
            chunks[0]
                .content
                .contains("| Alaska White | 130x65 | $21.00 |")
        );
    }

    #[test]
    fn csv_quotes_delimiters_doubled_quotes_newlines_and_pipes() {
        let quoted = csv_to_markdown_chunks(
            "color,size,price\n\"Alaska White, Polished\",130x65,\"$1,250.00\"",
            ',',
            DEFAULT_MAX_CHARS,
        );
        assert!(
            quoted[0]
                .content
                .contains("| Alaska White, Polished | 130x65 | $1,250.00 |")
        );
        let doubled = csv_to_markdown_chunks(
            "note,price\n\"the \"\"premium\"\" line\",$5",
            ',',
            DEFAULT_MAX_CHARS,
        );
        assert!(doubled[0].content.contains("| the \"premium\" line | $5 |"));
        let newline = csv_to_markdown_chunks(
            "color,note,price\nAlaska White,\"two\nlines\",$21.00",
            ',',
            DEFAULT_MAX_CHARS,
        );
        assert!(
            newline[0]
                .content
                .contains("| Alaska White | two lines | $21.00 |")
        );
        let pipe = csv_to_markdown_chunks("color,price\nBlack|Gold,$9", ',', DEFAULT_MAX_CHARS);
        assert!(pipe[0].content.contains("| Black\\|Gold | $9 |"));
    }

    #[test]
    fn plain_text_splits_on_lines_and_keeps_long_lines_whole() {
        let text: Vec<String> = (0..100).map(|i| format!("line {i}")).collect();
        let chunks = plain_text_chunks(&text.join("\n"), 200);
        assert!(chunks.iter().all(|c| js_len(&c.content) <= 200));
        assert!(chunks.last().unwrap().content.contains("line 99"));

        let huge = "x".repeat(500);
        let chunks = plain_text_chunks(&huge, 200);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].content, huge);

        let huge = "y".repeat(500);
        let chunks = plain_text_chunks(&format!("short line\n{huge}"), 200);
        assert_eq!(
            chunks
                .iter()
                .find(|c| c.content.contains(&huge))
                .unwrap()
                .content,
            huge
        );
    }
}
