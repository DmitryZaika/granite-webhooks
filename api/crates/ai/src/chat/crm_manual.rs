//! The CRM user manual in the chat.
//!
//! The sections a question needs (with the super admins' corrections from
//! `crm_manual_overrides`), what the asker may open, canned answers, and
//! when a question is about using the CRM. Ports of `crmManual.server.ts`, `crmManualUserAccess.server.ts`,
//! `crmManualHowToRedirect.ts` and `getCrmManualQuickAnswer`.
//!
//! The model picks the sections from the manual's table of contents
//! ([`pick`]); the topic table and the manual's own words ([`section_ids_for`],
//! typos corrected) are the fallback and its second opinion. A word that
//! names several features ("notifications") is asked back ([`features`]).
//!
//! `crm_manual.json` is `docs/manual/*.md` of the CRM repo plus the topic
//! table of `crmManual.server.ts` and the clarify table;
//! `scripts/sync-crm-manual.mjs` rebuilds it.

use super::instructions::is_price_query;
use crate::ai::{Caller, Chat, MINI_FALLBACK_MODEL, MINI_MODEL};
use crate::auth::session::SessionUser;
use crate::followups::gather::{customer_inventory_page_url, customer_quote_page_url};
use crate::followups::js::trim;
use crate::re;
use crate::state::AppState;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::MySqlPool;
use std::borrow::Cow;
use std::cmp::Reverse;
use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;
use std::time::Duration;

#[derive(Deserialize)]
struct Section {
    id: String,
    content: String,
}

#[derive(Deserialize)]
struct Topic {
    id: String,
    keywords: Vec<String>,
    #[serde(default)]
    related: Vec<String>,
}

/// A word that names several features: the question asked back, and the
/// features it offers.
#[derive(Deserialize)]
struct Ambiguity {
    keywords: Vec<String>,
    question: String,
    /// The question when it can also be the company's own process (the
    /// company has an instruction about it): one question, not two.
    #[serde(default)]
    with_company: Option<String>,
    choices: Vec<AmbiguousFeature>,
}

#[derive(Deserialize)]
struct AmbiguousFeature {
    label: String,
    /// Words that already say this is the feature meant.
    keywords: Vec<String>,
    sections: Vec<String>,
}

#[derive(Deserialize)]
struct Manual {
    intro: String,
    sections: Vec<Section>,
    core: Vec<String>,
    topics: Vec<Topic>,
    fallback: Vec<String>,
    #[serde(default)]
    clarify: Vec<Ambiguity>,
}

static MANUAL: LazyLock<Manual> = LazyLock::new(|| {
    serde_json::from_str(include_str!("crm_manual.json")).expect("crm_manual.json is valid")
});

/// The manual's section ids, in file order.
pub fn section_ids() -> Vec<&'static str> {
    MANUAL.sections.iter().map(|s| s.id.as_str()).collect()
}

/// A section's text as shipped (no override).
pub fn section(id: &str) -> Option<&'static str> {
    MANUAL
        .sections
        .iter()
        .find(|s| s.id == id)
        .map(|s| s.content.as_str())
}

pub fn normalize(text: &str) -> String {
    trim(&re!(r"\s+").replace_all(&text.to_lowercase(), " ")).to_string()
}

/// Whole word, or a phrase as a substring.
fn matches_keyword(text: &str, keyword: &str) -> bool {
    let keyword = keyword.to_lowercase();
    if keyword.contains(' ') {
        return text.contains(&keyword);
    }
    let word = |b: Option<&u8>| b.is_some_and(|b| b.is_ascii_alphanumeric() || *b == b'_');
    let bytes = text.as_bytes();
    let starts = word(keyword.as_bytes().first());
    let ends = word(keyword.as_bytes().last());
    text.match_indices(&keyword).any(|(at, _)| {
        word(at.checked_sub(1).and_then(|i| bytes.get(i))) != starts
            && word(bytes.get(at + keyword.len())) != ends
    })
}

/// The text without "a", "an" and "the", so "add a user" matches "add user".
fn without_articles(text: &str) -> String {
    text.split(' ')
        .filter(|w| !w.is_empty() && !matches!(*w, "a" | "an" | "the"))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Whether `text` (normalized, articles dropped) has one of `keywords`.
fn has_keyword(text: &str, keywords: &[String]) -> bool {
    keywords
        .iter()
        .any(|k| matches_keyword(text, &without_articles(&k.to_lowercase())))
}

/// The topics whose keywords `search` (normalized, typos corrected) has.
fn matched_topics(search: &str) -> impl Iterator<Item = &'static Topic> {
    let normalized = without_articles(search);
    MANUAL
        .topics
        .iter()
        .filter(move |topic| has_keyword(&normalized, &topic.keywords))
}

// ---------- typos ----------

/// The runs of letters and digits in `text`.
fn words(text: &str) -> impl Iterator<Item = &str> {
    text.split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|w| !w.is_empty())
}

/// Every word of the manual and of its topic keywords, with the number of
/// sections that use it: what a typo is corrected to.
static WORDS: LazyLock<HashMap<String, usize>> = LazyLock::new(|| {
    let mut counts: HashMap<String, usize> = HashMap::new();
    for section in &MANUAL.sections {
        let content = section.content.to_lowercase();
        let unique: HashSet<&str> = words(&content).collect();
        for word in unique {
            *counts.entry(word.to_string()).or_default() += 1;
        }
    }
    for keyword in MANUAL.topics.iter().flat_map(|t| &t.keywords) {
        for word in words(&keyword.to_lowercase()) {
            counts.entry(word.to_string()).or_insert(1);
        }
    }
    counts
});

/// Short words people type together with the next one ("aquote").
const JOINED: [&str; 10] = ["a", "an", "the", "to", "my", "our", "of", "in", "on", "for"];

/// Edits (a letter added, dropped, changed, or two letters swapped) from `a`
/// to `b`, when there are at most `most`.
fn distance(a: &[u8], b: &[u8], most: usize) -> Option<usize> {
    if a.len().abs_diff(b.len()) > most {
        return None;
    }
    let mut rows = vec![vec![0usize; b.len() + 1]; a.len() + 1];
    for (i, row) in rows.iter_mut().enumerate() {
        row[0] = i;
    }
    for (j, cell) in rows[0].iter_mut().enumerate() {
        *cell = j;
    }
    for i in 1..=a.len() {
        for j in 1..=b.len() {
            let changed = usize::from(a[i - 1] != b[j - 1]);
            let mut edits = (rows[i - 1][j] + 1)
                .min(rows[i][j - 1] + 1)
                .min(rows[i - 1][j - 1] + changed);
            if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                edits = edits.min(rows[i - 2][j - 2] + 1);
            }
            rows[i][j] = edits;
        }
    }
    let edits = rows[a.len()][b.len()];
    (edits <= most).then_some(edits)
}

/// A word the manual doesn't know, as meant: two words typed together
/// ("aquote" → "a quote"), else the manual's closest word, one letter off or
/// two for a long word ("specail" → "special"), else as typed.
fn correct_word(word: &str) -> Cow<'_, str> {
    let known = |w: &str| WORDS.contains_key(w);
    if word.len() < 4 || known(word) || word.bytes().any(|b| b.is_ascii_digit()) {
        return Cow::Borrowed(word);
    }
    let split = (1..word.len() - 2)
        .map(|at| word.split_at(at))
        .filter(|(head, tail)| {
            (JOINED.contains(head) || (head.len() >= 3 && known(head)))
                && tail.len() >= 3
                && known(tail)
        })
        .max_by_key(|(_, tail)| WORDS[*tail]);
    if let Some((head, tail)) = split {
        return Cow::Owned(format!("{head} {tail}"));
    }
    let most = if word.len() >= 8 { 2 } else { 1 };
    let first = word.as_bytes()[0];
    WORDS
        .iter()
        .filter(|(known, _)| known.len() >= 3)
        .filter_map(|(known, count)| {
            let edits = distance(word.as_bytes(), known.as_bytes(), most)?;
            Some((edits, known.as_bytes()[0] != first, Reverse(*count), known))
        })
        .min()
        .map_or(Cow::Borrowed(word), |(.., known)| Cow::Owned(known.clone()))
}

