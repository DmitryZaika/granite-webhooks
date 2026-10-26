//! What the customer's messages say about where they stand (waiting,
//! comparing quotes, not interested...). Port of `smsFlowAiCustomerIntent.ts`.

use super::contact_resume::text_looks_like_customer_owns_next_contact;
use super::dates::js_date_parse;
use super::js::{js_sort_by, trim};
use crate::{re, str_enum};
use regex::Regex;
use std::collections::BTreeSet;

str_enum! { pub enum MeaningfulCustomerIntentType {
    ActiveInterest = "ACTIVE_INTEREST", Waiting = "WAITING", Delayed = "DELAYED", ComparingQuotes = "COMPARING_QUOTES",
    EvaluatingOptions = "EVALUATING_OPTIONS", BudgetConcern = "BUDGET_CONCERN", FutureProject = "FUTURE_PROJECT",
    Planning = "PLANNING", MovingForward = "MOVING_FORWARD", NotInterested = "NOT_INTERESTED",
    ProjectCompleted = "PROJECT_COMPLETED", NotNow = "NOT_NOW", CustomerWillRecontact = "CUSTOMER_WILL_RECONTACT",
    AlmostReady = "ALMOST_READY", Unknown = "UNKNOWN",
} }

str_enum! { pub enum MeaningfulCustomerIntentSource {
    Sms = "SMS", Email = "EMAIL", SalespersonNote = "salesperson_note", ProjectInfo = "project_info", Unknown = "unknown",
} }

str_enum! { pub enum IntentConfidence { High = "high", Medium = "medium", Low = "low" } }

/// Where a text came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IntentChannel {
    Sms,
    Email,
    SalespersonNote,
    ProjectInfo,
}

/// One intent read from one message.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct MeaningfulCustomerIntent {
    #[serde(rename = "type")]
    pub kind: MeaningfulCustomerIntentType,
    pub source: MeaningfulCustomerIntentSource,
    pub timestamp: Option<String>,
    pub evidence: String,
    pub confidence: IntentConfidence,
}

/// An inbound message as the intent readers take it.
#[derive(Debug, Clone, Default)]
pub struct IntentText {
    pub text: String,
    pub channel: Option<IntentChannel>,
    pub at: Option<String>,
}

const INTENT_PRIORITY: [MeaningfulCustomerIntentType; 14] = {
    use MeaningfulCustomerIntentType as T;
    [
        T::NotInterested,
        T::ProjectCompleted,
        T::CustomerWillRecontact,
        T::NotNow,
        T::MovingForward,
        T::Delayed,
        T::Waiting,
        T::AlmostReady,
        T::EvaluatingOptions,
        T::ComparingQuotes,
        T::BudgetConcern,
        T::FutureProject,
        T::Planning,
        T::ActiveInterest,
    ]
};

/// `INTENT_PRIORITY.indexOf(type)` (-1 when absent).
fn priority(kind: MeaningfulCustomerIntentType) -> f64 {
    INTENT_PRIORITY
        .iter()
        .position(|item| *item == kind)
        .map_or(-1.0, |index| {
            f64::from(u32::try_from(index).unwrap_or(u32::MAX))
        })
}

fn sort_by_priority(intents: &mut [MeaningfulCustomerIntent]) {
    js_sort_by(intents, |left, right| {
        priority(left.kind) - priority(right.kind)
    });
}

/// The customer pauses but is coming back soon: a short delay, not silence
/// and not a decline.
pub fn short_term_return_re() -> &'static Regex {
    re!(
        r"(be|get|come|circle) back (to (it|this|you)|on (it|this))?.{0,16}\bin a (few|couple of|couple) (days?|weeks?)|back to (it|this) (in|within) a (few|couple)|(resume|revisit|pick (it|this) back up).{0,20}in a (few|couple of|couple) (days?|weeks?)|will be back to (it|this)",
        "i"
    )
}

