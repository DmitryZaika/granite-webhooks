//! Instructions mode: the company instructions the model gets.
//!
//! Which instructions go to the model, the hints that steer it, which ones
//! it used and their pictures. Port of the CRM's `instructionImages.ts` and
//! `loadVisibleInstructionsForUser`.

use super::markers::{parse_instruction_indices, strip_markers};
use crate::followups::instruction::AI_FOLLOWUP_ADMIN_INSTRUCTION_TITLE;
use crate::followups::js::{js_len, trim};
use crate::re;
use common::utils::email_body::html_to_plain_text;
use serde::Serialize;
use sqlx::MySqlPool;
use std::collections::{HashMap, HashSet};

/// An instruction as the chat reads it.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct Instruction {
    pub id: i32,
    pub title: Option<String>,
    pub rich_text: String,
}

/// An instruction the answer used, shown under it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MatchedInstruction {
    pub id: i32,
    pub title: String,
}

/// `loadVisibleInstructionsForUser`: the company's instructions the user's
/// positions open (an instruction with no positions is for everyone).
pub async fn load_visible(
    pool: &MySqlPool,
    company_id: i32,
    user_id: i32,
) -> sqlx::Result<Vec<Instruction>> {
    let rows: Vec<Instruction> = sqlx::query_as(
        "SELECT id, title, COALESCE(rich_text, '') AS rich_text FROM instructions
          WHERE company_id = ? AND (title IS NULL OR title <> ?)",
    )
    .bind(company_id)
    .bind(AI_FOLLOWUP_ADMIN_INSTRUCTION_TITLE)
    .fetch_all(pool)
    .await?;
    if rows.is_empty() {
        return Ok(rows);
    }
    let positions: HashSet<i32> = sqlx::query_scalar(
        "SELECT DISTINCT position_id FROM users_positions WHERE user_id = ? AND company_id = ?",
    )
    .bind(user_id)
    .bind(company_id)
    .fetch_all(pool)
    .await?
    .into_iter()
    .collect();
    let marks = vec!["?"; rows.len()].join(", ");
    let sql = format!(
        "SELECT instruction_id, position_id FROM instruction_access WHERE instruction_id IN ({marks})"
    );
    let mut query = sqlx::query_as::<_, (i32, i32)>(sqlx::AssertSqlSafe(sql));
    for row in &rows {
        query = query.bind(row.id);
    }
    let mut access: HashMap<i32, Vec<i32>> = HashMap::new();
    for (instruction, position) in query.fetch_all(pool).await? {
        access.entry(instruction).or_default().push(position);
    }
    Ok(rows
        .into_iter()
        .filter(|i| {
            access.get(&i.id).is_none_or(|allowed| {
                allowed.is_empty() || allowed.iter().any(|p| positions.contains(p))
            })
        })
        .collect())
}

const STOP_WORDS: [&str; 76] = [
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
    "out",
    "up",
    "so",
    "if",
    "then",
    "than",
    "there",
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
    "instruction",
    "instructions",
];

fn is_stop_word(word: &str) -> bool {
    STOP_WORDS.contains(&word)
}

/// `<img src>` URLs of an instruction body, in order, once each.
pub fn image_urls(html: &str) -> Vec<String> {
    let mut urls = Vec::new();
    for c in re!(r#"<img\b[^>]*\bsrc=["']([^"']+)["'][^>]*>"#, "gi").captures_iter(html) {
        let src = trim(&c[1]).to_string();
        if !src.is_empty() && !urls.contains(&src) {
            urls.push(src);
        }
    }
    urls
}

/// JS `text.split(/\W+/)`.
fn split_non_word(text: &str) -> Vec<&str> {
    re!(r"\W+").split(text).collect()
}

/// "15,499" → "15499", so a number matches however it is written.
fn join_digit_groups(text: &str) -> String {
    let mut text = text.to_string();
    loop {
        let joined = re!(r"(\d),(\d{3})\b").replace_all(&text, "$1$2").into_owned();
        if joined == text {
            return text;
        }
        text = joined;
    }
}

fn query_terms(query: &str) -> Vec<String> {
    split_non_word(&join_digit_groups(&query.to_lowercase()))
        .into_iter()
        .filter(|t| js_len(t) >= 3 && !is_stop_word(t))
        .map(str::to_string)
        .collect()
}

fn is_number(term: &str) -> bool {
    term.bytes().all(|b| b.is_ascii_digit())
}

/// The query names a number (a price, a size) this instruction has.
fn has_query_number(text: &Text, terms: &[String]) -> bool {
    terms
        .iter()
        .any(|t| is_number(t) && count_words(t, &text.body) > 0)
}

