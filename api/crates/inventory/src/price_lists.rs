//! Supplier price lists: the parts of the company's processed price files
//! that answer a question like "price for Calacatta Laza from MSI", as text
//! for a model to read, with the files it came from.
//!
//! Price files are split into Markdown chunks by the supplier file processor
//! (`supplier_file_chunks`). The search is lexical: suppliers named in the
//! question narrow the scope, chunks score by matching words (with typo
//! tolerance), group numbers and dollar signs, and a character budget is
//! shared so every file gets its best chunk. Port of the CRM's
//! `supplierChatContext.server.ts` and `fillDownSlabSizes`. The quote
//! builder's search for one color of one supplier
//! ([`build_color_context`]) reads only that supplier's files and scores
//! by the color's words.

use crate::openapi::ToSchema;
use regex::Regex;
use serde::Serialize;
use sqlx::{FromRow, MySql, MySqlPool, QueryBuilder};
use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;

/// Characters of excerpts given to the model.
const CONTEXT_CHUNK_BUDGET: usize = 9000;
const MIN_CHARS_PER_SOURCE_FILE: usize = 400;
/// A color search's bonus for a chunk with every word of the color (typos
/// allowed), and for one with the whole color name as written.
const ALL_COLOR_WORDS_BONUS: usize = 6;
const COLOR_PHRASE_BONUS: usize = 12;

const STOP_WORDS: &[&str] = &[
    "a",
    "an",
    "the",
    "and",
    "or",
    "but",
    "to",
    "of",
    "in",
    "on",
    "at",
    "by",
    "for",
    "with",
    "about",
    "into",
    "from",
    "as",
    "is",
    "are",
    "was",
    "were",
    "be",
    "been",
    "being",
    "do",
    "does",
    "did",
    "can",
    "could",
    "would",
    "should",
    "will",
    "have",
    "has",
    "had",
    "how",
    "what",
    "when",
    "where",
    "which",
    "who",
    "why",
    "i",
    "me",
    "my",
    "we",
    "us",
    "our",
    "you",
    "your",
    "it",
    "its",
    "this",
    "that",
    "these",
    "those",
    "please",
    "tell",
    "show",
    "give",
    "get",
    "want",
    "need",
    "know",
    "info",
    "information",
    "price",
    "cost",
    "much",
    "per",
    "sqft",
];

const CONTEXT_PREAMBLE: [&str; 12] = [
    "SUPPLIER PRICE LISTS",
    "Answer using ONLY the supplier price-list excerpts below. These are pre-structured Markdown tables extracted from supplier documents.",
    "You MUST check every SOURCE below before answering. A supplier often has multiple price-list files; the product or price may be in any of them, not only the first.",
    "Only report dollar prices that explicitly appear in the excerpts. Never invent or estimate prices.",
    "When a Group N block lists products without individual prices and one price at the bottom of that block, that bottom price applies to every product listed in Group N above it unless a product row shows its own price. A GROUP N SLAB PRICE line states that group price explicitly.",
    "When a section or group lists one slab size for the block (a merged cell, a size on the first row only, or a size in the heading), that size applies to every product in that block even if later rows show \"-\".",
    "When a product shows a color group or level and size but no dollar price on its row, check the GROUP price line and every other SOURCE from that same supplier before saying the price is not specified.",
    "Only after checking all excerpts from that supplier should you say the price is not specified and give the level or group and size.",
    "If the requested color name is close but not exact (for example \"Adonia\" vs \"Calacatta Adonia\"), give the price for the closest matching product from that supplier. Say the name was not an exact match, give the exact document name, then state the price and size.",
    "Only say the product could not be found when there is no reasonable close match in the excerpts from that supplier.",
    "Be brief. Include exact prices only when they are written in the excerpt.",
    "Each excerpt below is labeled with a SOURCE number. Track which excerpt your answer comes from so it can be cited.",
];