/// `normalized` with the words the manual doesn't know corrected
/// ([`correct_word`]): people type fast.
pub fn corrected(normalized: &str) -> String {
    re!(r"[a-z0-9]+")
        .replace_all(normalized, |found: &regex::Captures| {
            correct_word(&found[0]).into_owned()
        })
        .into_owned()
}

// ---------- the manual's own words ----------

/// Words that say nothing about which section is meant.
const STOP_WORDS: [&str; 52] = [
    "a", "an", "the", "to", "of", "in", "on", "for", "and", "or", "how", "do", "does", "did", "i",
    "we", "you", "my", "our", "your", "me", "us", "is", "are", "was", "be", "it", "this", "that",
    "with", "from", "at", "by", "as", "what", "where", "when", "why", "who", "which", "there",
    "way", "can", "could", "would", "should", "will", "if", "so", "not", "no", "please",
];

/// `word` without a plural ending ("quotes" → "quote", "activities" → "activity").
fn stem(word: &str) -> String {
    if word.len() > 4 && word.ends_with("ies") {
        return format!("{}y", &word[..word.len() - 3]);
    }
    if word.len() > 3
        && word.ends_with('s')
        && !["ss", "us", "is"].iter().any(|e| word.ends_with(e))
    {
        return word[..word.len() - 1].to_string();
    }
    word.to_string()
}

/// The stems of a section's headings, of its bold labels and of all its text.
struct Terms {
    id: &'static str,
    headings: HashSet<String>,
    labels: HashSet<String>,
    text: HashSet<String>,
}

fn stems(text: &str) -> HashSet<String> {
    words(&text.to_lowercase()).map(stem).collect()
}

static TERMS: LazyLock<Vec<Terms>> = LazyLock::new(|| {
    MANUAL
        .sections
        .iter()
        .map(|s| {
            let headings: Vec<&str> = s.content.lines().filter(|l| l.starts_with('#')).collect();
            let labels: Vec<&str> = s.content.split("**").skip(1).step_by(2).collect();
            Terms {
                id: s.id.as_str(),
                headings: stems(&headings.join(" ")),
                labels: stems(&labels.join(" ")),
                text: stems(&s.content),
            }
        })
        .collect()
});

/// How many sections use each stem.
static SECTIONS_WITH: LazyLock<HashMap<String, usize>> = LazyLock::new(|| {
    let mut counts: HashMap<String, usize> = HashMap::new();
    for terms in TERMS.iter() {
        for stem in &terms.text {
            *counts.entry(stem.clone()).or_default() += 1;
        }
    }
    counts
});

/// The sections other than the core ones whose headings, labels and text
/// have `search`'s words (normalized, typos corrected), best first: a word
/// few sections use counts more, and most in a heading.
fn ranked(search: &str) -> Vec<(&'static str, f64)> {
    let asked: HashSet<String> = words(search)
        .filter(|w| !STOP_WORDS.contains(w))
        .map(stem)
        .collect();
    #[allow(clippy::cast_precision_loss)]
    let sections = TERMS.len() as f64;
    let mut ranked: Vec<(&'static str, f64)> = TERMS
        .iter()
        .filter(|t| !MANUAL.core.iter().any(|id| id == t.id))
        .map(|terms| {
            let score = asked
                .iter()
                .map(|stem| {
                    #[allow(clippy::cast_precision_loss)]
                    let using = SECTIONS_WITH.get(stem).copied().unwrap_or(0) as f64;
                    let rarity = ((sections + 1.0) / (using + 0.5)).ln().max(0.0);
                    let weight = if terms.headings.contains(stem) {
                        3.0
                    } else if terms.labels.contains(stem) {
                        1.5
                    } else if terms.text.contains(stem) {
                        0.5
                    } else {
                        0.0
                    };
                    rarity * weight
                })
                .sum::<f64>();
            (terms.id, score)
        })
        .filter(|(_, score)| *score > 0.0)
        .collect();
    ranked.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(b.0)));
    ranked
}

/// `getCrmManualSectionIdsForQuery`: the core sections, then the ones the
/// topic table and the manual's own words pick, best first (typos
/// corrected); the fallback sections when nothing does.
pub fn section_ids_for(search: &str) -> Vec<String> {
    let search = corrected(&normalize(search));
    let ranked = ranked(&search);
    let score = |id: &str| {
        ranked
            .iter()
            .find(|(r, _)| *r == id)
            .map_or(0.0, |(_, score)| *score)
    };
    let mut picked: Vec<&str> = Vec::new();
    let mut add = |id: &'static str| {
        if !picked.contains(&id) && !MANUAL.core.iter().any(|c| c == id) {
            picked.push(id);
        }
    };
    let mut named: Vec<&str> = Vec::new();
    for topic in matched_topics(&search) {
        named.push(&topic.id);
        add(&topic.id);
        for related in &topic.related {
            add(related);
        }
    }
    // The sections the question's own words point to, when they stand out.
    let best = ranked.first().map_or(0.0, |(_, score)| *score);
    for (id, score) in ranked.iter().take(2) {
        if *score >= 3.0 && *score >= best * 0.5 {
            add(*id);
        }
    }
    // The sections the topic table names first, each lot by the words.
    picked.sort_by(|a, b| {
        named
            .contains(b)
            .cmp(&named.contains(a))
            .then_with(|| score(b).total_cmp(&score(a)))
    });
    let mut selected = MANUAL.core.clone();
    if picked.is_empty() {
        selected.extend(MANUAL.fallback.iter().cloned());
    } else {
        selected.extend(picked.into_iter().map(String::from));
    }
    selected
}

/// Whether a specific manual topic is about `query` (not only the sections
/// every answer gets).
pub fn matches_topic(query: &str) -> bool {
    matched_topics(&corrected(&normalize(query)))
        .next()
        .is_some()
}

/// Super admins' corrections of sections (empty before the table exists).
pub async fn overrides(pool: &MySqlPool) -> HashMap<String, String> {
    sqlx::query_as::<_, (String, String)>("SELECT section_id, content FROM crm_manual_overrides")
        .fetch_all(pool)
        .await
        .map(|rows| rows.into_iter().collect())
        .unwrap_or_default()
}

fn assemble(ids: &[String], overrides: &HashMap<String, String>) -> String {
    let core = &MANUAL.core;
    let mut ordered: Vec<&String> = core.iter().filter(|id| ids.contains(id)).collect();
    ordered.extend(ids.iter().filter(|id| !core.contains(id)));
    let mut seen = std::collections::HashSet::new();
    let mut parts = vec![MANUAL.intro.clone()];
    for id in ordered {
        if !seen.insert(id) {
            continue;
        }
        if let Some(text) = overrides
            .get(id)
            .map(String::as_str)
            .or_else(|| section(id))
        {
            parts.push(text.to_string());
        }
    }
    parts.join("\n\n---\n\n")
}

/// `loadCrmManualContent`: the sections the text is about.
pub async fn content_for(pool: &MySqlPool, search: &str) -> String {
    assemble(&section_ids_for(search), &overrides(pool).await)
}

/// The core sections and `ids`, with the super admins' corrections.
pub async fn content_with(pool: &MySqlPool, ids: &[String]) -> String {
    let mut all = MANUAL.core.clone();
    all.extend(ids.iter().cloned());
    assemble(&all, &overrides(pool).await)
}

