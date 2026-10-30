//! Which of a customer's projects (deals, addresses) a line of history is
//! about, so facts from another property stay out of this deal's follow-up.
//! Port of `smsFlowAiProjectAttribution.ts`.

use super::js::trim;
use crate::{re, str_enum};
use serde::Serialize;

str_enum! { pub enum EvidenceTargetScope {
    CustomerLevel = "CUSTOMER_LEVEL", TargetDeal = "TARGET_DEAL", OtherDeal = "OTHER_DEAL", UnknownProject = "UNKNOWN_PROJECT",
} }

/// The customer's projects as far as the history shows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectAttributionSummary {
    pub multi_project_customer: bool,
    pub target_deal_id: Option<i64>,
    pub target_deal_status_label: Option<String>,
    pub target_deal_is_terminal: bool,
    pub target_address: Option<String>,
    pub other_deal_ids: Vec<i64>,
    pub project_markers: Vec<String>,
    pub other_project_markers: Vec<String>,
}

fn normalize_marker(value: &str) -> String {
    re!(r"\s+").replace_all(trim(value), " ").to_lowercase()
}

fn unique_markers(markers: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut result = Vec::new();
    for marker in markers {
        let normalized = normalize_marker(&marker);
        if normalized.is_empty() || !seen.insert(normalized) {
            continue;
        }
        result.push(trim(&marker).to_string());
    }
    result
}

fn parse_id(text: &str) -> Option<i64> {
    text.parse::<i64>().ok()
}

fn extract_deal_ids(text: &str) -> Vec<i64> {
    re!(r"(?:Target|Other|Deal:?)\s*#(\d+)", "gi")
        .captures_iter(text)
        .filter_map(|caps| parse_id(&caps[1]))
        .collect()
}

fn infer_target_deal_id_from_profile(
    profile: &str,
    explicit_target_deal_id: Option<i64>,
) -> Option<i64> {
    explicit_target_deal_id.or_else(|| {
        profile.split('\n').find_map(|line| {
            re!(r"Deal:\s*Target\s*#(\d+)", "i")
                .captures(line)
                .and_then(|caps| parse_id(&caps[1]))
        })
    })
}

fn extract_other_deal_ids_from_profile(profile: &str) -> Vec<i64> {
    profile
        .split('\n')
        .flat_map(|line| {
            re!(r"Other\s*#(\d+)", "gi")
                .captures_iter(line)
                .filter_map(|caps| parse_id(&caps[1]))
        })
        .collect()
}

fn extract_street_addresses(text: &str) -> Vec<String> {
    unique_markers(
        re!(
            r"\b\d+\s+[A-Za-z0-9][A-Za-z0-9\s.'-]{0,48}(?:St\.?|Street|Dr\.?|Drive|Ave\.?|Avenue|Rd\.?|Road|Blvd\.?|Lane|Ln\.?|Way|Court|Ct\.?|str\.?)\b",
            "gi"
        )
        .find_iter(text)
        .map(|m| m.as_str().to_string())
        .filter(|marker| !re!(r"\b\d+\s+(?:app?artments?|units?|kitchens?|condos?|homes?)\b", "i").is_match(marker)),
    )
}

fn extract_named_project_markers(text: &str) -> Vec<String> {
    unique_markers(
        re!(
            r"\b(carica|carika|broadway|thornleigh|danville|pepperidge|cherry st|hardwood)\b",
            "gi"
        )
        .find_iter(text)
        .map(|m| m.as_str().to_string()),
    )
}

fn extract_semantic_project_markers(text: &str) -> Vec<String> {
    let mut markers: Vec<String> = Vec::new();
    let mut push = |marker: &str| markers.push(marker.to_string());
    if re!(r"\bfireplace\b", "i").is_match(text) {
        push("fireplace");
    }
    if re!(
        r"\blater kitchen\b|\banother kitchen\b|\bother kitchen\b",
        "i"
    )
    .is_match(text)
    {
        push("later kitchen");
    }
    if re!(r"\bother project\b|\banother project\b", "i").is_match(text) {
        push("other project");
    }
    if re!(
        r"\banother (house|home|property)\b|\bother (house|home|property)\b|\banother house\b",
        "i"
    )
    .is_match(text)
    {
        push("another property");
    }
    if re!(r"\bkitchen\b", "i").is_match(text) && re!(r"\bfireplace\b", "i").is_match(text) {
        push("kitchen");
    }
    if re!(
        r"\bother projects?\b|\bnew promotion\b|\banother (invoice|template|contract)\b",
        "i"
    )
    .is_match(text)
    {
        push("other project");
    }
    unique_markers(markers)
}