static THICKNESS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(\d+(?:\.\d+)?)\s*cm(?-u:\b)").expect("thickness regex"));
static NON_WORD: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[^A-Za-z0-9_]+").expect("non-word regex"));
static NON_ALNUM: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[^a-z0-9]+").expect("non-alnum regex"));
static QUERY_GROUP: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(?-u:\b)(?:group|level)\s*(\d+)(?-u:\b)").expect("group regex")
});
static FILE_EXTENSION: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\.([a-z0-9]+)(?:\?|$)").expect("extension regex"));
static SEPARATOR_ROW: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s*\|[\s\-:|]+\|?\s*$").expect("separator regex"));
static HEADING: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^###\s+").expect("heading regex"));
static SLAB_SIZE_VALUE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)(?:jumbo|regular|standard)?\s*\d{2,3}\s*"?\s*[x×]\s*\d{2,3}"#)
        .expect("slab size regex")
});
static SLAB_SIZE_HEADER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)slab\s*size|^size$|^dimensions?$").expect("slab size header regex")
});
static HEADER_CELL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)color/product|thickness|group/level").expect("header cell regex")
});

/// How far the company's price files for the searched suppliers are processed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, FromRow, ToSchema)]
pub struct FileReadiness {
    /// Price files.
    pub total: i64,
    /// Processed and searchable.
    pub ready: i64,
    /// Waiting or being processed.
    pub processing: i64,
    /// Could not be processed.
    pub failed: i64,
}

/// What kind of file a source is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum SourceFileType {
    Pdf,
    Image,
    File,
}

/// A price file excerpts were taken from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct PriceListSource {
    /// `supplier_files.id`; the context labels it `SOURCE <id>`.
    pub id: i64,
    /// File name.
    pub name: String,
    /// Its supplier.
    pub supplier_name: String,
    /// Where the file is stored.
    pub url: String,
    /// `pdf`, `image` or `file`.
    pub file_type: SourceFileType,
}

/// The excerpts for a question.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PriceListContext {
    /// Instructions and excerpts, labeled by source, for the model.
    pub context: String,
    /// Files the excerpts came from, in the order they appear.
    pub sources: Vec<PriceListSource>,
    /// Readiness of every file in the searched scope; `None` when the company
    /// has no suppliers.
    pub readiness: Option<FileReadiness>,
    /// The supplier the question named (best match); empty when none.
    pub supplier_name: String,
}

#[derive(Debug, Clone, FromRow)]
struct SupplierRow {
    id: i32,
    supplier_name: Option<String>,
}

#[derive(Debug, Clone, FromRow)]
struct ChunkRow {
    file_id: i64,
    file_name: String,
    file_url: String,
    supplier_name: Option<String>,
    heading: String,
    content: String,
    chunk_index: i32,
}

#[derive(Debug, Clone)]
struct ScoredChunk {
    row: ChunkRow,
    score: usize,
}

fn char_len(text: &str) -> usize {
    text.chars().count()
}

/// Lowercase, with a thickness written as one word (`3 cm` → `3cm`).
pub fn normalize_thickness(text: &str) -> String {
    THICKNESS
        .replace_all(&text.to_lowercase(), "${1}cm")
        .into_owned()
}