// ---------- several features: ask which ----------

/// A feature a question back offers, answered from its sections.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Feature {
    pub label: String,
    /// Always the manual (the `clarify` event's `source`).
    pub source: Source,
    #[serde(skip)]
    pub sections: Vec<String>,
}

/// A question back: which of several features the user means.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Clarification {
    pub question: String,
    pub choices: Vec<Feature>,
    /// The question with the company's own process as one more choice.
    pub with_company: Option<String>,
}

/// What the clarify table makes of a question with one of its words.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Features {
    /// It says which of the features it means: their sections.
    Named(Vec<String>),
    /// It doesn't: ask.
    Ask(Clarification),
}

/// `query` with a word of several features (`clarify` in
/// `crm_manual.json`, like "notifications"): the sections of the ones it
/// names, or the question to ask when it names none.
pub fn features(query: &str) -> Option<Features> {
    let text = without_articles(&corrected(&normalize(query)));
    let entry = MANUAL
        .clarify
        .iter()
        .find(|entry| has_keyword(&text, &entry.keywords))?;
    let named: Vec<&AmbiguousFeature> = entry
        .choices
        .iter()
        .filter(|choice| has_keyword(&text, &choice.keywords))
        .collect();
    if named.is_empty() {
        return Some(Features::Ask(Clarification {
            question: entry.question.clone(),
            choices: entry
                .choices
                .iter()
                .map(|choice| feature(&choice.label, choice.sections.clone()))
                .collect(),
            with_company: entry.with_company.clone(),
        }));
    }
    let mut sections: Vec<String> = Vec::new();
    for id in named.iter().flat_map(|choice| &choice.sections) {
        if !sections.contains(id) {
            sections.push(id.clone());
        }
    }
    Some(Features::Named(sections))
}

/// A feature to offer, answered from `sections`.
pub fn feature(label: &str, sections: Vec<String>) -> Feature {
    Feature {
        label: label.to_string(),
        source: Source::Manual,
        sections,
    }
}

/// The company's own process as one of the choices: answered from its
/// instructions, not from the manual.
pub fn company_choice() -> Feature {
    Feature {
        label: CLARIFY_CHOICES[1].label.to_string(),
        source: Source::Company,
        sections: Vec::new(),
    }
}

/// The choice a reply to a question back picks: its label (a click sends
/// it), the words of one label, or its number. Anything else is a new
/// question.
pub fn feature_choice<'a>(reply: &str, choices: &'a [Feature]) -> Option<&'a Feature> {
    let reply = normalize(&reply.replace('\u{2019}', "'"));
    let reply = trim(reply.trim_end_matches(['.', '!', '?']));
    if let Some(choice) = choices.iter().find(|c| normalize(&c.label) == reply) {
        return Some(choice);
    }
    if let Ok(number) = reply.parse::<usize>() {
        return number.checked_sub(1).and_then(|i| choices.get(i));
    }
    let asked: HashSet<&str> = words(reply).filter(|w| !STOP_WORDS.contains(w)).collect();
    if asked.is_empty() || asked.len() > 4 {
        return None;
    }
    let mut fitting = choices.iter().filter(|choice| {
        let label = normalize(&choice.label);
        let label: HashSet<&str> = words(&label).collect();
        asked.iter().all(|w| label.contains(w))
    });
    match (fitting.next(), fitting.next()) {
        (Some(choice), None) => Some(choice),
        _ => None,
    }
}

// ---------- the model picks the sections ----------

/// The manual's table of contents: each section's id, title and headings.
static CONTENTS: LazyLock<String> = LazyLock::new(|| {
    MANUAL
        .sections
        .iter()
        .filter(|s| !MANUAL.core.contains(&s.id) && s.id != "document-scope")
        .map(|s| {
            let mut headings = s.content.lines().filter(|l| l.starts_with('#')).map(|l| {
                let text = l.trim_start_matches('#').replace(['*', '`'], "");
                trim(&re!(r"\(/[^)]*\)").replace_all(&text, "")).to_string()
            });
            let title = headings.next().unwrap_or_default();
            let rest: Vec<String> = headings.collect();
            format!("[{}] {title}: {}", s.id, rest.join("; "))
        })
        .collect::<Vec<_>>()
        .join("\n")
});

/// How the model picks the sections (the table of contents follows).
const PICK_RULES: &str = r#"You find where the Granite Manager user manual answers a question. Granite Manager is a CRM for countertop and stone shops: customers, deals, quotes with a drawing tool, stone inventory, emails, SMS, calls, schedule, admin settings.

Reply with a JSON object only, in one of these forms:
1. {"sections": ["id", "id"]}: the 1 to 3 sections whose text answers the question, best first. Prefer the section of the page or screen where the task is done.
2. {"clarify": {"question": "...", "choices": [{"label": "...", "sections": ["id"]}]}}: rarely; only when the question fits two or more different features of the app about equally well, their steps differ, and nothing in the question or the earlier questions says which ("how do I add a template": an email, SMS or quote email template). Never ask about parts of one tool, or when one meaning is the usual one in a countertop shop. The question is one short sentence that names the choices; 2 to 5 choices, each label a few words naming the feature as the app names it, with the sections that answer it.
3. {"sections": []}: the question is not about using Granite Manager (the company's own prices, policies or process, or small talk).

Users type fast: read typos and run-together words as the closest app words ("specail" is special, "aquote" is a quote, "deal bord" is the deal board). A follow-up ("and how do I undo it?") is about the same feature as the earlier questions.

The manual's sections, one per line: [id] title: the headings inside it.
"#;

/// What the model picked for a question.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Picked {
    /// The sections that answer it (none: not about using the app).
    Sections(Vec<String>),
    /// It fits several features: ask which.
    Ask(Clarification),
}

fn known_section(id: &str) -> bool {
    MANUAL.sections.iter().any(|s| s.id == id) && !MANUAL.core.iter().any(|c| c == id)
}

/// The known, non-core section ids of `value` (a JSON list), at most `most`.
fn section_list(value: &Value, most: usize) -> Vec<String> {
    let mut ids: Vec<String> = Vec::new();
    for id in value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
    {
        let id = trim(id);
        if known_section(id) && !ids.iter().any(|known| known == id) && ids.len() < most {
            ids.push(id.to_string());
        }
    }
    ids
}

/// The model's reply, checked: only sections the manual has, and a question
/// back only with 2 to 6 choices that each have a label and a section.
pub fn parse_pick(reply: &str) -> Option<Picked> {
    let value: Value = serde_json::from_str(trim(reply)).ok()?;
    if let Some(clarify) = value.get("clarify").filter(|c| c.is_object()) {
        let question = clarify["question"].as_str().map(trim).unwrap_or_default();
        let choices: Vec<Feature> = clarify["choices"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|choice| {
                let label = choice["label"]
                    .as_str()?
                    .replace(['[', ']', '|', '=', ','], " ");
                let label = trim(&re!(r"\s+").replace_all(&label, " ")).to_string();
                let sections = section_list(&choice["sections"], 3);
                (!label.is_empty() && label.len() <= 60 && !sections.is_empty())
                    .then(|| feature(&label, sections))
            })
            .collect();
        if !question.is_empty() && question.len() <= 300 && (2..=6).contains(&choices.len()) {
            return Some(Picked::Ask(Clarification {
                question: question.to_string(),
                choices,
                with_company: None,
            }));
        }
    }
    value
        .get("sections")
        .map(|sections| Picked::Sections(section_list(sections, 3)))
}

