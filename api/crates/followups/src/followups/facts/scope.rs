//! What the customer is buying (countertops, cabinets, a remodel) and who
//! an activity is for (part of `smsFlowAiFollowupFacts.ts`).

use super::super::js::{js_regex, trim};
use super::materials::countertop_material_fancy;
use super::project::{customer_request_evidence_text, is_cabinet_prerequisite_language};
use super::quotes::normalize_product_scope_typos;
use super::types::{
    ActivityActor, DealProductScope, EvidenceConfidence, EvidenceSource, QuoteType,
};
use crate::re;

/// `uniqueEvidenceChunks`: distinct lines, without the customer prefixes.
pub fn unique_evidence_chunks(texts: &[&str]) -> Vec<String> {
    let mut chunks: Vec<String> = Vec::new();
    for text in texts {
        for raw in text.split('\n') {
            let stripped = re!(r"^(Customer message|Lead details):\s*", "iu").replace(raw, "");
            let line = trim(&stripped);
            if line.is_empty() || chunks.iter().any(|existing| existing == line) {
                continue;
            }
            chunks.push(line.to_string());
        }
    }
    chunks
}

pub fn priority_scope_evidence(profile: &str) -> String {
    let lines: Vec<&str> = profile.split('\n').collect();
    let mut parts: Vec<&str> = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        if re!(
            r"^(Project type:|Customer message:|Lead details:|When they want to start:|Deal note |Notes:)",
            "i"
        )
        .is_match(line)
        {
            parts.push(line);
            for continuation in lines.iter().skip(index + 1) {
                let continuation = trim(continuation);
                if continuation.is_empty() || re!(r"^[A-Z][^:\n]{0,40}:").is_match(continuation) {
                    break;
                }
                parts.push(continuation);
            }
        }
    }
    parts.join("\n")
}

/// Where the project's scope comes from, and how sure it is.
pub fn infer_project_scope_evidence(
    profile: &str,
    quote_type: Option<QuoteType>,
) -> (EvidenceSource, EvidenceConfidence) {
    let priority_text = priority_scope_evidence(profile);
    if scope_from_priority_evidence(&priority_text).is_some() {
        for (pattern, source) in [
            (r"^Lead details:", EvidenceSource::DealRecord),
            (r"^Customer message:", EvidenceSource::CustomerMessage),
            (r"^Deal note ", EvidenceSource::SalespersonNote),
            (r"^Project type:", EvidenceSource::DealRecord),
        ] {
            if js_regex(pattern, "im").is_match(&priority_text) {
                return (source, EvidenceConfidence::Medium);
            }
        }
    }
    match quote_type {
        Some(QuoteType::Cabinet) => {
            return (EvidenceSource::CrmActivity, EvidenceConfidence::Medium);
        }
        Some(QuoteType::Countertop) => {
            return (EvidenceSource::Conversation, EvidenceConfidence::Medium);
        }
        _ => {}
    }
    if re!(r"\bcountertops?\b", "i").is_match(profile) {
        return (EvidenceSource::Conversation, EvidenceConfidence::Medium);
    }
    if re!(r"\bcabinets?\b", "i").is_match(profile)
        && re!(r"\bcountertops?\b", "i").is_match(profile)
    {
        return (EvidenceSource::Conversation, EvidenceConfidence::Medium);
    }
    (EvidenceSource::None, EvidenceConfidence::Low)
}