fn extract_project_markers(text: &str) -> Vec<String> {
    unique_markers(
        extract_street_addresses(text)
            .into_iter()
            .chain(extract_named_project_markers(text)),
    )
}

fn marker_matches_target(marker: &str, target_address: Option<&str>) -> bool {
    let normalized = normalize_marker(marker);
    if normalized.is_empty() {
        return false;
    }
    target_address.is_some_and(|address| {
        let address = normalize_marker(address);
        address.contains(&normalized) || normalized.contains(&address)
    })
}

fn push_unique(ids: &mut Vec<i64>, id: i64) {
    if !ids.contains(&id) {
        ids.push(id);
    }
}

pub fn analyze_project_attribution(
    profile: &str,
    target_deal_id: Option<i64>,
    target_deal_status_label: Option<&str>,
    other_deal_ids: &[i64],
    target_deal_is_terminal: bool,
) -> ProjectAttributionSummary {
    let lines: Vec<&str> = profile.split('\n').collect();
    let resolved_target = infer_target_deal_id_from_profile(profile, target_deal_id);
    let mut unique_deal_ids = Vec::new();
    for id in extract_deal_ids(profile) {
        push_unique(&mut unique_deal_ids, id);
    }
    let mut all_other = Vec::new();
    for id in other_deal_ids
        .iter()
        .copied()
        .chain(extract_other_deal_ids_from_profile(profile))
        .chain(
            unique_deal_ids
                .iter()
                .copied()
                .filter(|id| resolved_target.is_some_and(|target| *id != target)),
        )
    {
        push_unique(&mut all_other, id);
    }
    all_other.retain(|id| resolved_target != Some(*id));
    let target_address = lines.iter().find_map(|line| {
        re!(r"^Address:\s*(.+)$", "i")
            .captures(line)
            .and_then(|caps| caps.get(1))
            .filter(|m| !m.as_str().is_empty())
            .map(|m| trim(m.as_str()).to_string())
    });
    let project_markers = extract_project_markers(profile);
    let other_project_markers: Vec<String> = project_markers
        .iter()
        .filter(|marker| !marker_matches_target(marker, target_address.as_deref()))
        .cloned()
        .collect();
    let deal_line_count = lines
        .iter()
        .filter(|line| line.starts_with("Deal:"))
        .count();
    let semantic_markers = extract_semantic_project_markers(profile);
    let multi_project_customer = !all_other.is_empty()
        || deal_line_count > 1
        || other_project_markers.len() > 1
        || semantic_markers.len() > 1
        || (!other_project_markers.is_empty()
            && resolved_target.is_some()
            && target_address.is_none()
            && deal_line_count > 0);
    let status_looks_terminal = re!(r"closed\/won|closed\/lost|closed won|closed lost", "i")
        .is_match(target_deal_status_label.unwrap_or(""));
    ProjectAttributionSummary {
        multi_project_customer,
        target_deal_id: resolved_target,
        target_deal_status_label: target_deal_status_label.map(str::to_string),
        target_deal_is_terminal: target_deal_is_terminal || status_looks_terminal,
        target_address,
        other_deal_ids: all_other,
        project_markers,
        other_project_markers,
    }
}

fn scope_from_markers(
    line: &str,
    summary: &ProjectAttributionSummary,
) -> Option<EvidenceTargetScope> {
    let markers = extract_project_markers(line);
    if markers.is_empty() {
        return None;
    }
    let address = summary.target_address.as_deref();
    let target = markers
        .iter()
        .filter(|m| marker_matches_target(m, address))
        .count();
    let foreign = markers.len() - target;
    Some(if foreign > 0 && target == 0 {
        EvidenceTargetScope::OtherDeal
    } else if foreign > 0 {
        EvidenceTargetScope::UnknownProject
    } else {
        // Only markers of the target property.
        EvidenceTargetScope::TargetDeal
    })
}