/// The model's pick of the sections that answer `question` (`earlier`: the
/// user's questions before it, newest last), or `None` when it fails.
pub async fn pick(
    state: &AppState,
    caller: Caller<'_>,
    question: &str,
    earlier: &[String],
) -> Option<Picked> {
    let earlier: Vec<String> = earlier
        .iter()
        .rev()
        .take(3)
        .rev()
        .map(|q| format!("- {}", trim(q)))
        .collect();
    let asked = if earlier.is_empty() {
        format!("Question: {}", trim(question))
    } else {
        format!(
            "Earlier questions:\n{}\n\nQuestion: {}",
            earlier.join("\n"),
            trim(question)
        )
    };
    let mut chat = Chat::new(MINI_MODEL)
        .message("system", &format!("{PICK_RULES}\n{}", *CONTENTS))
        .message("user", &asked)
        .max_tokens(Some(400));
    chat.json = true;
    // Picking needs no reasoning; it keeps the answer's wait short.
    chat.reasoning_effort = Some("none");
    // Every company sends the same table of contents first.
    chat.prompt_cache_key = Some("crm-manual-pick".into());
    chat.timeout = Duration::from_secs(10);
    let reply = match state.ai.chat(&state.pool, caller, &chat).await {
        Err(err) if err.is_model_permission() => {
            chat.model = MINI_FALLBACK_MODEL;
            state.ai.chat(&state.pool, caller, &chat).await
        }
        other => other,
    };
    parse_pick(&reply.ok()?)
}

/// The sections to answer `question` from (`search`: it and the earlier
/// questions), or the question to ask back: the clarify table's, else the
/// model's pick (or question back, for a feature the topic table doesn't
/// name) with the best section of the manual's own words in case it missed
/// one, else the manual's own words alone. `None`: the model finds the
/// question is not about using the app.
pub async fn sections_for(
    state: &AppState,
    caller: Caller<'_>,
    question: &str,
    earlier: &[String],
    search: &str,
) -> Result<Option<Vec<String>>, Clarification> {
    match features(question) {
        Some(Features::Ask(clarification)) => return Err(clarification),
        Some(Features::Named(ids)) => return Ok(Some(ids)),
        None => {}
    }
    let fallback = section_ids_for(search);
    let own_words = || -> Vec<String> {
        fallback
            .iter()
            .filter(|id| !MANUAL.core.contains(*id))
            .cloned()
            .collect()
    };
    match pick(state, caller, question, earlier).await {
        // A feature the topic table names is not one to ask about ("how to
        // make a layout" is the quote's slab layout): the manual's own words.
        Some(Picked::Ask(_)) if matches_topic(question) => Ok(Some(own_words())),
        Some(Picked::Ask(clarification)) => Err(clarification),
        Some(Picked::Sections(mut ids)) if !ids.is_empty() => {
            // The best of the manual's own words the model did not pick.
            if let Some(backup) = fallback.iter().find(|id| {
                !MANUAL.core.contains(*id) && !MANUAL.fallback.contains(*id) && !ids.contains(*id)
            }) {
                ids.push(backup.clone());
            }
            Ok(Some(ids))
        }
        // The model finds it is not about using the app.
        Some(Picked::Sections(_)) => Ok(None),
        None => Ok(Some(own_words())),
    }
}

/// What the asker may open, for the manual's answers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Access {
    pub is_employee: bool,
    pub is_admin: bool,
    pub is_platform_superuser: bool,
    pub position_labels: Vec<String>,
}

fn position_label(id: i32) -> String {
    match id {
        1 => "Sales Rep".into(),
        2 => "Sales Manager".into(),
        3 => "Shop Manager".into(),
        4 => "Shop Worker".into(),
        5 => "Manager".into(),
        6 => "Installer".into(),
        7 => "Marketing".into(),
        8 => "Walk-in".into(),
        9 => "Super Admin".into(),
        10 => "Office Manager".into(),
        11 => "Template Guy".into(),
        other => format!("Position {other}"),
    }
}

/// `loadCrmManualUserAccess`.
pub async fn access(pool: &MySqlPool, user: &SessionUser) -> sqlx::Result<Access> {
    let positions: Vec<i32> = sqlx::query_scalar(
        "SELECT DISTINCT position_id FROM users_positions WHERE user_id = ? AND company_id = ?",
    )
    .bind(user.id)
    .bind(user.company_id)
    .fetch_all(pool)
    .await?;
    Ok(Access {
        is_employee: user.is_employee,
        is_admin: user.is_admin,
        is_platform_superuser: user.is_superuser,
        position_labels: positions.into_iter().map(position_label).collect(),
    })
}

/// `formatCrmManualUserAccessBlock`.
pub fn access_block(access: &Access) -> String {
    let yes = |b: bool| if b { "yes" } else { "no" };
    let positions = if access.position_labels.is_empty() {
        "none".to_string()
    } else {
        access.position_labels.join(", ")
    };
    format!(
        "CURRENT USER ACCESS (strict — only describe UI this user can open):\n- Employee portal: {}\n- Admin portal: {}\n- Platform superuser (Superuser Dashboard / Create Company): {}\n- Positions at current company: {positions}",
        yes(access.is_employee),
        yes(access.is_admin),
        yes(access.is_platform_superuser)
    )
}

pub const ACCESS_RULES: &str = r#"Access rules for the current user:
1. Only describe pages, buttons, and workflows this user can open with CURRENT USER ACCESS above.
2. If they ask about a feature they cannot access, say clearly that they do not have access and which role is required. Do not give step-by-step instructions for locked features.
3. Superuser Dashboard and Create Company require platform superuser = yes. Super Admin position alone is not enough.
4. Admin portal pages require Admin portal = yes. Company settings (Users / Company tax/logo) also need the Manager position.
5. Shop Worker only has Shop Transactions, Samples, Maintenance, My Account — not employee CRM or admin.
6. Marketing position is the external marketing portal only — not employee CRM or admin.
7. If a non-superuser asks how to "add a new company", do NOT describe Superuser Dashboard. If they have Employee portal access, you may describe CRM > Customers → Add Customer → Company toggle (customer marked as a company). Otherwise say they lack access.
8. Never invent access the user does not have."#;

/// A question asking for help: "how do I", "where is", "can I"...
pub fn is_help_seeking(normalized: &str) -> bool {
    [
        re!(r"\bhow (?:do|can|should|would|does)\b"),
        re!(r"\bhow to\b"),
        re!(r"\bwhere (?:do|can|is|are)\b"),
        re!(r"\bwhere can i find\b"),
        re!(r"\bcan i\b"),
        re!(r"\bhelp(?: me)?\b"),
        re!(r"\bneed to\b"),
        re!(r"\bwant to\b"),
        re!(r"\btrying to\b"),
        re!(r"\bwalk me through\b"),
    ]
    .iter()
    .any(|p| p.is_match(normalized))
}

/// Asks how to use the app, or why it does what it does ("how do I…", "can
/// I…", "is there a way to…", "where is…", "why am I not getting…", "turn
/// off…"), whatever records it names: the manual answers it, not the
/// assistant.
pub fn asks_how_to(query: &str) -> bool {
    let normalized = normalize(&query.replace('\u{2019}', "'"));
    [
        re!(r"\bhow (?:do|can|could|should|would|does)\b"),
        re!(r"\bhow to\b"),
        re!(r"\bwhere (?:do|can|is|are|would|should)\b"),
        re!(r"\bcan (?:i|we|the|my|our)\b"),
        re!(r"\bwalk me through\b"),
        re!(r"\bis there (?:a way|any way|an option|a setting|a button|a place)\b"),
        re!(r"\bis it possible\b"),
        re!(r"\bwhy (?:am i|can't|cant|cannot|isn't|isnt|doesn't|doesnt|don't|dont|won't|wont|aren't|arent|do i not|is there no)\b"),
        re!(r"\b(?:can't|cant|cannot|unable to)\b"),
        re!(r"\b(?:not|doesn't|doesnt|isn't|isnt|won't|wont) (?:work|working|show|showing|load|loading|ring|ringing|send|sending|open|opening)\b"),
        re!(r"\bno longer\b"),
        re!(r"\b(?:turn|switch) (?:on|off)\b"),
        re!(r"\b(?:stop|disable|enable|mute|unmute)\b"),
    ]
    .iter()
    .any(|p| p.is_match(&normalized))
}