const fn intent_source(channel: Option<IntentChannel>) -> MeaningfulCustomerIntentSource {
    match channel {
        Some(IntentChannel::Email) => MeaningfulCustomerIntentSource::Email,
        Some(IntentChannel::Sms) => MeaningfulCustomerIntentSource::Sms,
        Some(IntentChannel::SalespersonNote) => MeaningfulCustomerIntentSource::SalespersonNote,
        Some(IntentChannel::ProjectInfo) => MeaningfulCustomerIntentSource::ProjectInfo,
        None => MeaningfulCustomerIntentSource::Unknown,
    }
}

/// Every intent in one text, most decisive first.
#[allow(clippy::too_many_lines)]
pub fn classify_all_meaningful_customer_intents(
    text: &str,
    channel: Option<IntentChannel>,
    at: Option<&str>,
) -> Vec<MeaningfulCustomerIntent> {
    use IntentConfidence::{High, Low, Medium};
    use MeaningfulCustomerIntentType as T;
    let text = trim(text);
    let source = intent_source(channel);
    let timestamp = at.map(str::to_string);
    let lower = text.to_lowercase();
    let l = lower.as_str();
    let mut found: Vec<MeaningfulCustomerIntent> = Vec::new();
    let mut add = |kind: T, confidence: IntentConfidence| {
        if found.iter().any(|item| item.kind == kind) {
            return;
        }
        found.push(MeaningfulCustomerIntent {
            kind,
            source,
            timestamp: timestamp.clone(),
            evidence: text.to_string(),
            confidence,
        });
    };
    if re!(r"moving forward with (our |my |the )?designer").is_match(l)
        || re!(r"going with (another|a different) (company|contractor|fabricator|designer)")
            .is_match(l)
        || re!(r"already (picked|chose|selected).{0,40}(on our own|with our designer|ourselves)")
            .is_match(l)
        || re!(r"not interested|please stop|do not contact|unsubscribe|no longer interested")
            .is_match(l)
        || (re!(r"thank you for your time").is_match(l)
            && re!(
                r"highest (of all my bids|bid)|someone cheaper|another company|too (high|expensive)"
            )
            .is_match(l))
        || (re!(r"\b(yes|yeah|yep)[,.]?\s*(sorry[, ]*)?(we |i )?(have|did)\b").is_match(l)
            && re!(
                r"given up|abandon|another direction|moved on|not moving forward",
                "i"
            )
            .is_match(l))
        || re!(r"^(yes|yeah|yep)[,.]?\s*sorry[, ]*we have[.!]?\s*$", "i").is_match(text)
    {
        add(T::NotInterested, High);
    }
    if re!(r"we (already )?finished|project is done|already installed|already completed")
        .is_match(l)
    {
        add(T::ProjectCompleted, High);
    }
    if re!(r"not doing the project now|we are not doing the project|project is (on hold|paused)|put (the )?project on hold").is_match(l) {
        add(T::NotNow, High);
    }
    if re!(r"i('ll| will) (contact|reach out|write|text|call|co\s*tact|get back to).{0,60}(once|after|when).{0,80}(estimate|ready|hear|back|done|them|from them)|i('ll| will) (contact|reach out|write|text) you when|will contact you when\s*ever|contact you whenever|whenever i plan|i will contact you when|i will contact you once|will contact you once|contact you once|напиш(ет|у|ешь|ете|е)( сама)?[, ]*(как|когда|коли)|сказ(ала|ал) напиш|я свяжусь|сама свяжусь|я напишу|я зв.?яжус|напишу[, ]*когда")
        .is_match(l)
        || text_looks_like_customer_owns_next_contact(text)
        // A salesperson note recording the same promise in the third person.
        || re!(r"(she|he|they)('ll| will) (text|call|contact|reach out|write|let (me|us) know).{0,40}(when|whenever|once)").is_match(l)
    {
        add(T::CustomerWillRecontact, High);
    }
    if re!(r"postponed|pushed back|on delay|delay the project|construction delay|contractor delay|put off the project|decided tu put off|decided to put off").is_match(l)
        || re!(r"something came up|can'?t push to go on|for now we can'?t|for now i can'?t").is_match(l)
    {
        add(
            T::Delayed,
            if re!(r"postponed|pushed back|something came up|can'?t push").is_match(l) { High } else { Medium },
        );
    }
    if re!(r"said they (decided to )?wait|they (decided to|want to) wait|decided to wait|customer (said|wants) to wait").is_match(l)
        || re!(r"\bbe patient\b|\bno rush\b|\btry(ing)? to get (the )?money\b|\bnot right now\b").is_match(l)
        || re!(r"contact me|check back|ready in|next (year|month|spring|fall)|after the (holidays|cabinets)|beginning of (the |next )?year|start of (the |next )?year|asked to call").is_match(l)
        || re!(r"i'?ll let you know when .{0,24}ready|let you know when (we are|i'?m|i am) ready").is_match(l)
        || re!(r"reach out closer|closer to (that|the) (date|time)|finances will be aligned|wait until late").is_match(l)
        || re!(r"hoping to start soon|should be able to get started|days would be best|about to have a baby").is_match(l)
        || re!(r"just hold for now|not ready for any renovations|at least for a y[ea]*r|waiting for (the )?(floors|cabinets)|no decisions will be made|минимум \d+|напиш(ет|у).{0,24}готов").is_match(l)
        || short_term_return_re().is_match(l)
        || re!(r"^not yet\b").is_match(l)
    {
        add(
            T::Waiting,
            if re!(r"said they|decided to wait|ready in|let you know when").is_match(l) || short_term_return_re().is_match(l) {
                High
            } else {
                Medium
            },
        );
    }
    if re!(r"more quotes|collecting prices|getting (a few )?(more )?quotes|comparing quotes|compare prices|wants to compare|compare with the other|shop around|other quotes|other ones i am getting|before we make a decision|highest of all my bids|highest bid|better price elsewhere|higher than the others").is_match(l) {
        add(T::ComparingQuotes, High);
    }
    if re!(r"i('ll| will) review|compare with the other|price and quality|evaluating|looking at options").is_match(l) {
        add(T::EvaluatingOptions, High);
    }
    if re!(r"too expensive|over (my |our |the )?budget|can'?t afford|budget is (too )?(high|much)|price is (too )?(high|much)|cheaper options").is_match(l) {
        add(T::BudgetConcern, High);
    }
    if re!(
        r"next year|in a year|at least for a y[ea]*r|future project|down the road|beginning of (the |next )?year|start of (the |next )?year|late (january|february|march|april|may|june|july|august|september|october|november|december)|ready in (january|february|march|april|may|june|july|august|september|october|november|december)|started in late|минимум \d+\s*месяц|building a home|building our home|choosing vendors ahead",
        "i"
    )
    .is_match(l)
    {
        add(T::FutureProject, Medium);
    }
    if re!(r"still in the planning process|in the planning process|still planning|planning process")
        .is_match(l)
    {
        add(T::Planning, High);
    }
    if re!(r"moving forward with (you|granite depot|us)").is_match(l)
        || re!(r"let'?s (do|book|schedule) (it|the )").is_match(l)
        || re!(r"signed (the |our |your )?contract").is_match(l)
        || re!(r"we (just )?received (your |the )?(check|payment|deposit)").is_match(l)
        || re!(r"received your check").is_match(l)
    {
        add(T::MovingForward, High);
    }
    if re!(r"almost ready for counter").is_match(l) {
        add(T::AlmostReady, High);
    }
    if re!(r"still interested|want to move forward|need a quote|can you send").is_match(l)
        || (re!(r"looking at").is_match(l)
            && !re!(r"no point looking at|not ready|hold for now").is_match(l))
    {
        add(T::ActiveInterest, Medium);
    }
    if found.is_empty() {
        return vec![MeaningfulCustomerIntent {
            kind: T::Unknown,
            source,
            timestamp,
            evidence: text.to_string(),
            confidence: Low,
        }];
    }
    sort_by_priority(&mut found);
    found
}