fn scope_from_priority_evidence(text: &str) -> Option<DealProductScope> {
    if trim(text).is_empty() {
        return None;
    }
    let normalized = normalize_product_scope_typos(text);
    let cabinet_as_prerequisite = is_cabinet_prerequisite_language(&normalized);
    let cabinet = !cabinet_as_prerequisite
        && re!(
            r"cabinetr|cabinet project|kitchen cabinetry|quote for cabinets|cabinet fronts|\bcabinets?\b|kitchen cabinets",
            "i"
        )
        .is_match(&normalized);
    let countertop = re!(
        r"\bcountertops?\b|\bcounters?\b|cabinet and countertops|countertop and cabinet|measurments? for the countertops|measurements for the countertops|provided the measur",
        "i"
    )
    .is_match(&normalized);
    if re!(
        r"complete kitchen repair|complete kitchen remodel|full remodel package|kitchen remod",
        "i"
    )
    .is_match(&normalized)
        && (cabinet || re!(r"cabinet", "i").is_match(&normalized))
    {
        return Some(
            if countertop || re!(r"counterops|stone|granite|quartz", "i").is_match(&normalized) {
                DealProductScope::CabinetsAndCountertops
            } else {
                DealProductScope::FullRemodel
            },
        );
    }
    if cabinet && countertop {
        return Some(DealProductScope::CabinetsAndCountertops);
    }
    if cabinet && !countertop {
        return Some(DealProductScope::Cabinets);
    }
    if countertop {
        return Some(DealProductScope::Countertops);
    }
    if super::super::js::fancy_test(countertop_material_fancy(), &normalized)
        || re!(
            r"\b(quartz|soap\s*stone|granite|marble|quartzite|dekton)\b",
            "i"
        )
        .is_match(&normalized)
    {
        return Some(DealProductScope::Countertops);
    }
    None
}

/// What the deal is for: countertops, cabinets, both, or a full remodel.
pub fn infer_deal_product_scope(profile: &str, quote_type: Option<QuoteType>) -> DealProductScope {
    if quote_type == Some(QuoteType::FullRemodel) {
        return DealProductScope::FullRemodel;
    }
    let customer_scope_evidence = customer_request_evidence_text(profile, profile);
    if re!(
        r"kitchen remodel package|full remodel package|\bfull remodel\b",
        "i"
    )
    .is_match(&customer_scope_evidence)
    {
        return DealProductScope::FullRemodel;
    }
    if let Some(priority) = scope_from_priority_evidence(&priority_scope_evidence(profile)) {
        return priority;
    }
    let mut target_specific = normalize_product_scope_typos(profile);
    for pattern in [
        r"https?:\/\/\S+",
        r"granite depot",
        r"customer page url.*",
        r"granite-manager\.com\S*",
        r"save \d+%[^\n]*",
        r"natural stone countertops have been on your wishlist[^\n]*",
        r"thanks for reaching out about (your )?(kitchen|countertop)[^\n]*",
        r"\byour countertop project\b",
        r"\byour kitchen project\b",
        r"recently reached out to us about new countertops[^\n]*",
    ] {
        target_specific = js_regex(pattern, "gi")
            .replace_all(&target_specific, " ")
            .into_owned();
    }
    let target_specific = target_specific
        .split('\n')
        .filter(|line| {
            !re!(r"^EMAIL (Company|Customer):", "i").is_match(line)
                && !re!(r"browse our stone|stones you can", "i").is_match(line)
        })
        .collect::<Vec<_>>()
        .join("\n");
    let cabinet_as_prerequisite = is_cabinet_prerequisite_language(&target_specific);
    let cabinet = !cabinet_as_prerequisite
        && re!(
            r"cabinet quote|quote for cabinets|cabinets? quote|cabinet design|karamel cabinets|cabinet project|white cabinets|black cabinets|\bcabinets\b",
            "i"
        )
        .is_match(&target_specific);
    let countertop = re!(
        r"countertop quote|purchasing countertops|interested in purchasing countertops|\byour countertop project\b|countertop project|stone quote|slab quote|granite quote|quartz quote|\bmsi quartz\b|\bisland\b|fabricated .{0,40}countertop|looking for a fabricated|\bcountertops?\b",
        "i"
    )
    .is_match(&target_specific);
    if quote_type == Some(QuoteType::Cabinet) && !countertop {
        return DealProductScope::Cabinets;
    }
    if quote_type == Some(QuoteType::Countertop) {
        return DealProductScope::Countertops;
    }
    if cabinet && countertop {
        return DealProductScope::CabinetsAndCountertops;
    }
    if cabinet || quote_type == Some(QuoteType::Cabinet) {
        return DealProductScope::Cabinets;
    }
    if countertop {
        return DealProductScope::Countertops;
    }
    DealProductScope::Unknown
}