/// Asks about the company's own process, rules or prices ("what is our
/// special order policy", "how much do we charge"), not about the app.
pub fn asks_about_company_process(query: &str) -> bool {
    let normalized = normalize(&query.replace('\u{2019}', "'"));
    asks_a_price(&normalized)
        || re!(r"\b(?:our|my|company|company's)\b(?: \S+){0,4} (?:process|processes|policy|policies|procedure|procedures|rules|handbook)\b").is_match(&normalized)
        || re!(r"\b(?:our|company|company's) instructions?\b").is_match(&normalized)
}

/// Asks what something costs ("what is the fabrication price"), not how to
/// set a price in the app ("how do I change the price on one line"). A price
/// list is a thing of the app, not a price asked.
fn asks_a_price(normalized: &str) -> bool {
    let normalized = re!(r"\bprice lists?\b").replace_all(normalized, "list");
    re!(r"\b(?:prices?|pricing|costs?|fees?|rates?|charges?|fabrication|sqft)\b").is_match(&normalized)
        && !re!(r"\b(?:change|set|edit|update|override|add|remove|enter|type|put|adjust|hide|show|apply|give)\b(?: \S+){0,4} (?:prices?|pricing|costs?|fees?|rates?|charges?)\b").is_match(&normalized)
}

/// What a how-to does in the CRM ("add", "make"...), before what it does it to.
const ACTION_VERBS: &str = "add|create|edit|delete|upload|change|open|find|use|set up|setup|sell|remove|update|enable|disable|mark|reply|compose|attach|restore|undelete|make|do|put|enter|send|connect|disconnect|turn on|turn off|switch on|switch off|print|share|import|export|assign|rename|move|hide|show|link|unlink|copy|duplicate|download|reset|set|stop|cancel|draw|rotate";

/// Up to two words between the verb and the thing ("the", "all my").
const DETERMINERS: &str =
    r"(?:(?:a|an|the|new|my|our|your|this|that|all|each|every|another) ){0,2}";

/// What a how-to does it to: the app's things ("a quote", "the layout").
const APP_THINGS: &str = "stone|stones|slab|slabs|customer|customers|deal|deals|user|users|sink|sinks|faucet|faucets|supplier|suppliers|invoice|invoices|checklist|checklists|instruction|instructions|logo|company|email|emails|questionnaire|questionnaires|note|notes|event|events|message|messages|inbox|sms|thread|conversation|draft|drafts|quote|quotes|drawing|drawings|layout|layouts|notification|notifications|alert|alerts|widget|widgets|dashboard|sidebar|template|templates|price list|price lists|price|prices|color|colors|area|areas|seam|seams|counter|counters|countertop|countertops|edge|edges|cutout|cutouts|backsplash|splash|photo|photos|picture|pictures|image|images|document|documents|file|files|calendar|calendars|appointment|appointments|reminder|reminders|task|tasks|activity|activities|password|account|signature|signatures|contract|contracts|phone|number|numbers|line|lines|lead|leads|tour|tours|report|reports|tax|discount|discounts|form|forms|link|links|catalog|showroom|transaction|transactions|survey|surveys|commission|commissions|integration|integrations";

/// `looksLikeCrmManualHowToQuery`: a question about using the CRM itself.
/// The browser has the same rules; both repos test them with the cases in
/// `crm_manual_how_to_cases.json`. Only here are typos corrected first.
pub fn looks_like_how_to(query: &str) -> bool {
    let normalized = corrected(&normalize(query));
    if normalized.is_empty() || asks_a_price(&normalized) {
        return false;
    }
    let help = is_help_seeking(&normalized);
    let product = [
        re!(r"\bcrm\b"),
        re!(r"\bgranite manager\b"),
        re!(r"\bgranitemanager\b"),
        re!(r"\bin the app\b"),
        re!(r"\bin this app\b"),
        re!(r"\bthis software\b"),
        re!(r"\bthe software\b"),
        re!(r"\bin the system\b"),
        re!(r"\bemployee portal\b"),
        re!(r"\badmin portal\b"),
        re!(r"\buser manual\b"),
        re!(r"\bin (?:a|the) quote\b"),
        re!(r"\bin the drawing\b"),
    ]
    .iter()
    .any(|p| p.is_match(&normalized));
    if product && help {
        return true;
    }
    if re!(&format!(
        r"\b(?:{ACTION_VERBS})\s+{DETERMINERS}(?:{APP_THINGS})\b"
    ))
    .is_match(&normalized)
    {
        return true;
    }
    // In a quote, a special order is a color typed by name (not in the list).
    if re!(r"\bquotes?\b").is_match(&normalized)
        && re!(&format!(
            r"\b(?:{ACTION_VERBS})\s+{DETERMINERS}special orders?\b"
        ))
        .is_match(&normalized)
    {
        return true;
    }
    if help
        && re!(r"\b(?:inbox|mailbox|unread|mark as unread|mark as read|mark unread|mark read|new email|email thread|email draft|sms|text message|text messages|deal board|activity panel|customer information|find customer|drafts folder|drafts|attach files|ai reply|improve text)\b").is_match(&normalized)
    {
        return true;
    }
    // The services the app connects to, by name.
    if help
        && re!(r"\b(?:telnyx|telegram|quickbooks|qbo|cloudtalk|ringcentral|signwell|zapier|10dlc|granite phone)\b").is_match(&normalized)
    {
        return true;
    }
    re!(r"\bwhere (?:is|are|do i find|can i find)\b.+\b(?:page|button|sidebar|menu|tab|setting|screen|form|folder|inbox)\b").is_match(&normalized)
}

/// Where an answer comes from when the user was asked which they meant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Source {
    /// How to do it in Granite Manager: the CRM manual.
    Manual,
    /// The company's own process: its instructions.
    Company,
}

/// An answer to [`CLARIFY_QUESTION`] the user can pick (the `clarify` event).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Choice {
    pub label: &'static str,
    pub source: Source,
}

/// Asked instead of guessing when a question could be about either.
pub const CLARIFY_QUESTION: &str =
    "Do you mean how to do this in Granite Manager, or your company's own process?";

/// The choices sent with [`CLARIFY_QUESTION`]; a click sends the label.
pub const CLARIFY_CHOICES: [Choice; 2] = [
    Choice {
        label: "In Granite Manager",
        source: Source::Manual,
    },
    Choice {
        label: "Our company's process",
        source: Source::Company,
    },
];

/// Whether to ask before answering: a question asking for help that the
/// how-to rules leave to the company instructions, while a specific manual
/// topic covers it too and so does one of the company's instructions.
pub fn should_clarify(query: &str, how_to: bool, company_instruction: bool) -> bool {
    let normalized = normalize(query);
    !how_to
        && company_instruction
        && !is_price_query(&normalized)
        && is_help_seeking(&normalized)
        && matches_topic(query)
}

/// The source a reply to [`CLARIFY_QUESTION`] picks: a choice's label, or a
/// typed word like "app" or "company". Anything else is a new question.
pub fn clarify_choice(reply: &str) -> Option<Source> {
    let normalized = normalize(&reply.replace('\u{2019}', "'"));
    let answer = trim(normalized.trim_end_matches(['.', '!', '?']));
    if let Some(choice) = CLARIFY_CHOICES
        .iter()
        .find(|c| c.label.to_lowercase() == answer)
    {
        return Some(choice.source);
    }
    match answer {
        "app" | "the app" | "in the app" | "granite manager" | "in granite manager" | "crm"
        | "the crm" | "in the crm" => Some(Source::Manual),
        "company"
        | "the company"
        | "our company"
        | "my company"
        | "company process"
        | "our process"
        | "my process"
        | "company's process"
        | "our company's process"
        | "my company's process"
        | "the company's process"
        | "our own process"
        | "company's own process"
        | "our company's own process" => Some(Source::Company),
        _ => None,
    }
}