/// The most decisive intent in one text.
pub fn classify_meaningful_customer_intent(
    text: &str,
    channel: Option<IntentChannel>,
    at: Option<&str>,
) -> MeaningfulCustomerIntent {
    classify_all_meaningful_customer_intents(text, channel, at).swap_remove(0)
}

fn profile_line_minute(line: &str) -> Option<String> {
    re!(r"\((\d{4}-\d{2}-\d{2}[ T]\d{2}:\d{2})")
        .captures(line)
        .and_then(|caps| caps.get(1))
        .map(|m| m.as_str().to_string())
}

/// The latest deal note that says something about intent.
pub fn classify_intent_from_salesperson_notes(profile: &str) -> Option<MeaningfulCustomerIntent> {
    profile
        .split('\n')
        .rev()
        .filter(|line| line.starts_with("Deal note "))
        .find_map(|line| {
            let intent = classify_meaningful_customer_intent(
                line,
                Some(IntentChannel::SalespersonNote),
                profile_line_minute(line).as_deref(),
            );
            (intent.kind != MeaningfulCustomerIntentType::Unknown).then_some(intent)
        })
}

/// A tapback or a bare "thanks".
pub fn is_reaction_or_acknowledgment(text: &str) -> bool {
    let trimmed = trim(text);
    if trimmed.is_empty() {
        return false;
    }
    re!(r"^(liked|loved)\b", "i").is_match(trimmed)
        || re!(r"^(thanks|thank you|got it|okay|ok|👍)[.!\s]*$", "i").is_match(trimmed)
}