/// Typos between two words or numbers: letters added, dropped, changed, or
/// two neighbours swapped ("speical" → "special", "154999" → "15499").
fn typo_distance(a: &[char], b: &[char]) -> usize {
    let mut d = vec![vec![0; b.len() + 1]; a.len() + 1];
    for (i, row) in d.iter_mut().enumerate() {
        row[0] = i;
    }
    for j in 0..=b.len() {
        d[0][j] = j;
    }
    for i in 1..=a.len() {
        for j in 1..=b.len() {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            d[i][j] = (d[i - 1][j] + 1)
                .min(d[i][j - 1] + 1)
                .min(d[i - 1][j - 1] + cost);
            if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                d[i][j] = d[i][j].min(d[i - 2][j - 2] + 1);
            }
        }
    }
    d[a.len()][b.len()]
}

/// How many typos a term may carry and still be recognised: one for
/// numbers of 4+ digits and words of 5+ letters, two for words of 8+.
/// Shorter terms are left alone, too many words sit one letter apart.
fn allowed_typos(term: &str) -> usize {
    let len = js_len(term);
    if is_number(term) {
        usize::from(len >= 4)
    } else if len >= 8 {
        2
    } else {
        usize::from(len >= 5)
    }
}

/// A query term no instruction has, and the closest word or number one does.
struct Correction {
    typed: String,
    meant: String,
}

/// The user's misspelled words and mistyped numbers, each mapped to the
/// closest one the instructions actually use.
fn corrections(texts: &[Text], terms: &[String]) -> Vec<Correction> {
    let missing: Vec<&String> = terms
        .iter()
        .filter(|t| allowed_typos(t) > 0)
        .filter(|t| {
            texts
                .iter()
                .all(|x| count_words(t, &x.title) == 0 && count_words(t, &x.body) == 0)
        })
        .collect();
    if missing.is_empty() {
        return Vec::new();
    }
    let words: HashSet<&str> = texts
        .iter()
        .flat_map(|x| split_non_word(&x.title).into_iter().chain(split_non_word(&x.body)))
        .filter(|w| js_len(w) >= 3)
        .collect();
    missing
        .into_iter()
        .filter_map(|typed| {
            let max = allowed_typos(typed);
            let typed_chars: Vec<char> = typed.chars().collect();
            words
                .iter()
                .filter(|w| is_number(w) == is_number(typed))
                .filter(|w| w.chars().count().abs_diff(typed_chars.len()) <= max)
                .map(|w| (typo_distance(&typed_chars, &w.chars().collect::<Vec<_>>()), *w))
                .filter(|(d, _)| *d <= max)
                .min()
                .map(|(_, meant)| Correction {
                    typed: typed.clone(),
                    meant: meant.to_string(),
                })
        })
        .collect()
}

/// Query terms with each typo replaced by what the user meant.
fn corrected_terms(texts: &[Text], terms: Vec<String>) -> Vec<String> {
    let fixes = corrections(texts, &terms);
    terms
        .into_iter()
        .map(|t| {
            fixes
                .iter()
                .find(|c| c.typed == t)
                .map_or(t, |c| c.meant.clone())
        })
        .collect()
}