/// Who an activity is for: the salesperson ("call her") or the customer.
pub fn infer_activity_actor(activity_name_text: &str) -> ActivityActor {
    let t = activity_name_text.to_lowercase();
    if re!(r"^call[, ]|tell (him|her|them|that)|sent her|sent him|we should|i should").is_match(&t)
    {
        return ActivityActor::Salesperson;
    }
    if re!(r"customer (will|should)|they will send|ask (him|her|them) to send").is_match(&t) {
        return ActivityActor::Customer;
    }
    ActivityActor::Unknown
}

/// Whether later messages show the activity was done.
pub fn activity_looks_satisfied_by_later_text(activity_name_text: &str, later_text: &str) -> bool {
    let activity = activity_name_text.to_lowercase();
    let later = later_text.to_lowercase();
    if re!(r"prices? are going up|pricing will change|price of quartz").is_match(&activity) {
        return re!(r"prices? .*chang|price of quartz|90% chance").is_match(&later);
    }
    if re!(r"send .{0,20}pictures|pictures of the kitchen").is_match(&activity) {
        return re!(r"attached .{0,20}picture|here are the (kitchen )?pictures").is_match(&later);
    }
    if re!(r"cabinet quote").is_match(&activity) {
        return re!(r"attached your quote|cabinet quote").is_match(&later);
    }
    false
}

fn is_promo_line(line: &str) -> bool {
    re!(
        r"save \d+%|browse our stone|promo|this august|granite depot pictures",
        "i"
    )
    .is_match(line)
}

/// What the customer asked us to do (countertops, cabinets, an island...)
/// and what they asked for that we do not do (flooring, appliances).
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CustomerProjectScope {
    pub customer_project_components: Vec<String>,
    pub unsupported_requested_components: Vec<String>,
}

fn add_component(list: &mut Vec<String>, value: &str) {
    if !list
        .iter()
        .any(|item| item.to_lowercase() == value.to_lowercase())
    {
        list.push(value.to_string());
    }
}

fn process_scope_line(line: &str, components: &mut Vec<String>) {
    let mut add = |value: &str| add_component(components, value);
    if re!(r"^floors?\.?$", "i").is_match(line) || re!(r"\bflooring\b", "i").is_match(line) {
        add("flooring");
    }
    if re!(
        r"want(?:ing)? counters?(?:tops?)?|counters?, cabinets|cabinets and counters?",
        "i"
    )
    .is_match(line)
    {
        add("countertops");
        add("cabinets");
    }
    if re!(r"extend(?:ing)? (?:the )?island|island extension", "i").is_match(line) {
        add("island or bar");
    }
    if re!(
        r"in[- ]?home estimate.{0,40}(?:cabinets?|countertops?)|needs inhome estimate cabinets \+ countertops",
        "i"
    )
    .is_match(line)
    {
        add("cabinets");
        add("countertops");
    }
    if re!(r"\bisland\b", "i").is_match(line)
        && re!(r"looking for an island|for an island", "i").is_match(line)
    {
        add("island or bar");
        add("countertops");
    }
    if re!(
        r"\bgranite and quartz\b|\bcountertop sample|\bsee and feel.*sample|\bwhite granite\b|\bbathroom countertop\b",
        "i"
    )
    .is_match(line)
    {
        add("countertops");
    }
    if re!(
        r"redo (?:her |his |their |the )?kitchen|wants to redo kitchen|want'?s to redo .{0,12}kitchen|kitchen remodel package|interested in .{0,40}(?:kitchen remodel|11,999|11999)",
        "i"
    )
    .is_match(line)
    {
        add("kitchen remodel");
        add("cabinets");
        add("countertops");
    }
    if re!(r"remove (?:the )?wall|wall removal|remove the wall", "i").is_match(line) {
        add("wall removal");
    }
    if re!(
        r"build an island|island or a bar|island there|or a bar",
        "i"
    )
    .is_match(line)
    {
        add("island or bar");
    }
    if re!(
        r"complete kitchen repair|complete kitchen remodel|kitchen remod",
        "i"
    )
    .is_match(line)
    {
        add("kitchen remodel");
        if re!(r"cabinet|repair|remodel", "i").is_match(line) {
            add("cabinets");
            add("countertops");
        }
    }
    if re!(
        r"kitchen remod.*cabinet|including cabinets|wants to get the kitchen cabinets",
        "i"
    )
    .is_match(line)
    {
        add("cabinets");
        add("countertops");
    }
    if re!(r"\bappliances?\b", "i").is_match(line) {
        add("appliances");
    }
    if re!(r"\blighting\b|\blights\b", "i").is_match(line) {
        add("lighting");
    }
}