fn is_auto_reply_noise(text: &str) -> bool {
    let trimmed = trim(text);
    !trimmed.is_empty()
        && (re!(r"^i'?m driving\b", "i").is_match(trimmed)
            || re!(r"sent from my car", "i").is_match(trimmed)
            || re!(r"automatic reply|out of office", "i").is_match(trimmed))
}

fn pick_highest_priority_intent(
    intents: &[MeaningfulCustomerIntent],
) -> Option<MeaningfulCustomerIntent> {
    INTENT_PRIORITY
        .iter()
        .find_map(|kind| intents.iter().find(|item| item.kind == *kind))
        .or_else(|| intents.first())
        .cloned()
}

/// Intents from deal notes (with their continuation lines) and CRM
/// activities, latest first per type, most decisive first.
pub fn classify_all_intents_from_salesperson_notes(profile: &str) -> Vec<MeaningfulCustomerIntent> {
    struct Chunk {
        text: String,
        at: Option<String>,
    }
    let stop = re!(
        r"^(Name:|Company:|Phone:|Deal:|Activity:|Deal note |Customer message:|Lead details:|You \(|Address:|Source:|Remove |Customer page|Postal |City:)",
        "i"
    );
    let mut chunks: Vec<Chunk> = Vec::new();
    let mut current: Option<Chunk> = None;
    for raw in profile.split('\n') {
        let line = raw.trim_end_matches(|c: char| c.is_whitespace() || c == '\u{feff}');
        if line.starts_with("Deal note ") {
            chunks.extend(current.take());
            current = Some(Chunk {
                text: line.to_string(),
                at: profile_line_minute(line),
            });
            continue;
        }
        if line.starts_with("Activity:") {
            chunks.extend(current.take());
            let cleaned =
                re!(r"— do not contact the customer before this date", "gi").replace_all(line, "");
            let cleaned = re!(r"^Activity:\s*", "u").replace(&cleaned, "");
            chunks.push(Chunk {
                text: trim(cleaned.split(" · ").next().unwrap_or("")).to_string(),
                at: profile_line_minute(line),
            });
            continue;
        }
        if let Some(chunk) = current.as_mut()
            && !trim(line).is_empty()
            && !stop.is_match(line)
        {
            chunk.text = format!("{}\n{}", chunk.text, trim(line));
            continue;
        }
        chunks.extend(current.take());
    }
    chunks.extend(current);
    let mut merged: Vec<MeaningfulCustomerIntent> = Vec::new();
    for chunk in chunks.iter().rev() {
        if trim(&chunk.text).is_empty() {
            continue;
        }
        for intent in classify_all_meaningful_customer_intents(
            &chunk.text,
            Some(IntentChannel::SalespersonNote),
            chunk.at.as_deref(),
        ) {
            if intent.kind != MeaningfulCustomerIntentType::Unknown
                && !merged.iter().any(|item| item.kind == intent.kind)
            {
                merged.push(intent);
            }
        }
    }
    sort_by_priority(&mut merged);
    merged
}