/// "15499" → "15,499"; words stay as they are.
fn readable(term: &str) -> String {
    if !is_number(term) {
        return term.to_string();
    }
    let mut out = String::new();
    for (i, c) in term.chars().enumerate() {
        if i > 0 && (term.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// A user who mistyped something ("$154999" for "$15,499", "calacata" for
/// "calacatta") gets the closest match instead of a question back.
fn typo_hint(texts: &[Text], query: &str) -> String {
    let fixes = corrections(texts, &query_terms(query));
    if fixes.is_empty() {
        return String::new();
    }
    let list: Vec<String> = fixes
        .iter()
        .map(|c| format!("\"{}\" → \"{}\"", readable(&c.typed), readable(&c.meant)))
        .collect();
    format!(
        "LIKELY TYPOS: No instruction has what the user typed, but they have something one or two characters off: {}. Unless a correction plainly does not fit the question, treat it as what the user meant and answer without asking them to confirm. Start with one short line saying what you used, like \"Showing **{}** (closest match to {}).\"\n\n",
        list.join(", "),
        readable(&fixes[0].meant),
        readable(&fixes[0].typed),
    )
}

/// Lowercased title and plain body of one instruction, worked out once.
struct Text {
    title: String,
    body: String,
    plain_len: usize,
}

impl Text {
    fn of(instruction: &Instruction) -> Self {
        let plain = html_to_plain_text(&instruction.rich_text);
        Self {
            title: instruction
                .title
                .as_deref()
                .unwrap_or_default()
                .to_lowercase(),
            body: join_digit_groups(&plain.to_lowercase()),
            plain_len: js_len(&plain),
        }
    }
}

fn count_occurrences(term: &str, haystack: &str) -> usize {
    if term.is_empty() || haystack.is_empty() {
        return 0;
    }
    if js_len(term) <= 4 {
        return count_words(term, haystack);
    }
    let count = haystack.matches(term).count();
    if count == 0 && js_len(term) >= 6 {
        let units: Vec<u16> = term.encode_utf16().collect();
        let keep = 5.max(units.len() - 2);
        let stem = String::from_utf16_lossy(&units[..keep.min(units.len())]);
        if js_len(&stem) >= 5 && haystack.contains(&stem) {
            return 1;
        }
    }
    count
}

/// Matches of `/\bterm\b/gi` in a lowercase haystack (terms are ASCII
/// word characters, so occurrences inside a match can never stand alone).
fn count_words(term: &str, haystack: &str) -> usize {
    let word = |b: Option<&u8>| b.is_some_and(|b| b.is_ascii_alphanumeric() || *b == b'_');
    let bytes = haystack.as_bytes();
    let starts_word = word(term.as_bytes().first());
    let ends_word = word(term.as_bytes().last());
    haystack
        .match_indices(term)
        .filter(|(at, _)| {
            let before = at.checked_sub(1).and_then(|i| bytes.get(i));
            let after = bytes.get(at + term.len());
            word(before) != starts_word && word(after) != ends_word
        })
        .count()
}

fn count_matching(terms: &[String], haystack: &str) -> usize {
    if haystack.is_empty() {
        return 0;
    }
    terms
        .iter()
        .filter(|t| count_occurrences(t, haystack) > 0)
        .count()
}

fn frequency(terms: &[String], haystack: &str) -> usize {
    terms
        .iter()
        .map(|t| count_occurrences(t, haystack))
        .filter(|n| *n > 1)
        .map(|n| 8.min(n - 1))
        .sum()
}

#[derive(Debug, Clone, Copy)]
struct Scored {
    index: usize,
    title: usize,
    body: usize,
    frequency: usize,
}

fn score_all(texts: &[Text], query: &str) -> Vec<Scored> {
    let terms = query_terms(query);
    if terms.is_empty() {
        return Vec::new();
    }
    texts
        .iter()
        .enumerate()
        .map(|(index, t)| Scored {
            index,
            title: count_matching(&terms, &t.title),
            body: count_matching(&terms, &t.body),
            frequency: frequency(&terms, &t.title) + frequency(&terms, &t.body),
        })
        .collect()
}

const PRICE_SIGNAL_TERMS: [&str; 14] = [
    "price",
    "prices",
    "cost",
    "costs",
    "fee",
    "fees",
    "rate",
    "rates",
    "charge",
    "charges",
    "fabrication",
    "pricing",
    "sqft",
    "sqft",
];

pub fn is_price_query(query: &str) -> bool {
    let lower = query.to_lowercase();
    PRICE_SIGNAL_TERMS.iter().any(|t| lower.contains(t))
}

pub fn is_procedure_query(query: &str) -> bool {
    let lower = query.to_lowercase();
    [
        re!(r"\bwhat (?:do|should) i do\b"),
        re!(r"\bhow (?:do|should|can) i\b"),
        re!(r"\bhow to\b"),
        re!(r"\bwhat are the steps\b"),
        re!(r"\bwhat steps\b"),
        re!(r"\bwalk me through\b"),
        re!(r"\bstep by step\b"),
        re!(r"\bprocess (?:for|when|if)\b"),
        re!(r"\bprocedure (?:for|when|if)\b"),
        re!(r"\bwhat (?:happens|do i need to do) when\b"),
        re!(r"\bwhat do i need to do\b"),
    ]
    .iter()
    .any(|p| p.is_match(&lower))
}

pub const PROCEDURE_QUERY_HINT: &str = "PROCEDURE QUERY: The user needs a complete step-by-step checklist for this scenario. Include every step, field, requirement, color code, document, reminder, and follow-up action written under the matching scenario in the instruction BODY — not only the main actions. If the scenario covers creating events and also what to enter on those events, include both in this answer so the user does not need a follow-up question.\n\n";

pub fn is_video_or_link_query(query: &str) -> bool {
    let lower = query.to_lowercase();
    [
        re!(r"\bvideo\b"),
        re!(r"\byoutube\b"),
        re!(r"\btutorial\b"),
        re!(r"\bhyperlink\b"),
        re!(r"\blink\b"),
        re!(r"\bwatch\b"),
        re!(r"\bshort\b"),
    ]
    .iter()
    .any(|p| p.is_match(&lower))
}

fn has_url(text: &str) -> bool {
    re!(r"https?://[^\s)]+", "i").is_match(text)
}

fn url_snippets(body: &str) -> Vec<String> {
    re!(r"\n+")
        .split(body)
        .filter(|line| has_url(line))
        .map(|line| trim(&re!(r"\s+").replace_all(line, " ")).to_string())
        .filter(|line| !line.is_empty())
        .collect()
}

const VIDEO_WORDS: [&str; 11] = [
    "video",
    "videos",
    "youtube",
    "link",
    "links",
    "tutorial",
    "watch",
    "short",
    "shorts",
    "hyperlink",
    "find",
];

fn topic_terms(terms: &[String], drop: &[&str]) -> Vec<String> {
    let topic: Vec<String> = terms
        .iter()
        .filter(|t| !drop.contains(&t.as_str()))
        .cloned()
        .collect();
    if topic.is_empty() {
        terms.to_vec()
    } else {
        topic
    }
}

fn video_relevance(text: &Text, terms: &[String]) -> usize {
    if !has_url(&text.body) {
        return 0;
    }
    let snippets = url_snippets(&text.body);
    let match_terms = topic_terms(terms, &VIDEO_WORDS);
    snippets
        .iter()
        .map(|s| count_matching(&match_terms, &s.to_lowercase()))
        .max()
        .unwrap_or(0)
}

fn video_link_hint(instructions: &[Instruction], texts: &[Text], query: &str) -> String {
    if !is_video_or_link_query(query) {
        return String::new();
    }
    let terms = query_terms(query);
    let mut ranked: Vec<(usize, usize)> = texts
        .iter()
        .enumerate()
        .map(|(i, t)| (i, video_relevance(t, &terms)))
        .filter(|(_, score)| *score > 0)
        .collect();
    ranked.sort_by_key(|r| std::cmp::Reverse(r.1));
    let match_terms = topic_terms(&terms, &VIDEO_WORDS[..10]);
    let focus = ranked.first().map_or_else(String::new, |(index, _)| {
        let mut lines: Vec<String> =
            url_snippets(&html_to_plain_text(&instructions[*index].rich_text))
                .into_iter()
                .filter(|line| count_matching(&match_terms, &line.to_lowercase()) > 0)
                .collect();
        lines.sort_by_key(|line| std::cmp::Reverse(re!(r"^video\s*:", "i").is_match(line)));
        lines.truncate(3);
        format!(
            "Check Instruction {} first{}.\n\n",
            index + 1,
            if lines.is_empty() {
                String::new()
            } else {
                format!(" — matching link lines:\n- {}", lines.join("\n- "))
            }
        )
    });
    format!(
        "VIDEO/LINK QUERY: The user wants a video or link from the instructions. Search BODIES for lines that start with \"Video:\" or contain http/https URLs. When you find a match, include the full URL exactly in your answer. Never replace a URL with placeholders like {{hyperlink}}, [link], or \"hyperlink\".\n\n{focus}"
    )
}

fn has_pricing(body: &str) -> bool {
    re!(r"\$\s*\d").is_match(body) || re!(r"(?:per|/)\s*sq\.?\s*ft").is_match(body)
}

fn price_relevance(text: &Text, terms: &[String]) -> usize {
    if !has_pricing(&text.body) {
        return 0;
    }
    let mut score = count_matching(terms, &text.body);
    if terms.iter().any(|t| t.contains("fabric")) && text.body.contains("fabrication") {
        score += 2;
    }
    score
}

fn score_for_query(text: &Text, query: &str, terms: &[String]) -> usize {
    let mut score = count_matching(terms, &text.title) * 3
        + count_matching(terms, &text.body)
        + frequency(terms, &text.title)
        + frequency(terms, &text.body);
    if is_video_or_link_query(query) {
        let video = video_relevance(text, terms);
        if video > 0 {
            score += 20 + video * 5;
        }
    }
    if is_price_query(query) {
        let price = price_relevance(text, terms);
        if price > 0 {
            score += 10 + price;
        }
    }
    score
}

const CONTEXT_CHAR_BUDGET: usize = 90_000;

#[derive(Default)]
struct Picked {
    selected: Vec<usize>,
    seen: HashSet<i32>,
    used: usize,
}

impl Picked {
    /// Takes `(index, score, length)` items in order; with `require_fit`
    /// only those that still fit the budget.
    fn take(
        &mut self,
        instructions: &[Instruction],
        items: &[(usize, usize, usize)],
        require_fit: bool,
        budget: usize,
    ) {
        for (i, _, len) in items {
            if self.seen.contains(&instructions[*i].id) {
                continue;
            }
            if require_fit && self.used > 0 && self.used + len > budget {
                continue;
            }
            self.selected.push(*i);
            self.seen.insert(instructions[*i].id);
            self.used += len;
            if self.used >= budget {
                return;
            }
        }
    }
}

/// `selectInstructionsForChatContext`: the best matches first, within the
/// budget, then the rest in order while they fit.
pub fn select_for_context(instructions: &[Instruction], query: &str) -> Vec<Instruction> {
    if instructions.is_empty() {
        return Vec::new();
    }
    let budget = CONTEXT_CHAR_BUDGET;
    let video = is_video_or_link_query(query);
    let texts: Vec<Text> = instructions.iter().map(Text::of).collect();
    // Typos ("speical", "$154999") count as what the user meant.
    let terms = corrected_terms(&texts, query_terms(query));
    let ranked: Vec<(usize, usize, usize)> = texts
        .iter()
        .enumerate()
        .map(|(i, t)| (i, score_for_query(t, query, &terms), t.plain_len.max(1)))
        .collect();
    let mut relevant: Vec<(usize, usize, usize)> = ranked
        .iter()
        .copied()
        .filter(|(i, score, _)| *score > 0 && (!video || video_relevance(&texts[*i], &terms) > 0))
        .collect();
    relevant.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    let fillers: Vec<(usize, usize, usize)> = ranked.iter().copied().filter(|r| r.1 == 0).collect();

    let mut picked = Picked::default();
    // An instruction with the exact number asked about ("$15,499") is the
    // answer however long it is, so it goes in first.
    let with_number: Vec<_> = relevant
        .iter()
        .copied()
        .filter(|r| has_query_number(&texts[r.0], &terms))
        .collect();
    picked.take(instructions, &with_number, false, budget);
    let share = if relevant.len() > 1 {
        12_000.max(budget / relevant.len())
    } else {
        budget
    };
    let small: Vec<_> = relevant.iter().copied().filter(|r| r.2 <= share).collect();
    picked.take(instructions, &small, false, budget);
    if picked.used < budget {
        picked.take(instructions, &relevant, true, budget);
    }
    if picked.used < budget && !video {
        picked.take(instructions, &fillers, true, budget);
    }
    let selected = picked.selected;
    if selected.is_empty() {
        return instructions.iter().take(5).cloned().collect();
    }
    selected
        .into_iter()
        .map(|i| instructions[i].clone())
        .collect()
}

fn pick_best_matches(texts: &[Text], scored: &[Scored], query: &str) -> Vec<usize> {
    if scored.is_empty() {
        return Vec::new();
    }
    if is_price_query(query) {
        let terms = query_terms(query);
        let prices: Vec<(usize, usize)> = scored
            .iter()
            .map(|s| (s.index, price_relevance(&texts[s.index], &terms)))
            .collect();
        let max = prices.iter().map(|p| p.1).max().unwrap_or(0);
        if max > 0 {
            return prices
                .into_iter()
                .filter(|p| p.1 == max)
                .map(|p| p.0)
                .collect();
        }
    }
    let best = |items: Vec<&Scored>| -> Vec<usize> {
        let max_frequency = items.iter().map(|s| s.frequency).max().unwrap_or(0);
        items
            .into_iter()
            .filter(|s| s.frequency == max_frequency)
            .map(|s| s.index)
            .collect()
    };
    let max_title = scored.iter().map(|s| s.title).max().unwrap_or(0);
    if max_title > 0 {
        return best(scored.iter().filter(|s| s.title == max_title).collect());
    }
    let max_body = scored.iter().map(|s| s.body).max().unwrap_or(0);
    if max_body == 0 {
        return Vec::new();
    }
    best(scored.iter().filter(|s| s.body == max_body).collect())
}

fn matched(instruction: &Instruction) -> MatchedInstruction {
    MatchedInstruction {
        id: instruction.id,
        title: instruction
            .title
            .as_deref()
            .map(trim)
            .filter(|t| !t.is_empty())
            .map_or_else(
                || format!("Instruction #{}", instruction.id),
                str::to_string,
            ),
    }
}

/// `buildPriceQueryHint`.
fn price_query_hint(instructions: &[Instruction], texts: &[Text], query: &str) -> String {
    if !is_price_query(query) {
        return String::new();
    }
    let matches = pick_best_matches(texts, &score_all(texts, query), query);
    matches.first().map_or_else(String::new, |i| {
        let id = instructions[*i].id;
        let index = instructions.iter().position(|x| x.id == id).unwrap_or(*i);
        format!("PRICE QUERY: Check Instruction {} first — its body contains pricing relevant to this question.\n\n", index + 1)
    })
}

/// `buildPriceQueryHint` and `buildVideoLinkQueryHint` for `instructions`.
pub fn hints(instructions: &[Instruction], query: &str) -> (String, String) {
    let texts: Vec<Text> = instructions.iter().map(Text::of).collect();
    (
        price_query_hint(instructions, &texts, query),
        video_link_hint(instructions, &texts, query),
    )
}

/// `buildInstructionContext`: the instructions the model gets, numbered.
pub fn instruction_context(instructions: &[Instruction], query: &str) -> String {
    let selected = select_for_context(instructions, query);
    let texts: Vec<Text> = selected.iter().map(Text::of).collect();
    let entries: Vec<String> = selected
        .iter()
        .enumerate()
        .map(|(index, i)| {
            let title = i
                .title
                .as_deref()
                .map(trim)
                .filter(|t| !t.is_empty())
                .map_or_else(|| format!("Untitled #{}", i.id), str::to_string);
            let body = html_to_plain_text(&i.rich_text);
            let body = trim(&body);
            format!(
                "Instruction {}\nTITLE: {title}\nBODY: {}",
                index + 1,
                if body.is_empty() { "(empty)" } else { body }
            )
        })
        .collect();
    let procedure = if is_procedure_query(query) {
        PROCEDURE_QUERY_HINT
    } else {
        ""
    };
    format!(
        "{}{}{procedure}{}SEARCH ALL INSTRUCTIONS: Check every BODY below before answering. Details are often in a later instruction, not the first one that merely mentions the topic. A passing mention (for example \"ACH is an accepted payment method\") is not the answer if another BODY has the actual details (account numbers, routing, rates, steps, contacts). Only say the information is missing after checking every instruction.\n\nHere are the instructions you can use, each TITLE is linked to its own BODY:\n\n{}",
        typo_hint(&texts, query),
        price_query_hint(&selected, &texts, query),
        video_link_hint(&selected, &texts, query),
        entries.join("\n\n")
    )
}

/// `resolveInstructionsUsedForReply`: the cited instructions the answer
/// really draws on.
pub fn used_for_reply(
    instructions: &[Instruction],
    answer: &str,
    query: &str,
) -> Vec<MatchedInstruction> {
    let Some(Some(indices)) = parse_instruction_indices(answer) else {
        return Vec::new();
    };
    let clean = strip_markers(answer);
    let score_text = if clean.is_empty() {
        query.to_string()
    } else {
        format!("{query}\n{clean}")
    };
    let texts: Vec<Text> = instructions.iter().map(Text::of).collect();
    let scored = score_all(&texts, &score_text);
    let max_title = scored.iter().map(|s| s.title).max().unwrap_or(0);
    let max_body = scored.iter().map(|s| s.body).max().unwrap_or(0);
    let body_evidence = max_body >= 3;
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss
    )]
    let body_floor = 3.max((max_body as f64 * 0.6).ceil() as usize);
    let price = is_price_query(query);
    let terms = query_terms(query);
    let mut out: Vec<MatchedInstruction> = Vec::new();
    for index in indices {
        let Some(instruction) = index.checked_sub(1).and_then(|i| instructions.get(i)) else {
            continue;
        };
        let Some(item) = scored.iter().find(|s| s.index == index - 1) else {
            continue;
        };
        if out.iter().any(|m| m.id == instruction.id) {
            continue;
        }
        if price && price_relevance(&texts[item.index], &terms) > 0 {
            out.push(matched(instruction));
            continue;
        }
        let acceptable = if body_evidence {
            item.body >= body_floor
        } else {
            (max_title > 0 && item.title == max_title) || item.body >= 2
        };
        if acceptable {
            out.push(matched(instruction));
        }
    }
    out
}

/// `findBestMatchingInstruction`.
pub fn best_matching(instructions: &[Instruction], query: &str) -> Option<MatchedInstruction> {
    let texts: Vec<Text> = instructions.iter().map(Text::of).collect();
    pick_best_matches(&texts, &score_all(&texts, query), query)
        .first()
        .map(|i| matched(&instructions[*i]))
}

/// `isInstructionRelatedToQuery`.
pub fn is_related(instruction: &Instruction, query: &str) -> bool {
    let terms = query_terms(query);
    if terms.is_empty() {
        return false;
    }
    let text = Text::of(instruction);
    count_matching(&terms, &text.title) > 0 || count_matching(&terms, &text.body) > 0
}

/// `instructionIdsToExcludeForRetry`.
pub fn ids_to_exclude_for_retry(selected: &[Instruction], answer: &str, query: &str) -> Vec<i32> {
    let used = used_for_reply(selected, answer, query);
    if !used.is_empty() {
        return used.iter().map(|m| m.id).collect();
    }
    if let Some(Some(indices)) = parse_instruction_indices(answer) {
        let ids: Vec<i32> = indices
            .iter()
            .filter_map(|i| i.checked_sub(1).and_then(|i| selected.get(i)))
            .map(|i| i.id)
            .collect();
        if !ids.is_empty() {
            return ids;
        }
    }
    selected.first().map(|i| vec![i.id]).unwrap_or_default()
}

enum Block {
    Text(String),
    Image(String),
}

fn blocks(html: &str) -> Vec<Block> {
    if html.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    let tag = re!(r"<img\b[^>]*>", "gi");
    let mut last = 0;
    let mut segments: Vec<&str> = Vec::new();
    for m in tag.find_iter(html) {
        segments.push(&html[last..m.start()]);
        segments.push(m.as_str());
        last = m.end();
    }
    segments.push(&html[last..]);
    for segment in segments {
        if segment.is_empty() {
            continue;
        }
        if re!(r"^<img\b", "i").is_match(segment) {
            if let Some(c) = re!(r#"\bsrc=["']([^"']+)["']"#, "i").captures(segment) {
                let url = trim(&c[1]).to_string();
                if !url.is_empty() {
                    out.push(Block::Image(url));
                }
            }
            continue;
        }
        let text = html_to_plain_text(segment);
        for line in re!(r"\n+").split(&text) {
            let line = trim(line);
            if !line.is_empty() {
                out.push(Block::Text(line.to_string()));
            }
        }
    }
    out
}

fn match_tokens(text: &str) -> Vec<String> {
    re!(r"[^a-z0-9]+")
        .split(&text.to_lowercase())
        .filter(|t| js_len(t) >= 4 && !is_stop_word(t))
        .map(str::to_string)
        .collect()
}

fn block_strength(text: &str, query_tokens: &HashSet<String>) -> usize {
    let tokens = match_tokens(text);
    if tokens.is_empty() {
        return 0;
    }
    let unique: HashSet<&String> = tokens.iter().collect();
    let shared = unique.iter().filter(|t| query_tokens.contains(**t)).count();
    #[allow(clippy::cast_precision_loss)]
    let ratio = shared as f64 / unique.len() as f64;
    if shared >= 2 || (shared >= 1 && ratio >= 0.5) {
        shared
    } else {
        0
    }
}

/// `collectImagesForAnswerInInstruction`: pictures beside the part of the
/// instruction the answer comes from.
fn images_for_answer(instruction: &Instruction, answer: &str, query: &str) -> Vec<String> {
    let blocks = blocks(&instruction.rich_text);
    if blocks.is_empty() {
        return Vec::new();
    }
    let query_tokens: HashSet<String> = match_tokens(&format!("{answer}\n{query}"))
        .into_iter()
        .collect();
    if query_tokens.is_empty() {
        return Vec::new();
    }
    // (block index, text ordinal, strength)
    let mut found: Vec<(usize, usize, usize)> = Vec::new();
    let mut ordinal = 0;
    for (index, block) in blocks.iter().enumerate() {
        let Block::Text(text) = block else { continue };
        let strength = block_strength(text, &query_tokens);
        if strength > 0 {
            found.push((index, ordinal, strength));
        }
        ordinal += 1;
    }
    if found.is_empty() {
        return Vec::new();
    }
    let mut strongest = 0;
    for i in 1..found.len() {
        if found[i].2 > found[strongest].2 {
            strongest = i;
        }
    }
    let (mut start, mut end) = (strongest, strongest);
    while end + 1 < found.len() && found[end + 1].1 - found[end].1 - 1 <= 1 {
        end += 1;
    }
    while start > 0 && found[start].1 - found[start - 1].1 - 1 <= 1 {
        start -= 1;
    }
    let (first, last) = (found[start].0, found[end].0);
    let mut urls: Vec<String> = Vec::new();
    let mut push = |url: &str| {
        if !url.is_empty() && !urls.iter().any(|u| u == url) {
            urls.push(url.to_string());
        }
    };
    for block in &blocks[first + 1..last.max(first + 1)] {
        if let Block::Image(url) = block {
            push(url);
        }
    }
    if let Some(Block::Image(url)) = blocks.get(last + 1) {
        push(url);
    }
    urls
}

/// `collectImagesForUsedInstructions`.
pub fn images_for_used(
    instructions: &[Instruction],
    used: &[MatchedInstruction],
    answer: &str,
    query: &str,
) -> Vec<String> {
    let mut urls: Vec<String> = Vec::new();
    for m in used {
        let Some(instruction) = instructions.iter().rev().find(|i| i.id == m.id) else {
            continue;
        };
        for url in images_for_answer(instruction, answer, query) {
            if !urls.contains(&url) {
                urls.push(url);
            }
        }
    }
    urls
}

/// `findInstructionMatchingAnswer`: the instruction whose body covers most
/// of the answer's words (75% at least).
pub fn matching_answer(
    instructions: &[Instruction],
    answer: &str,
    query: &str,
) -> Option<MatchedInstruction> {
    let answer_tokens: HashSet<String> = match_tokens(&format!("{answer}\n{query}"))
        .into_iter()
        .collect();
    if answer_tokens.len() < 5 {
        return None;
    }
    let mut best: Option<(usize, f64)> = None;
    for (index, instruction) in instructions.iter().enumerate() {
        let body: HashSet<String> = match_tokens(&Text::of(instruction).body)
            .into_iter()
            .collect();
        if body.is_empty() {
            continue;
        }
        let covered = answer_tokens.iter().filter(|t| body.contains(*t)).count();
        #[allow(clippy::cast_precision_loss)]
        let coverage = covered as f64 / answer_tokens.len() as f64;
        if best.is_none_or(|(_, c)| coverage > c) {
            best = Some((index, coverage));
        }
    }
    let (index, coverage) = best?;
    (coverage >= 0.75).then(|| matched(&instructions[index]))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn instruction(id: i32, title: &str, body: &str) -> Instruction {
        Instruction {
            id,
            title: Some(title.into()),
            rich_text: format!("<p>{body}</p>"),
        }
    }

    #[test]
    fn a_price_is_found_with_or_without_its_comma() {
        let filler = "special order notes ".repeat(600);
        let mut all: Vec<Instruction> = (1..=10)
            .map(|id| instruction(id, &format!("Handbook {id}"), &filler))
            .collect();
        let packages = format!(
            "{} Two Full Remodel packages: $11,999 and $15,499.",
            "quote rules ".repeat(3000)
        );
        all.push(instruction(96, "Handbook - 3: Quotes", &packages));

        for query in [
            "tell me everything about special $15499",
            "tell me everything about special $15,499",
        ] {
            let selected = select_for_context(&all, query);
            assert_eq!(selected[0].id, 96, "{query}");
        }
        assert_eq!(join_digit_groups("$1,234,567 and 12,34"), "$1234567 and 12,34");
    }

    #[test]
    fn a_price_one_typo_off_is_found() {
        let filler = "special order notes ".repeat(600);
        let mut all: Vec<Instruction> = (1..=10)
            .map(|id| instruction(id, &format!("Handbook {id}"), &filler))
            .collect();
        let packages = format!(
            "{} Two Full Remodel packages: $11,999 and $15,499.",
            "quote rules ".repeat(3000)
        );
        all.push(instruction(96, "Handbook - 3: Quotes", &packages));

        for query in [
            "Show me the special for $154999",
            "Show me the special for $1549",
            "Show me the special for $15599",
            "Show me the special for $14599",
        ] {
            let selected = select_for_context(&all, query);
            assert_eq!(selected[0].id, 96, "{query}");
            let context = instruction_context(&all, query);
            assert!(context.contains("LIKELY TYPOS"), "{query}");
            assert!(context.contains("→ \"15,499\""), "{query}");
        }
        let exact = instruction_context(&all, "Show me the special for $15499");
        assert!(!exact.contains("LIKELY TYPOS"));
        let chars = |s: &str| s.chars().collect::<Vec<_>>();
        assert_eq!(typo_distance(&chars("154999"), &chars("11999")), 2);
    }

    #[test]
    fn a_misspelled_word_is_found() {
        let filler = "general shop notes ".repeat(600);
        let mut all: Vec<Instruction> = (1..=10)
            .map(|id| instruction(id, &format!("Handbook {id}"), &filler))
            .collect();
        all.push(instruction(
            42,
            "Calacatta Vagli",
            "Calacatta Vagli is cut on the waterjet only.",
        ));

        for query in ["how do we cut calacata vagli", "how do we cut calcatta vagil"] {
            let selected = select_for_context(&all, query);
            assert_eq!(selected[0].id, 42, "{query}");
            let context = instruction_context(&all, query);
            assert!(context.contains("→ \"calacatta\""), "{query}");
        }
        let exact = instruction_context(&all, "how do we cut calacatta vagli");
        assert!(!exact.contains("LIKELY TYPOS"));
    }
}