pub fn asks_cloudtalk_phone(normalized: &str) -> bool {
    (normalized.contains("cloudtalk") || normalized.contains("qualtalk"))
        && ["phone", "number", "line", "sms sender", "outbound"]
            .iter()
            .any(|w| normalized.contains(w))
}

fn can_edit_admin_users(access: Option<&Access>) -> bool {
    access.is_some_and(|a| {
        a.is_platform_superuser || (a.is_admin && a.position_labels.iter().any(|l| l == "Manager"))
    })
}

pub fn cloudtalk_phone_answer(question: &str, access: Option<&Access>) -> Option<String> {
    let normalized = normalize(question);
    if !asks_cloudtalk_phone(&normalized) {
        return None;
    }
    let asks_where = ["find", "get", "where", "obtain", "look up"]
        .iter()
        .any(|w| normalized.contains(w));
    let find_steps = [
        "The phone number itself lives in CloudTalk, not in Granite Manager.",
        "",
        "1. Log in to the [CloudTalk Dashboard](https://dashboard.cloudtalk.io) with **CloudTalk Admin** access.",
        "2. Open **Numbers** and search for the line assigned to the user.",
        "   You can also open **Users → Edit user** and check which numbers are assigned.",
        "3. Copy the full number including country code.",
    ]
    .join("\n");
    if asks_where
        && !["crm", "granite", "enter", "set"]
            .iter()
            .any(|w| normalized.contains(w))
    {
        return Some(find_steps);
    }
    let mut parts: Vec<String> = vec![
        "**CloudTalk phone number** and **CloudTalk agent ID** are separate fields in Granite Manager.".into(),
        String::new(),
        "### Find the CloudTalk phone number".into(),
        String::new(),
        find_steps,
    ];
    if can_edit_admin_users(access) {
        parts.extend([
            String::new(),
            "### Enter it for another user".into(),
            String::new(),
            [
                "1. Open **Admin → Company settings → Users**.",
                "2. Click **Edit** on the user.",
                "3. Paste the number into **CloudTalk phone number** (at least 10 digits).",
                "4. Submit.",
            ]
            .join("\n"),
        ]);
    }
    if access.is_some_and(|a| a.is_employee) {
        parts.extend([
            String::new(),
            "### Enter it on your own account".into(),
            String::new(),
            [
                "1. Open **My Account** in the sidebar.",
                "2. Paste the number into **CloudTalk phone number**.",
                "3. Save.",
            ]
            .join("\n"),
        ]);
    }
    if access.is_some() && !can_edit_admin_users(access) && access.is_some_and(|a| a.is_employee) {
        parts.extend([
            String::new(),
            "To set another user's CloudTalk phone number, a **Manager** with **Admin** access must use **Admin → Company settings → Users → Edit**.".into(),
        ]);
    }
    parts.extend([
        String::new(),
        "**CloudTalk phone number** is the outbound SMS line Granite Manager uses for that user. **CloudTalk agent ID** links call history and **Made a call** activity.".into(),
    ]);
    Some(parts.join("\n"))
}

pub fn cloudtalk_credentials_answer(question: &str, access: Option<&Access>) -> Option<String> {
    let n = normalize(question);
    let asks = n.contains("cloudtalk")
        && (n.contains("access key")
            || n.contains("access secret")
            || n.contains("credential")
            || n.contains("api key")
            || n.contains("api keys")
            || (n.contains("key") && n.contains("secret"))
            || n.contains("where to get")
            || n.contains("how do i get"));
    if !asks {
        return None;
    }
    if access.is_some_and(|a| !a.is_platform_superuser) {
        return Some(
            [
                "CloudTalk API credentials are set when a **platform superuser** creates or edits a company in Granite Manager.",
                "",
                "Your account does not have platform superuser access. Ask a platform superuser to add or update the company CloudTalk keys.",
            ]
            .join("\n"),
        );
    }
    Some(
        [
            "**CloudTalk Access Key** and **CloudTalk Access Secret** are pasted into Granite Manager, but you obtain them from CloudTalk — not from Granite Manager.",
            "",
            "### Get the keys in CloudTalk",
            "",
            "1. Log in to the [CloudTalk Dashboard](https://dashboard.cloudtalk.io) with **CloudTalk Admin** access.",
            "2. Go to **Account → Settings → API Keys**.",
            "3. Click **+ Add API Key** (or use an existing key).",
            "4. Copy the **Access Key ID** and **Access Key Secret** (use **Show** to reveal the secret).",
            "",
            "### Enter them in Granite Manager",
            "",
            "1. Open **Superuser Dashboard → Companies**.",
            "2. Click **Create Company** (or edit an existing company).",
            "3. Paste the values into **CloudTalk Access Key** and **CloudTalk Access Secret**.",
            "4. Submit the form.",
        ]
        .join("\n"),
    )
}

/// Asks for the company's inventory link ("our inventory link", "link to the stones").
fn asks_inventory_link(normalized: &str) -> bool {
    re!(r"\b(?:inventory|stones?)(?: page)? (?:link|url)\b").is_match(normalized)
        || re!(
            r"\b(?:link|url) (?:to|for) (?:our |the |my |your )?(?:live )?(?:inventory|stones)\b"
        )
        .is_match(normalized)
        || re!(r"\b(?:customer (?:inventory|stones?)|showroom) page\b").is_match(normalized)
}

/// Asks for the company's customer quote link ("customer quote link", "link to the online quote").
fn asks_quote_link(normalized: &str) -> bool {
    re!(r"\bquote(?: page)? (?:link|url)\b").is_match(normalized)
        || re!(r"\b(?:link|url) (?:to|for) (?:our |the |my |your )?(?:customer |online )?quote\b")
            .is_match(normalized)
        // Not a bare "quote page": that is also the quote page inside the app.
        || re!(r"\b(?:customer|online) quote page\b").is_match(normalized)
}

/// Asks for the inventory or the customer quote link.
pub fn asks_customer_link(question: &str) -> bool {
    let normalized = normalize(question);
    asks_inventory_link(&normalized) || asks_quote_link(&normalized)
}

/// The company's own inventory and customer quote links, for a question
/// asking for either: always the asker's company, never a guess by the model.
pub fn customer_link_answer(
    question: &str,
    company_id: i32,
    domain: Option<&str>,
) -> Option<String> {
    let normalized = normalize(question);
    let inventory = asks_inventory_link(&normalized);
    let quote = asks_quote_link(&normalized);
    let mut lines = Vec::new();
    if inventory {
        lines.push(format!(
            "**Inventory link:** {}",
            customer_inventory_page_url(company_id, domain)
        ));
    }
    if quote {
        lines.push(format!(
            "**Customer quote link:** {}",
            customer_quote_page_url(company_id, domain)
        ));
    }
    if lines.is_empty() {
        return None;
    }
    lines.push(String::new());
    lines.push("Copy it and send it to your customer.".into());
    Some(lines.join("\n"))
}