/// Which project a line of history is about.
pub fn attribute_project_specific_line(
    line: &str,
    summary: &ProjectAttributionSummary,
) -> EvidenceTargetScope {
    use EvidenceTargetScope as S;
    let multi_or_target = if summary.multi_project_customer {
        S::UnknownProject
    } else {
        S::TargetDeal
    };
    if re!(
        r"^(Name|Company|Phone|Phone 2|Email|Address|Postal code|Source|City|State|Project type|Project size|When they want to start|Sink|Backsplash|Stove|Remove and dispose|Lead details|Customer message|Notes|You \(the salesperson|Customer page URL)",
        "i"
    )
    .is_match(trim(line))
    {
        if let Some(marked) = scope_from_markers(line, summary) {
            return marked;
        }
        if re!(
            r"^(Project type|Project size|When they want to start|Sink|Backsplash|Stove|Remove and dispose|Lead details|Customer message)",
            "i"
        )
        .is_match(trim(line))
            && summary.multi_project_customer
            && summary.target_deal_is_terminal
        {
            return S::UnknownProject;
        }
        return S::CustomerLevel;
    }
    if line.starts_with("Deal:") {
        if summary
            .target_deal_id
            .is_some_and(|id| line.contains(&format!("Target #{id}")))
        {
            return S::TargetDeal;
        }
        if re!(r"Other #\d+").is_match(line) {
            return S::OtherDeal;
        }
        let ids = extract_deal_ids(line);
        if ids.iter().any(|id| summary.other_deal_ids.contains(id)) {
            return S::OtherDeal;
        }
        if summary
            .target_deal_id
            .is_some_and(|target| ids.contains(&target))
        {
            return S::TargetDeal;
        }
        return multi_or_target;
    }
    let line_deal_ids = extract_deal_ids(line);
    if line_deal_ids
        .iter()
        .any(|id| summary.other_deal_ids.contains(id))
    {
        return S::OtherDeal;
    }
    if summary
        .target_deal_id
        .is_some_and(|target| line_deal_ids.contains(&target))
    {
        return S::TargetDeal;
    }
    if let Some(marked) = scope_from_markers(line, summary) {
        marked
    } else {
        if re!(r"^Deal note ", "i").is_match(line) || re!(r"^Activity:", "i").is_match(line) {
            if summary.multi_project_customer && !summary.other_project_markers.is_empty() {
                return S::UnknownProject;
            }
            return S::TargetDeal;
        }
        S::CustomerLevel
    }
}

/// The profile without lines about other projects (when there are any).
pub fn filter_profile_for_target_deal(
    profile: &str,
    summary: &ProjectAttributionSummary,
) -> String {
    if !summary.multi_project_customer {
        return profile.to_string();
    }
    profile
        .split('\n')
        .filter(|line| {
            !trim(line).is_empty()
                && !matches!(
                    attribute_project_specific_line(line, summary),
                    EvidenceTargetScope::OtherDeal | EvidenceTargetScope::UnknownProject
                )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Customer texts about this deal: without foreign ones, or only target ones.
pub fn filter_customer_texts_for_target_deal(
    texts: &[String],
    summary: &ProjectAttributionSummary,
    target_deal_only: bool,
) -> Vec<String> {
    if !summary.multi_project_customer {
        return texts.to_vec();
    }
    texts
        .iter()
        .filter(|text| {
            let scope = attribute_project_specific_line(text, summary);
            if target_deal_only {
                scope == EvidenceTargetScope::TargetDeal
            } else {
                !matches!(
                    scope,
                    EvidenceTargetScope::OtherDeal | EvidenceTargetScope::UnknownProject
                )
            }
        })
        .cloned()
        .collect()
}

/// The pressure fields [`cross_project_attribution_errors`] checks.
pub struct AttributionCheck<'a> {
    pub explicit_customer_request: Option<&'a str>,
    pub unfinished_next_step: Option<&'a str>,
    pub material_preferences: &'a [String],
    pub last_customer_message_summary: Option<&'a str>,
    pub last_conversation_reply: Option<&'a str>,
}

/// Evidence about another property that leaked into this deal's state.
pub fn cross_project_attribution_errors(
    summary: &ProjectAttributionSummary,
    pressure: &AttributionCheck<'_>,
) -> Vec<String> {
    if !summary.multi_project_customer {
        return Vec::new();
    }
    let mut parts = vec![
        pressure.explicit_customer_request.unwrap_or(""),
        pressure.unfinished_next_step.unwrap_or(""),
        pressure.last_customer_message_summary.unwrap_or(""),
        pressure.last_conversation_reply.unwrap_or(""),
    ];
    parts.extend(pressure.material_preferences.iter().map(String::as_str));
    let corpus = parts.join("\n");
    let lower = corpus.to_lowercase();
    let mut errors = Vec::new();
    for marker in &summary.other_project_markers {
        if !lower.contains(&normalize_marker(marker)) {
            continue;
        }
        if summary.target_address.as_deref().is_some_and(|address| {
            !address.is_empty() && normalize_marker(&corpus).contains(&normalize_marker(address))
        }) {
            continue;
        }
        errors.push(format!(
            "target deal state contains project-specific evidence from another property ({marker})"
        ));
    }
    if summary.target_deal_is_terminal
        && re!(
            r"carica|carika|broadway|danville|thornleigh|cherry|hardwood",
            "i"
        )
        .is_match(&corpus)
    {
        errors.push("target deal state contains project-specific evidence from another property (named active property)".into());
    }
    errors
}