fn add_unique(into: &mut Vec<MeaningfulCustomerIntent>, items: Vec<MeaningfulCustomerIntent>) {
    for item in items {
        if item.kind != MeaningfulCustomerIntentType::Unknown
            && !into.iter().any(|existing| existing.kind == item.kind)
        {
            into.push(item);
        }
    }
}

/// Every intent across the conversation and the deal notes.
pub fn classify_intents_from_conversation(
    inbound_texts: &[IntentText],
    profile: Option<&str>,
) -> Vec<MeaningfulCustomerIntent> {
    let mut merged = Vec::new();
    for inbound in inbound_texts {
        if is_reaction_or_acknowledgment(&inbound.text) {
            continue;
        }
        add_unique(
            &mut merged,
            classify_all_meaningful_customer_intents(
                &inbound.text,
                inbound.channel,
                inbound.at.as_deref(),
            ),
        );
    }
    if let Some(profile) = profile.filter(|p| !p.is_empty()) {
        add_unique(
            &mut merged,
            classify_all_intents_from_salesperson_notes(profile),
        );
    }
    if merged.is_empty()
        && let Some(first) = inbound_texts.first()
    {
        return classify_all_meaningful_customer_intents(
            &first.text,
            first.channel,
            first.at.as_deref(),
        );
    }
    sort_by_priority(&mut merged);
    merged
}

/// "Yes, we have" in answer to "have you given up on the project?".
pub fn classify_abandonment_reply(
    text: &str,
    prior_outbound_text: &str,
) -> Option<MeaningfulCustomerIntent> {
    let trimmed = trim(text);
    let lower = trimmed.to_lowercase();
    let prior = prior_outbound_text.to_lowercase();
    let asked = re!(
        r"given up on (the )?project|abandon(ed)? the project|go another direction|moved on from"
    )
    .is_match(&prior);
    let agreed = re!(
        r"^(yes|yeah|yep)[,.]?\s*(sorry[, ]*)?(we |i )?have[.!]?\s*$",
        "i"
    )
    .is_match(trimmed)
        || re!(r"\b(yes|yeah|yep)[,.]?\s*(sorry[, ]*)?(we |i )?(have|did)\b").is_match(&lower);
    (asked && agreed).then(|| MeaningfulCustomerIntent {
        kind: MeaningfulCustomerIntentType::NotInterested,
        source: MeaningfulCustomerIntentSource::Unknown,
        timestamp: None,
        evidence: trimmed.to_string(),
        confidence: IntentConfidence::High,
    })
}

/// A visit actually put on the calendar (not an open "schedule" task, not
/// "I'll visit when I have time").
pub fn text_confirms_scheduled_visit(text: &str) -> bool {
    re!(
        r"\b(?:add|added|adding|putting) (?:you|her|him|them) (?:on|to) the schedule\b|\b(?:you(?:'| a)?re|she(?:'| i)?s|he(?:'| i)?s|they(?:'| a)?re) on the schedule\b|\bplan(?:ning)? to come (?:back|in)\b|\bcome back to the (?:store|showroom)\b|\bshooting for (?:monday|tuesday|wednesday|thursday|friday|saturday|sunday)\b|\btrying to come\b|\bcome one evening\b",
        "i"
    )
    .is_match(text)
}

/// A message time in ms; a zoneless time is UTC.
pub fn message_time_ms(at: Option<&str>) -> Option<i64> {
    let trimmed = trim(at?);
    if trimmed.is_empty() {
        return None;
    }
    let with_t = if trimmed.contains('T') {
        trimmed.to_string()
    } else {
        trimmed.replacen(' ', "T", 1)
    };
    let has_zone =
        re!(r"Z$", "u").is_match(&with_t) || re!(r"[+-]\d{2}:\d{2}$", "u").is_match(&with_t);
    let iso = if has_zone {
        with_t
    } else if super::js::js_len(&with_t) == 16 {
        format!("{with_t}:00.000Z")
    } else {
        format!("{with_t}Z")
    };
    js_date_parse(&iso)
}