pub fn extract_customer_project_scope(
    text: &str,
    rep_clarification_text: &str,
) -> CustomerProjectScope {
    let mut components: Vec<String> = Vec::new();
    let mut unsupported: Vec<String> = Vec::new();
    let scoped_lines: Vec<&str> = text
        .split('\n')
        .filter(|raw| {
            !is_promo_line(raw)
                && (re!(
                    r"^Customer message:|^Lead details:|^Deal note|^Activity:|^Project type:",
                    "i"
                )
                .is_match(raw)
                    || re!(r"^Name:", "i").is_match(raw))
        })
        .collect();
    let scope_text = scoped_lines.join("\n");
    for raw in &scoped_lines {
        let line = re!(r"^(Customer message:|Lead details:)\s*", "iu").replace(raw, "");
        let line = re!(r"^Deal note \(\d{4}-\d{2}-\d{2}[^)]*\):\s*", "i").replace(&line, "");
        process_scope_line(trim(&line), &mut components);
    }
    for raw in text.split('\n') {
        if re!(
            r"^(Activity:|Deal:|Name:|EMAIL |Deal note|Project type:)",
            "i"
        )
        .is_match(raw)
            || is_promo_line(raw)
        {
            continue;
        }
        let trimmed = trim(raw);
        if trimmed.is_empty() || scoped_lines.contains(&raw) {
            continue;
        }
        process_scope_line(trimmed, &mut components);
    }
    if re!(
        r"\bcabinets and counterops\b|\bcabinets and countertops\b",
        "i"
    )
    .is_match(&scope_text)
    {
        add_component(&mut components, "cabinets");
        add_component(&mut components, "countertops");
    }
    let with_rep = format!("{text}\n{rep_clarification_text}");
    if re!(
        r"\bnot floor\b|\bdon'?t do floor\b|\bwe don'?t (do|handle) floor",
        "i"
    )
    .is_match(&with_rep)
    {
        add_component(&mut unsupported, "flooring");
    }
    if re!(r"\bappliances?\b", "i").is_match(&scope_text)
        && !re!(r"\bwe (do|install|supply) appliances\b", "i").is_match(&with_rep)
    {
        add_component(&mut unsupported, "appliances");
    }
    if re!(r"\blighting\b|\blights\b", "i").is_match(&scope_text)
        && !re!(r"\bwe (do|install|supply) lighting\b", "i").is_match(&with_rep)
    {
        add_component(&mut unsupported, "lighting");
    }
    if unsupported
        .iter()
        .any(|item| re!(r"floor", "i").is_match(item))
    {
        components.retain(|item| !re!(r"floor", "i").is_match(item));
    }
    CustomerProjectScope {
        customer_project_components: components,
        unsupported_requested_components: unsupported,
    }
}

/// The customer's own words: the latest messages plus the lead's message
/// (with its continuation lines).
pub fn customer_originated_text(latest_customer: &str, profile: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    if !trim(latest_customer).is_empty() {
        parts.push(latest_customer);
    }
    let lines: Vec<&str> = profile.split('\n').collect();
    for (index, line) in lines.iter().enumerate() {
        if re!(r"^Customer message:", "i").is_match(line)
            || re!(r"^Lead details:", "i").is_match(line)
        {
            parts.push(line);
            for continuation in lines.iter().skip(index + 1) {
                let continuation = trim(continuation);
                if continuation.is_empty() || re!(r"^[A-Z][^:\n]{0,40}:").is_match(continuation) {
                    break;
                }
                parts.push(continuation);
            }
        }
    }
    unique_evidence_chunks(&parts).join("\n")
}