/// The words of a text, lowercase and single-spaced, to find a name in it
/// as written.
fn phrase_words(text: &str) -> String {
    NON_ALNUM
        .split(&normalize_thickness(text))
        .filter(|word| !word.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

fn query_terms(query: &str) -> Vec<String> {
    NON_WORD
        .split(&normalize_thickness(query))
        .filter(|term| char_len(term) >= 2 && !STOP_WORDS.contains(term))
        .map(str::to_string)
        .collect()
}

fn levenshtein(a: &[char], b: &[char]) -> usize {
    if a.is_empty() {
        return b.len();
    }
    if b.is_empty() {
        return a.len();
    }
    let mut previous: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.iter().enumerate() {
        let mut current = vec![i + 1; b.len() + 1];
        for (j, cb) in b.iter().enumerate() {
            let cost = usize::from(ca != cb);
            current[j + 1] = (previous[j + 1] + 1)
                .min(current[j] + 1)
                .min(previous[j] + cost);
        }
        previous = current;
    }
    previous[b.len()]
}

/// The term is in the text, or (terms of 5+ letters) a word of the text is
/// within one typo (two for 8+ letters).
fn term_matches(term: &str, haystack: &str) -> bool {
    let lower = normalize_thickness(haystack);
    let term_lower = normalize_thickness(term);
    if lower.contains(&term_lower) {
        return true;
    }
    let term_len = char_len(term);
    if term_len < 5 {
        return false;
    }
    let max_dist = if term_len >= 8 { 2 } else { 1 };
    let term_chars: Vec<char> = term_lower.chars().collect();
    NON_ALNUM
        .split(&lower)
        .filter(|word| char_len(word) >= 3)
        .any(|word| {
            let word_chars: Vec<char> = word.chars().collect();
            word_chars.len().abs_diff(term_len) <= max_dist
                && levenshtein(&term_chars, &word_chars) <= max_dist
        })
}

fn count_matching_terms(terms: &[String], haystack: &str) -> usize {
    if haystack.is_empty() {
        return 0;
    }
    terms
        .iter()
        .filter(|term| term_matches(term, haystack))
        .count()
}

fn truncate_text(text: &str, max_chars: usize) -> String {
    let trimmed = text.trim();
    if char_len(trimmed) <= max_chars {
        return trimmed.to_string();
    }
    let head: String = trimmed.chars().take(max_chars).collect();
    format!("{head}\n...[truncated]")
}

fn matching_suppliers(suppliers: &[SupplierRow], query: &str) -> Vec<SupplierRow> {
    let terms = query_terms(query);
    if terms.is_empty() {
        return Vec::new();
    }
    let mut scored: Vec<(usize, &SupplierRow)> = suppliers
        .iter()
        .map(|supplier| {
            let name = supplier.supplier_name.as_deref().unwrap_or_default();
            (count_matching_terms(&terms, name), supplier)
        })
        .filter(|(score, _)| *score > 0)
        .collect();
    scored.sort_by_key(|(score, _)| std::cmp::Reverse(*score));
    let Some(top) = scored.first().map(|(score, _)| *score) else {
        return Vec::new();
    };
    scored
        .into_iter()
        .filter(|(score, _)| *score == top)
        .map(|(_, supplier)| supplier.clone())
        .collect()
}

fn mentions_group(text: &str, group: &str) -> bool {
    Regex::new(&format!(r"(?i)(?-u:\b)(?:group|level)\s*{group}(?-u:\b)"))
        .is_ok_and(|pattern| pattern.is_match(text))
}

fn score_chunks(rows: Vec<ChunkRow>, query: &str) -> Vec<ScoredChunk> {
    let terms = query_terms(query);
    let group = QUERY_GROUP
        .captures(query)
        .and_then(|captures| captures.get(1))
        .map(|m| m.as_str().to_string());
    let mut scored: Vec<ScoredChunk> = rows
        .into_iter()
        .map(|row| {
            let mut score = count_matching_terms(
                &terms,
                &format!("{} {} {}", row.file_name, row.heading, row.content),
            );
            if let Some(group) = &group
                && (mentions_group(&row.heading, group) || mentions_group(&row.content, group))
            {
                score += 25;
            }
            if row.content.contains('$') {
                score += 4;
            }
            ScoredChunk { row, score }
        })
        .collect();
    best_first(&mut scored);
    scored
}

/// Chunks scored for one color: its words only (not the supplier's), in
/// the heading and text, with a bonus when all of them are there and a
/// bigger one when the whole name is.
fn score_color_chunks(rows: Vec<ChunkRow>, color: &str) -> Vec<ScoredChunk> {
    let terms = query_terms(color);
    let phrase = format!(" {} ", phrase_words(color));
    let mut scored: Vec<ScoredChunk> = rows
        .into_iter()
        .map(|row| {
            let text = format!("{} {}", row.heading, row.content);
            let matched = count_matching_terms(&terms, &text);
            let mut score = matched;
            if matched > 0 && matched == terms.len() {
                score += ALL_COLOR_WORDS_BONUS;
            }
            if phrase.trim().len() >= 2 && format!(" {} ", phrase_words(&text)).contains(&phrase) {
                score += COLOR_PHRASE_BONUS;
            }
            if row.content.contains('$') {
                score += 4;
            }
            ScoredChunk { row, score }
        })
        .collect();
    best_first(&mut scored);
    scored
}

fn best_first(scored: &mut [ScoredChunk]) {
    scored.sort_by(|a, b| {
        b.score
            .cmp(&a.score)
            .then(a.row.file_id.cmp(&b.row.file_id))
            .then(a.row.chunk_index.cmp(&b.row.chunk_index))
    });
}

/// Every file's best chunk first (each capped to its share of the budget),
/// then the best of the rest while the budget lasts.
fn select_chunks(scored: &[ScoredChunk]) -> Vec<ScoredChunk> {
    if scored.is_empty() {
        return Vec::new();
    }
    let mut file_order: Vec<i64> = Vec::new();
    let mut best_by_file: HashMap<i64, &ScoredChunk> = HashMap::new();
    for item in scored {
        best_by_file.entry(item.row.file_id).or_insert_with(|| {
            file_order.push(item.row.file_id);
            item
        });
    }
    let per_file_share = MIN_CHARS_PER_SOURCE_FILE.max(CONTEXT_CHUNK_BUDGET / file_order.len());

    let mut selected: Vec<ScoredChunk> = Vec::new();
    let mut keys: HashSet<(i64, i32)> = HashSet::new();
    let mut total = 0usize;
    let mut add = |item: ScoredChunk, selected: &mut Vec<ScoredChunk>, total: &mut usize| {
        let key = (item.row.file_id, item.row.chunk_index);
        if keys.contains(&key) || *total >= CONTEXT_CHUNK_BUDGET {
            return;
        }
        let remaining = CONTEXT_CHUNK_BUDGET - *total;
        let mut item = item;
        if char_len(&item.row.content) > remaining {
            if remaining < MIN_CHARS_PER_SOURCE_FILE && !selected.is_empty() {
                return;
            }
            item.row.content = truncate_text(&item.row.content, remaining);
        }
        *total += char_len(&item.row.content);
        keys.insert(key);
        selected.push(item);
    };

    for file_id in &file_order {
        let best = best_by_file[file_id];
        let mut capped = best.clone();
        if char_len(&capped.row.content) > per_file_share {
            capped.row.content = truncate_text(&capped.row.content, per_file_share);
        }
        add(capped, &mut selected, &mut total);
    }
    for item in scored {
        if total >= CONTEXT_CHUNK_BUDGET {
            break;
        }
        if total + char_len(&item.row.content) > CONTEXT_CHUNK_BUDGET {
            continue;
        }
        add(item.clone(), &mut selected, &mut total);
    }
    selected
}

fn markdown_row(line: &str) -> Option<Vec<String>> {
    let trimmed = line.trim();
    if !trimmed.starts_with('|') || SEPARATOR_ROW.is_match(trimmed) {
        return None;
    }
    let parts: Vec<&str> = trimmed.split('|').collect();
    let end = if parts.last().is_some_and(|last| last.trim().is_empty()) {
        parts.len() - 1
    } else {
        parts.len()
    };
    Some(
        parts
            .get(1..end)
            .unwrap_or_default()
            .iter()
            .map(|cell| cell.trim().to_string())
            .collect(),
    )
}

fn is_blank_size(value: &str) -> bool {
    matches!(value.trim(), "" | "-" | "–" | "—")
}

fn size_column_from_header(cells: &[String]) -> Option<usize> {
    cells
        .iter()
        .position(|cell| SLAB_SIZE_HEADER.is_match(cell))
}

fn infer_size_column(cells: &[String]) -> Option<usize> {
    cells
        .iter()
        .position(|cell| SLAB_SIZE_VALUE.is_match(cell))
        .or_else(|| (cells.len() == 5).then_some(3))
}

fn is_header_row(cells: &[String]) -> bool {
    size_column_from_header(cells).is_some() || cells.iter().any(|cell| HEADER_CELL.is_match(cell))
}

/// Copies a section's slab size onto its later rows that show `-` or nothing.
///
/// A merged size cell is often only on the section's first row. Returns the
/// text and the size in effect at its end, to carry into the next chunk.
pub fn fill_down_slab_sizes(text: &str, initial_size: &str) -> (String, String) {
    let mut last_size = initial_size.to_string();
    let mut size_column: Option<usize> = None;
    let lines: Vec<String> = text
        .split('\n')
        .map(|line| {
            if HEADING.is_match(line.trim()) {
                last_size.clear();
                size_column = None;
                return line.to_string();
            }
            let Some(mut cells) = markdown_row(line) else {
                return line.to_string();
            };
            if is_header_row(&cells) {
                size_column = size_column_from_header(&cells);
                return line.to_string();
            }
            let Some(column) = size_column.or_else(|| infer_size_column(&cells)) else {
                return line.to_string();
            };
            let Some(value) = cells.get(column) else {
                return line.to_string();
            };
            if !is_blank_size(value) {
                last_size.clone_from(value);
                size_column.get_or_insert(column);
                return line.to_string();
            }
            if last_size.is_empty() {
                return line.to_string();
            }
            cells[column].clone_from(&last_size);
            format!("| {} |", cells.join(" | "))
        })
        .collect();
    (lines.join("\n"), last_size)
}

fn fill_down_selected(all: &[ChunkRow], selected: Vec<ScoredChunk>) -> Vec<ScoredChunk> {
    let mut by_file: HashMap<i64, Vec<&ChunkRow>> = HashMap::new();
    for chunk in all {
        by_file.entry(chunk.file_id).or_default().push(chunk);
    }
    let mut initial: HashMap<(i64, i32), String> = HashMap::new();
    for chunks in by_file.values_mut() {
        chunks.sort_by_key(|chunk| chunk.chunk_index);
        let mut last_size = String::new();
        let mut last_heading = String::new();
        for chunk in chunks.iter() {
            if chunk.heading != last_heading {
                last_size.clear();
                last_heading.clone_from(&chunk.heading);
            }
            initial.insert((chunk.file_id, chunk.chunk_index), last_size.clone());
            last_size = fill_down_slab_sizes(&chunk.content, &last_size).1;
        }
    }
    selected
        .into_iter()
        .map(|mut item| {
            let start = initial
                .get(&(item.row.file_id, item.row.chunk_index))
                .map_or("", String::as_str);
            item.row.content = fill_down_slab_sizes(&item.row.content, start).0;
            item
        })
        .collect()
}

/// Lowercase extension of a file name, else of its URL; empty when neither has one.
pub fn file_extension(name: &str, url: &str) -> String {
    let find = |text: &str| {
        FILE_EXTENSION
            .captures(&text.to_lowercase())
            .and_then(|captures| captures.get(1))
            .map(|m| m.as_str().to_string())
    };
    find(name).or_else(|| find(url)).unwrap_or_default()
}

fn source_file_type(name: &str, url: &str) -> SourceFileType {
    match file_extension(name, url).as_str() {
        "pdf" => SourceFileType::Pdf,
        "jpg" | "jpeg" | "png" | "webp" | "gif" | "bmp" | "tiff" => SourceFileType::Image,
        _ => SourceFileType::File,
    }
}

/// The preamble and one `SOURCE` section per file, in order of first excerpt.
fn render_sources(filled: Vec<ScoredChunk>) -> (Vec<String>, Vec<PriceListSource>) {
    let mut sections: Vec<String> = CONTEXT_PREAMBLE.iter().map(|s| (*s).to_string()).collect();
    let mut sources = Vec::new();
    let mut order: Vec<i64> = Vec::new();
    let mut by_file: HashMap<i64, Vec<ChunkRow>> = HashMap::new();
    for item in filled {
        let file_id = item.row.file_id;
        by_file
            .entry(file_id)
            .or_insert_with(|| {
                order.push(file_id);
                Vec::new()
            })
            .push(item.row);
    }
    for file_id in order {
        let mut chunks = by_file.remove(&file_id).unwrap_or_default();
        chunks.sort_by_key(|chunk| chunk.chunk_index);
        let Some(first) = chunks.first() else {
            continue;
        };
        let supplier_name = first.supplier_name.clone().unwrap_or_default();
        sources.push(PriceListSource {
            id: file_id,
            name: first.file_name.clone(),
            supplier_name: supplier_name.clone(),
            url: first.file_url.clone(),
            file_type: source_file_type(&first.file_name, &first.file_url),
        });
        let body = chunks
            .iter()
            .map(|chunk| {
                if chunk.heading.is_empty() {
                    chunk.content.clone()
                } else {
                    format!("### {}\n{}", chunk.heading, chunk.content)
                }
            })
            .collect::<Vec<_>>()
            .join("\n\n");
        sections.push(format!(
            "\nSOURCE {file_id}: {supplier_name} — {}",
            first.file_name
        ));
        sections.push(format!("CONTENT:\n{body}"));
    }
    (sections, sources)
}

/// Readiness of the files of the given suppliers of the company.
pub async fn readiness_for_suppliers(
    pool: &MySqlPool,
    company_id: i32,
    supplier_ids: &[i32],
) -> Result<FileReadiness, sqlx::Error> {
    if supplier_ids.is_empty() {
        return Ok(FileReadiness::default());
    }
    let mut query: QueryBuilder<MySql> = QueryBuilder::new(
        "SELECT COUNT(sf.id) AS total,
                CAST(COALESCE(SUM(sf.status = 'ready'), 0) AS SIGNED) AS ready,
                CAST(COALESCE(SUM(sf.status IN ('pending', 'processing')), 0) AS SIGNED) AS processing,
                CAST(COALESCE(SUM(sf.status = 'failed'), 0) AS SIGNED) AS failed
           FROM suppliers s
           LEFT JOIN supplier_files sf ON sf.supplier_id = s.id
          WHERE s.company_id = ",
    );
    query.push_bind(company_id);
    crate::sql::push_in(&mut query, "s.id", supplier_ids);
    query.build_query_as().fetch_one(pool).await
}

async fn company_suppliers(
    pool: &MySqlPool,
    company_id: i32,
) -> Result<Vec<SupplierRow>, sqlx::Error> {
    sqlx::query_as("SELECT id, supplier_name FROM suppliers WHERE company_id = ? ORDER BY id")
        .bind(company_id)
        .fetch_all(pool)
        .await
}

const fn no_suppliers() -> PriceListContext {
    PriceListContext {
        context: String::new(),
        sources: Vec::new(),
        readiness: None,
        supplier_name: String::new(),
    }
}

/// The price-list excerpts that answer `query`, for the company.
pub async fn build_context(
    pool: &MySqlPool,
    company_id: i32,
    query: &str,
) -> Result<PriceListContext, sqlx::Error> {
    let suppliers = company_suppliers(pool, company_id).await?;
    if suppliers.is_empty() {
        return Ok(no_suppliers());
    }
    let matched = matching_suppliers(&suppliers, query);
    let scope: Vec<i32> = if matched.is_empty() {
        &suppliers
    } else {
        &matched
    }
    .iter()
    .map(|supplier| supplier.id)
    .collect();
    excerpts(pool, company_id, &scope, matched.first(), |rows| {
        score_chunks(rows, query)
    })
    .await
}

/// The price-list excerpts for one color of one supplier.
///
/// The supplier is the company's suppliers with that name (trimmed, any
/// case), else the closest names; no other supplier's files are read, and
/// none at all when no name is close. Chunks score by the color's words
/// only.
pub async fn build_color_context(
    pool: &MySqlPool,
    company_id: i32,
    supplier: &str,
    color: &str,
) -> Result<PriceListContext, sqlx::Error> {
    let suppliers = company_suppliers(pool, company_id).await?;
    if suppliers.is_empty() {
        return Ok(no_suppliers());
    }
    let name = supplier.trim().to_lowercase();
    let exact: Vec<SupplierRow> = suppliers
        .iter()
        .filter(|s| {
            s.supplier_name
                .as_deref()
                .is_some_and(|n| n.trim().to_lowercase() == name)
        })
        .cloned()
        .collect();
    let matched = if exact.is_empty() {
        matching_suppliers(&suppliers, supplier)
    } else {
        exact
    };
    let scope: Vec<i32> = matched.iter().map(|supplier| supplier.id).collect();
    excerpts(pool, company_id, &scope, matched.first(), |rows| {
        score_color_chunks(rows, color)
    })
    .await
}

/// The context from the ready files of the suppliers in `scope` (an empty
/// scope reads none), and their readiness. `top` is the supplier the
/// search named, for the note on files still processing; `rank` scores
/// the chunks, best first.
async fn excerpts(
    pool: &MySqlPool,
    company_id: i32,
    scope: &[i32],
    top: Option<&SupplierRow>,
    rank: impl FnOnce(Vec<ChunkRow>) -> Vec<ScoredChunk>,
) -> Result<PriceListContext, sqlx::Error> {
    let rows: Vec<ChunkRow> = if scope.is_empty() {
        Vec::new()
    } else {
        let mut chunk_query: QueryBuilder<MySql> = QueryBuilder::new(
            "SELECT CAST(sf.id AS SIGNED) AS file_id, sf.name AS file_name, sf.url AS file_url,
                    s.supplier_name, c.heading, c.content, c.chunk_index
               FROM supplier_file_chunks c
               JOIN supplier_files sf ON sf.id = c.supplier_file_id
               JOIN suppliers s ON s.id = sf.supplier_id
              WHERE sf.status = 'ready' AND s.company_id = ",
        );
        chunk_query.push_bind(company_id);
        crate::sql::push_in(&mut chunk_query, "sf.supplier_id", scope);
        chunk_query.push(" ORDER BY sf.id, c.chunk_index");
        chunk_query.build_query_as().fetch_all(pool).await?
    };

    let selected = select_chunks(&rank(rows.clone()));
    let filled = fill_down_selected(&rows, selected);

    let (mut sections, sources) = render_sources(filled);
    if sources.is_empty() {
        sections.push("\nNo matching supplier price-list content was found.".into());
    }

    let readiness = readiness_for_suppliers(pool, company_id, scope).await?;
    if let Some(top) = top
        && (readiness.processing > 0 || readiness.failed > 0)
    {
        let mut parts = Vec::new();
        if readiness.processing > 0 {
            parts.push(format!(
                "{} are still being processed",
                readiness.processing
            ));
        }
        if readiness.failed > 0 {
            parts.push(format!("{} could not be processed", readiness.failed));
        }
        sections.push(format!(
            "\nNOTE: of {} price files for {}, {}; the answer may be incomplete.",
            readiness.total,
            top.supplier_name.as_deref().unwrap_or_default(),
            parts.join(" and ")
        ));
    }
    Ok(PriceListContext {
        context: sections.join("\n"),
        sources,
        readiness: Some(readiness),
        supplier_name: top
            .and_then(|supplier| supplier.supplier_name.clone())
            .unwrap_or_default(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terms_drop_stop_words_and_join_thickness() {
        assert_eq!(
            query_terms("Price for Calacatta Laza 3 cm from MSI"),
            ["calacatta", "laza", "3cm", "msi"]
        );
    }

    #[test]
    fn terms_match_with_typos_by_length() {
        assert!(term_matches("calacata", "Calacatta Edelia"));
        assert!(!term_matches("kolokata", "Calacatta Edelia"));
        assert!(term_matches("edelia", "Calacatta Edelia"));
        assert!(!term_matches("edelai", "Calacatta Edelia"));
        assert!(term_matches("edeli", "Calacatta Edelia"));
        assert!(!term_matches("msx", "MSI"));
        assert!(term_matches("3cm", "| 3 cm |"));
    }

    fn chunk_row(file_id: i64, chunk_index: i32, heading: &str, content: &str) -> ChunkRow {
        ChunkRow {
            file_id,
            file_name: "Stellar price list".into(),
            file_url: String::new(),
            supplier_name: Some("Cosentino".into()),
            heading: heading.into(),
            content: content.into(),
            chunk_index,
        }
    }

    #[test]
    fn a_color_scores_by_its_own_words_most_for_the_whole_name() {
        let rows = vec![
            chunk_row(1, 0, "Silestone", "| Stellar Snow | 2cm | $2,042.23 |"),
            chunk_row(1, 1, "Silestone", "| Night Stellar | 2cm | $1,990.00 |"),
            chunk_row(1, 2, "Silestone", "| Stellar Night | 3cm | $2,609.33 |"),
            chunk_row(1, 3, "Cosentino Stellar", "Terms and conditions"),
        ];
        let scored: Vec<(i32, usize)> = score_color_chunks(rows, "Stellar  Night")
            .iter()
            .map(|s| (s.row.chunk_index, s.score))
            .collect();
        // Both words + all-words bonus + the whole name + the dollar sign.
        assert_eq!(scored, [(2, 24), (1, 12), (0, 5), (3, 1)]);
    }

    #[test]
    fn a_color_name_matches_as_written_whatever_the_case_and_spacing() {
        assert_eq!(phrase_words("Calacatta  LAZA, 3 cm"), "calacatta laza 3cm");
        let rows = vec![chunk_row(1, 0, "", "| CALACATTA-LAZA | $28 |")];
        assert_eq!(
            score_color_chunks(rows, "calacatta laza")[0].score,
            2 + 6 + 12 + 4
        );
        // A typo still counts the word, but is not the name as written.
        let rows = vec![chunk_row(1, 0, "", "| Calacatta Laza | $28 |")];
        assert_eq!(
            score_color_chunks(rows, "calacata laza")[0].score,
            2 + 6 + 4
        );
    }

    #[test]
    fn levenshtein_counts_edits() {
        let chars = |s: &str| s.chars().collect::<Vec<_>>();
        assert_eq!(levenshtein(&chars("kitten"), &chars("sitting")), 3);
        assert_eq!(levenshtein(&chars(""), &chars("abc")), 3);
    }

    #[test]
    fn slab_sizes_fill_down_within_a_section() {
        let text = "| color/product | thickness | group/level | slab size | price |\n|---|---|---|---|---|\n| A | 2 cm | Q+ | 127x64 | $29.82 |\n| B | 2 cm | Q+ | - | $30.00 |\n### Next\n| C | 2 cm | Q+ | - | $1 |";
        let (filled, last) = fill_down_slab_sizes(text, "");
        assert_eq!(
            filled,
            "| color/product | thickness | group/level | slab size | price |\n|---|---|---|---|---|\n| A | 2 cm | Q+ | 127x64 | $29.82 |\n| B | 2 cm | Q+ | 127x64 | $30.00 |\n### Next\n| C | 2 cm | Q+ | - | $1 |"
        );
        assert_eq!(last, "");
        let (carried, last) = fill_down_slab_sizes("| D | 3cm | 1 | – | $5 |", "130x65");
        assert_eq!(carried, "| D | 3cm | 1 | 130x65 | $5 |");
        assert_eq!(last, "130x65");
    }

    #[test]
    fn markdown_rows_split_cells() {
        assert_eq!(
            markdown_row("| a | b |"),
            Some(vec!["a".into(), "b".into()])
        );
        assert_eq!(markdown_row("| a | b"), Some(vec!["a".into(), "b".into()]));
        assert_eq!(markdown_row("|---|---|"), None);
        assert_eq!(markdown_row("text"), None);
    }

    #[test]
    fn extensions_come_from_the_name_then_the_url() {
        assert_eq!(file_extension("MSI.PDF", "x"), "pdf");
        assert_eq!(file_extension("MSI", "https://b/x.png?sig=1"), "png");
        assert_eq!(file_extension("MSI", "https://b/x"), "");
    }

    #[test]
    fn truncation_marks_the_cut() {
        assert_eq!(truncate_text("  abc  ", 5), "abc");
        assert_eq!(truncate_text("abcdef", 3), "abc\n...[truncated]");
    }
}