/// `getCrmManualQuickAnswer`: a canned answer for questions the manual gets wrong.
pub fn quick_answer(question: &str, access: Option<&Access>) -> Option<String> {
    cloudtalk_phone_answer(question, access)
        .or_else(|| cloudtalk_credentials_answer(question, access))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The browser's detector runs the same cases (synced from the CRM repo).
    #[derive(Deserialize)]
    struct HowToCase {
        query: String,
        #[serde(rename = "howTo")]
        how_to: bool,
    }

    #[test]
    fn how_to_cases_shared_with_the_browser() {
        let cases: Vec<HowToCase> =
            serde_json::from_str(include_str!("crm_manual_how_to_cases.json"))
                .expect("crm_manual_how_to_cases.json is valid");
        assert!(cases.len() > 20);
        for case in cases {
            assert_eq!(
                looks_like_how_to(&case.query),
                case.how_to,
                "{}",
                case.query
            );
        }
    }

    #[test]
    fn a_special_order_in_a_quote_is_the_drawing_s_color_step() {
        let question = "how to make a special order in quote";
        assert!(looks_like_how_to(question));
        let ids = section_ids_for(question);
        assert!(
            ids.iter().any(|id| id == "quote-drawing-color-slab-layout"),
            "{ids:?}"
        );
        let manual = assemble(&ids, &HashMap::new());
        assert!(manual.contains("#### Special order (a color not in the list)"));
    }

    #[test]
    fn asks_when_the_app_and_the_company_both_cover_a_question() {
        let both = "how do I make a special order?";
        assert!(!looks_like_how_to(both));
        assert!(matches_topic(both));
        assert!(should_clarify(both, false, true));
        // Without a company instruction about it, it is answered as before.
        assert!(!should_clarify(both, false, false));
        // A plain company question goes to the instructions.
        let company = "what is our special order deposit policy";
        assert!(!looks_like_how_to(company));
        assert!(!should_clarify(company, false, true));
        // A how-to by the rules goes to the manual.
        let app = "how to make a special order in quote";
        assert!(!should_clarify(app, looks_like_how_to(app), true));
    }

    #[test]
    fn a_reply_picks_where_the_answer_comes_from() {
        for (reply, source) in [
            ("In Granite Manager", Some(Source::Manual)),
            ("in granite manager", Some(Source::Manual)),
            ("app", Some(Source::Manual)),
            ("The app.", Some(Source::Manual)),
            ("Our company's process", Some(Source::Company)),
            ("our company\u{2019}s process", Some(Source::Company)),
            ("company", Some(Source::Company)),
            ("how do I add a customer?", None),
            ("yes", None),
        ] {
            assert_eq!(clarify_choice(reply), source, "{reply}");
        }
        for choice in CLARIFY_CHOICES {
            assert_eq!(clarify_choice(choice.label), Some(choice.source));
        }
        assert_eq!(
            serde_json::to_string(&CLARIFY_CHOICES).unwrap(),
            r#"[{"label":"In Granite Manager","source":"manual"},{"label":"Our company's process","source":"company"}]"#
        );
    }

    #[test]
    fn renaming_a_person_routes_to_the_users_section() {
        for question in [
            "how to change installer name",
            "how to change user's name",
            "how to rename a sales rep",
            "change technician name",
            "how to edit user name",
            "how to change the name of an installer",
            "installer name is misspelled",
            "how do I fix a wrong name on a survey card",
            "change employee name",
            "update staff name",
        ] {
            assert!(
                section_ids_for(question)
                    .iter()
                    .any(|id| id == "admin-company-settings-users"),
                "{question}"
            );
        }
        let ids = section_ids_for("installer name is wrong on the survey");
        assert!(ids.iter().any(|id| id == "admin-surveys-customer-feedback"));
        let manual = assemble(
            &section_ids_for("how to change installer name"),
            &HashMap::new(),
        );
        assert!(manual.contains("Change a person's name") && manual.contains("→ **Edit**"));
    }

    #[test]
    fn articles_do_not_break_phrases() {
        for (question, id) in [
            ("how do I add a user", "admin-company-settings-users"),
            ("how do I upload a document", "resources-documents-images"),
            (
                "how do I sell a slab",
                "employee-stones-inventory-selling-slabs",
            ),
        ] {
            assert!(
                section_ids_for(question).iter().any(|s| s == id),
                "{question}"
            );
        }
    }

    #[test]
    fn every_section_is_reachable() {
        let reachable: std::collections::HashSet<&str> = MANUAL
            .core
            .iter()
            .chain(&MANUAL.fallback)
            .chain(
                MANUAL
                    .topics
                    .iter()
                    .flat_map(|t| std::iter::once(&t.id).chain(&t.related)),
            )
            .map(String::as_str)
            .collect();
        for id in section_ids() {
            assert!(
                reachable.contains(id) || id == "document-scope",
                "no topic loads {id}"
            );
        }
    }

    #[test]
    fn link_questions_get_the_company_s_own_links() {
        let domain = Some("granitedepotindy.granite-manager.com");
        let inventory = "https://granitedepotindy.granite-manager.com/customer/7/stones";
        let quote = "https://granitedepotindy.granite-manager.com/customer/7/quote";
        for asked in [
            "What is our inventory link?",
            "send me the link to our inventory",
            "stones page url",
            "what's the customer inventory page",
            "customer stones page?",
        ] {
            let answer = customer_link_answer(asked, 7, domain).expect(asked);
            assert!(
                answer.contains(inventory) && !answer.contains(quote),
                "{asked}"
            );
        }
        for asked in [
            "customer quote link?",
            "Link to the online quote please",
            "what's the quote url",
            "what's the customer quote page",
            "send me the online quote page",
        ] {
            let answer = customer_link_answer(asked, 7, domain).expect(asked);
            assert!(
                answer.contains(quote) && !answer.contains(inventory),
                "{asked}"
            );
        }
        let both = customer_link_answer("inventory link and customer quote link", 7, domain)
            .expect("both");
        assert!(both.contains(inventory) && both.contains(quote));
        for other in [
            "how do I create a quote",
            "how do I link a quote to a deal",
            "how many stones are in inventory",
            "how do I open the quote page",
        ] {
            assert_eq!(customer_link_answer(other, 7, domain), None, "{other}");
        }
    }

    #[test]
    fn new_features_route_to_their_sections() {
        for (question, id) in [
            ("how do I create a quote", "employee-quotes"),
            ("how do I rotate text in the drawing", "quote-drawing"),
            ("how do I add a new calendar", "schedule-admin-settings"),
            ("how do I send a contract for e-signature", "esignatures"),
            ("how do I connect telegram", "employee-dashboard"),
            ("how do I make a call from the computer", "phone-calls"),
            ("where are invoices", "invoices"),
            ("how do I create a follow-up flow", "admin-automations"),
        ] {
            assert!(
                section_ids_for(question).iter().any(|s| s == id),
                "{question}"
            );
        }
    }

    #[test]
    fn typos_are_read_as_the_manual_s_words() {
        for (typed, meant) in [
            (
                "how to add a specail order slab to quote",
                "how to add a special order slab to quote",
            ),
            (
                "how to send aquote to customer",
                "how to send a quote to customer",
            ),
            (
                "where do i find the invocies",
                "where do i find the invoices",
            ),
            ("custmer list where", "customer list where"),
            ("how to dupliacte a quote", "how to duplicate a quote"),
            ("how can i connect telnix", "how can i connect telnyx"),
        ] {
            assert_eq!(corrected(typed), meant);
        }
        // Words the manual has, and short ones, stay as typed.
        assert_eq!(corrected("how do i add a deal"), "how do i add a deal");
    }

    /// The questions users asked that the chat got wrong.
    #[test]
    fn the_reported_questions_reach_their_sections() {
        for (question, id) in [
            (
                "How to add a specail order slab to quote",
                "quote-drawing-color-slab-layout",
            ),
            ("How can I connect Telnyx", "admin-phone-telnyx"),
            ("How to make a layout", "quote-drawing-color-slab-layout"),
            (
                "How to send aquote to customer",
                "employee-quotes-sending-and-selling",
            ),
            ("How to turn off notifications", "notifications"),
        ] {
            assert!(looks_like_how_to(question), "{question}");
            let ids = section_ids_for(question);
            assert!(ids.iter().any(|s| s == id), "{question}: {ids:?}");
        }
    }

    #[test]
    fn a_price_asked_is_not_a_how_to_but_setting_one_is() {
        assert!(!looks_like_how_to(
            "what is the fabrication price for quartz"
        ));
        assert!(!looks_like_how_to(
            "do we charge for delivery on special orders"
        ));
        assert!(looks_like_how_to(
            "how do I change the price on one line of the quote"
        ));
        // "separate" has "rate" in it; it is no price.
        assert!(looks_like_how_to(
            "how do I add an area to separate the island"
        ));
    }

    #[test]
    fn notifications_are_asked_back_with_the_kinds_there_are() {
        let Some(Features::Ask(asked)) = features("How to turn off notifications") else {
            panic!("asked back")
        };
        let labels: Vec<&str> = asked.choices.iter().map(|c| c.label.as_str()).collect();
        assert_eq!(
            labels,
            [
                "Bell in the header",
                "Desktop pop-ups",
                "Telegram alerts",
                "Granite Phone app",
                "Phone ringing",
                "Reminders to customers"
            ]
        );
        // One short question that names them.
        for kind in [
            "the bell",
            "desktop pop-ups",
            "telegram alerts",
            "granite phone app",
            "phone ringing",
            "reminders sent to customers",
        ] {
            assert!(asked.question.to_lowercase().contains(kind), "{kind}");
        }
        // The browser reads the same choices as the app or company ones.
        assert_eq!(
            serde_json::to_value(&asked.choices[0]).unwrap(),
            serde_json::json!({ "label": "Bell in the header", "source": "manual" })
        );
        assert!(matches!(
            features("why am I not getting notifications"),
            Some(Features::Ask(_))
        ));
        // A question that names the kind is answered without asking.
        for (question, id) in [
            ("how to turn off desktop notifications", "notifications"),
            ("stop telegram alerts for emails", "my-account-telegram"),
            ("how do i clear all notifications", "notifications"),
            (
                "how do I turn off notifications to customers",
                "schedule-notifications",
            ),
        ] {
            let Some(Features::Named(ids)) = features(question) else {
                panic!("{question}")
            };
            assert!(ids.iter().any(|s| s == id), "{question}: {ids:?}");
        }
        assert_eq!(features("how do I add a stone"), None);
    }

    #[test]
    fn a_reply_picks_one_of_the_features() {
        let choices = [
            feature("Bell in the header", vec!["notifications".into()]),
            feature("Telegram alerts", vec!["my-account-telegram".into()]),
        ];
        for (reply, label) in [
            ("Telegram alerts", Some("Telegram alerts")),
            ("telegram alerts.", Some("Telegram alerts")),
            ("telegram", Some("Telegram alerts")),
            ("the bell", Some("Bell in the header")),
            ("1", Some("Bell in the header")),
            ("3", None),
            ("how do I add a customer?", None),
            ("In Granite Manager", None),
        ] {
            assert_eq!(
                feature_choice(reply, &choices).map(|c| c.label.as_str()),
                label,
                "{reply}"
            );
        }
    }

    #[test]
    fn the_model_s_pick_is_checked() {
        assert_eq!(
            parse_pick(
                r#"{"sections": ["quote-drawing-color-slab-layout", "nope", "user-positions-access", "quote-drawing-color-slab-layout", "quote-drawing", "employee-quotes", "invoices"]}"#
            ),
            Some(Picked::Sections(vec![
                "quote-drawing-color-slab-layout".into(),
                "quote-drawing".into(),
                "employee-quotes".into(),
            ]))
        );
        assert_eq!(
            parse_pick(r#"{"sections": []}"#),
            Some(Picked::Sections(vec![]))
        );
        let asked = parse_pick(
            r#"{"clarify": {"question": "The email template or the SMS template?", "choices": [
                {"label": "Email templates", "sections": ["admin-automations"]},
                {"label": "SMS [templates]", "sections": ["employee-sms", "nope"]}]}}"#,
        );
        let Some(Picked::Ask(asked)) = asked else {
            panic!("{asked:?}")
        };
        assert_eq!(asked.choices[1].label, "SMS templates");
        assert_eq!(asked.choices[1].sections, ["employee-sms"]);
        // One choice is no question: the sections, else nothing.
        assert_eq!(
            parse_pick(
                r#"{"clarify": {"question": "Which?", "choices": [{"label": "A", "sections": ["invoices"]}]}, "sections": ["invoices"]}"#
            ),
            Some(Picked::Sections(vec!["invoices".into()]))
        );
        assert_eq!(parse_pick("Open the quote."), None);
    }

    #[test]
    fn the_table_of_contents_names_every_section_but_the_core() {
        for id in section_ids() {
            let line = format!("[{id}] ");
            let listed = CONTENTS.lines().any(|l| l.starts_with(&line));
            assert_eq!(
                listed,
                !MANUAL.core.iter().any(|c| c == id) && id != "document-scope",
                "{id}"
            );
        }
        assert!(!CONTENTS.contains("(/admin"), "no routes");
        assert!(CONTENTS.contains("Special order (a color not in the list)"));
    }

    #[test]
    fn how_to_questions_that_name_records_are_not_for_the_assistant() {
        for query in [
            "can i delete a deal?",
            "why am I not getting notifications",
            "customer says the quote link is no longer active",
            "stop telegram alerts for emails",
        ] {
            assert!(asks_how_to(query), "{query}");
        }
        for query in [
            "find customer Alice Lead",
            "how many deals did we win this month",
        ] {
            assert!(!asks_how_to(query), "{query}");
        }
    }
    /// "How to make a special order?" is a special-order slab in a quote or
    /// the Special Order calculator; the question names which, or is asked.
    #[test]
    fn a_special_order_is_asked_back_unless_the_question_says_which() {
        for question in [
            "How to make a special order?",
            "how can I make a special order?",
        ] {
            let Some(Features::Ask(asked)) = features(question) else {
                panic!("{question}")
            };
            let labels: Vec<&str> = asked.choices.iter().map(|c| c.label.as_str()).collect();
            assert_eq!(
                labels,
                ["Special-order slab in a quote", "Special Order calculator"]
            );
            assert_eq!(
                asked.question,
                "Do you mean a special-order slab in a quote, or the Special Order calculator?"
            );
            assert_eq!(
                asked.with_company.as_deref(),
                Some(
                    "Do you mean a special-order slab in a quote, the Special Order calculator, or your company's own process?"
                )
            );
        }
        for (question, id) in [
            (
                "How to add a specail order slab to quote",
                "quote-drawing-color-slab-layout",
            ),
            (
                "how to make a special order in quote",
                "quote-drawing-color-slab-layout",
            ),
            (
                "whats the special order calculator",
                "samples-special-order",
            ),
        ] {
            assert_eq!(
                features(question),
                Some(Features::Named(vec![id.to_string()])),
                "{question}"
            );
        }
        // The company's process is a choice too, answered from its instructions.
        let company = company_choice();
        assert_eq!(
            (company.label.as_str(), company.source),
            ("Our company's process", Source::Company)
        );
        assert!(company.sections.is_empty());
    }

    #[test]
    fn the_company_s_own_process_is_told_from_the_app() {
        for query in [
            "what is our special order deposit policy",
            "is that our company's process?",
            "what do our instructions say about it",
            "how much do we charge for that",
        ] {
            assert!(asks_about_company_process(query), "{query}");
        }
        for query in [
            "what does it mean other color?? there is no such option",
            "why are you sending me this page, there is no mention about other color??",
            "no, in quotes",
            "they don't have it in the price lists",
        ] {
            assert!(!asks_about_company_process(query), "{query}");
        }
    }
}