/// A message (text and time) as the visit/recontact readers take it.
#[derive(Debug, Clone, Default)]
pub struct TimedText {
    pub text: String,
    pub at: Option<String>,
}

/// "I'll reach out when..." lines made moot by a visit scheduled later.
pub fn will_recontact_texts_superseded_by_later_visit(messages: &[TimedText]) -> BTreeSet<String> {
    let dated: Vec<(String, i64)> = messages
        .iter()
        .filter_map(|message| {
            let text = trim(&message.text).to_string();
            let time = message_time_ms(message.at.as_deref())?;
            (!text.is_empty()).then_some((text, time))
        })
        .collect();
    let Some(latest_visit) = dated
        .iter()
        .filter(|(text, _)| text_confirms_scheduled_visit(text))
        .map(|(_, time)| *time)
        .max()
    else {
        return BTreeSet::new();
    };
    dated
        .into_iter()
        .filter(|(text, time)| {
            *time < latest_visit
                && classify_all_meaningful_customer_intents(text, None, None)
                    .iter()
                    .any(|item| item.kind == MeaningfulCustomerIntentType::CustomerWillRecontact)
        })
        .map(|(text, _)| text)
        .collect()
}

/// The text without the superseded statements.
pub fn omit_superseded_will_recontact_text(text: &str, superseded: &BTreeSet<String>) -> String {
    superseded
        .iter()
        .filter(|statement| !statement.is_empty())
        .fold(text.to_string(), |next, statement| {
            next.split(statement.as_str()).collect::<Vec<_>>().join(" ")
        })
}

fn will_recontact_intent_is_stale(
    intent: &MeaningfulCustomerIntent,
    superseded: &BTreeSet<String>,
    latest_visit_at: Option<i64>,
) -> bool {
    if intent.kind != MeaningfulCustomerIntentType::CustomerWillRecontact {
        return false;
    }
    if let (Some(at), Some(visit)) = (
        message_time_ms(intent.timestamp.as_deref()),
        latest_visit_at,
    ) {
        return at < visit;
    }
    let evidence = trim(&intent.evidence);
    superseded.iter().any(|statement| {
        !statement.is_empty() && (evidence == statement || evidence.contains(statement.as_str()))
    })
}

/// The latest intent and the rest, without "I'll reach out" promises a later
/// scheduled visit replaced.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ResolvedIntents {
    pub latest: Option<MeaningfulCustomerIntent>,
    pub additional: Vec<MeaningfulCustomerIntent>,
}

pub fn drop_will_recontact_superseded_by_later_visit(
    latest: Option<MeaningfulCustomerIntent>,
    additional: Vec<MeaningfulCustomerIntent>,
    messages: &[TimedText],
    superseded_texts: &BTreeSet<String>,
) -> ResolvedIntents {
    let latest_visit_at = messages
        .iter()
        .filter(|message| text_confirms_scheduled_visit(&message.text))
        .filter_map(|message| message_time_ms(message.at.as_deref()))
        .max();
    let stale = |intent: &MeaningfulCustomerIntent| {
        will_recontact_intent_is_stale(intent, superseded_texts, latest_visit_at)
    };
    let additional: Vec<MeaningfulCustomerIntent> =
        additional.into_iter().filter(|item| !stale(item)).collect();
    match latest {
        Some(latest) if stale(&latest) => {
            let replacement = pick_highest_priority_intent(&additional);
            let additional = match &replacement {
                Some(replacement) => additional
                    .into_iter()
                    .filter(|item| item.kind != replacement.kind)
                    .collect(),
                None => additional,
            };
            ResolvedIntents {
                latest: replacement,
                additional,
            }
        }
        latest => ResolvedIntents { latest, additional },
    }
}

/// The customer's latest meaningful intent (the newest message that says
/// something), and the other intents seen in the conversation and notes.
pub fn resolve_latest_meaningful_customer_intent(
    inbound_texts: &[IntentText],
    profile: Option<&str>,
    prior_outbound_for_latest: Option<&str>,
) -> ResolvedIntents {
    let mut chronological: Vec<&IntentText> = inbound_texts.iter().collect();
    let time = |item: &IntentText| {
        item.at
            .as_deref()
            .filter(|at| !at.is_empty())
            .map_or(Some(0), |at| js_date_parse(&at.replacen(' ', "T", 1)))
    };
    js_sort_by(&mut chronological, |left, right| {
        match (time(left), time(right)) {
            #[allow(clippy::cast_precision_loss)]
            (Some(l), Some(r)) => (l - r) as f64,
            _ => f64::NAN,
        }
    });
    let noise = |text: &str| is_reaction_or_acknowledgment(text) || is_auto_reply_noise(text);
    let mut all_found = Vec::new();
    for inbound in &chronological {
        if !noise(&inbound.text) {
            add_unique(
                &mut all_found,
                classify_all_meaningful_customer_intents(
                    &inbound.text,
                    inbound.channel,
                    inbound.at.as_deref(),
                ),
            );
        }
    }
    let profile = profile.filter(|p| !p.is_empty());
    if let Some(profile) = profile {
        add_unique(
            &mut all_found,
            classify_all_intents_from_salesperson_notes(profile),
        );
    }
    let mut latest: Option<MeaningfulCustomerIntent> = None;
    for index in (0..chronological.len()).rev() {
        let inbound = chronological[index];
        if noise(&inbound.text) {
            continue;
        }
        let prior = if index == chronological.len() - 1 {
            prior_outbound_for_latest.unwrap_or("").to_string()
        } else {
            chronological[index + 1].text.clone()
        };
        if let Some(abandonment) = classify_abandonment_reply(&inbound.text, &prior) {
            latest = Some(abandonment);
            break;
        }
        if let Some(first) = classify_all_meaningful_customer_intents(
            &inbound.text,
            inbound.channel,
            inbound.at.as_deref(),
        )
        .into_iter()
        .find(|item| item.kind != MeaningfulCustomerIntentType::Unknown)
        {
            latest = Some(first);
            break;
        }
    }
    if latest.is_none() && !all_found.is_empty() {
        latest = pick_highest_priority_intent(&all_found);
    }
    if latest.is_none() {
        latest = profile.and_then(classify_intent_from_salesperson_notes);
    }
    latest = latest.filter(|intent| intent.kind != MeaningfulCustomerIntentType::Unknown);
    let latest_kind = latest.as_ref().map(|intent| intent.kind);
    let additional = all_found
        .into_iter()
        .filter(|item| Some(item.kind) != latest_kind)
        .collect();
    ResolvedIntents { latest, additional }
}

pub fn intent_stops_automated_pursuit(intent: Option<&MeaningfulCustomerIntent>) -> bool {
    intent.is_some_and(|intent| {
        matches!(
            intent.kind,
            MeaningfulCustomerIntentType::ProjectCompleted
                | MeaningfulCustomerIntentType::NotInterested
        )
    })
}

pub fn moved_forward_elsewhere(text: &str) -> bool {
    re!(
        r"moving forward with (our |my |the )?(designer|contractor|builder|architect|another company)|going with (another|a different) (company|contractor|fabricator|designer)|already (picked|chose|selected).{0,40}(on our own|with our designer|ourselves)",
        "i"
    )
    .is_match(text)
}

pub const fn intent_prefers_low_pressure_phase(kind: Option<MeaningfulCustomerIntentType>) -> bool {
    use MeaningfulCustomerIntentType as T;
    matches!(
        kind,
        Some(
            T::Waiting
                | T::Delayed
                | T::ComparingQuotes
                | T::EvaluatingOptions
                | T::BudgetConcern
                | T::FutureProject
                | T::Planning
                | T::NotNow
                | T::CustomerWillRecontact
                | T::AlmostReady
        )
    )
}
