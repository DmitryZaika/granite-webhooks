//! Checks on AI-written follow-up copy against what the CRM knows: tone,
//! unsupported claims, scope and timing (`followupToneErrors`,
//! `validateGeneratedFollowupCopy`, `validateGeneratedFollowupCopyForSendAt`).

use super::super::dates::Instant;
use super::super::facts::timing::{
    body_attributes_planned_showroom_visit_to_rep, body_mentions_planned_event_date,
};
use super::super::facts::{
    DealProductScope, DecisionObstacle, PlannedCustomerEventType, ProjectAreaKind,
};
use super::super::intent::MeaningfulCustomerIntentType as I;
use super::super::js::{escape_reg_exp, fancy_test, js_number, js_regex_try};
use super::state::{relative_timing_copy_errors, timeline_copy_is_valid};
use super::types::{CommunicationHistoryStatus, FollowupContactPressure};
use crate::{fre, re};
use chrono::Duration;
use regex::Regex;

/// Rules for every follow-up: no apologies, no remarks on earlier outreach
/// or on what we don't know, never "<name> here", and no "This is <name>
/// from <company>" unless `allow_intro` (we have never contacted them).
pub fn followup_tone_errors(body: &str, allow_intro: bool) -> Vec<String> {
    let mut errors = Vec::new();
    if re!(r"(?:^|[,.!?;:—–-]\s*)\p{Lu}[\p{L}'’-]+ here\b|\b(?:[Tt]his is|[Ii]t(?:'|’)s) \p{Lu}[\p{L}'’-]+ here\b", "u").is_match(body) {
        errors.push("tone: never write \"<name> here\" — open with the greeting".to_string());
    } else if !allow_intro
        && re!(r"\b(?:[Tt]his is|[Ii]t(?:'|’)s) \p{Lu}[\p{L}'’-]+(?: \p{Lu}[\p{L}'’-]+)? (?:from|with|at)\b", "u").is_match(body)
    {
        errors.push(
            "tone: they already know who is texting — do not introduce yourself (\"This is <name> from <company>\"); open with the greeting"
                .to_string(),
        );
    }
    if fancy_test(
        fre!(
            r"\b(?:sorry(?! to hear)|apologi[sz]e|apologies)\b|\b(?:my|our) (?:last|earlier|previous) (?:texts?|messages?|notes?|emails?) (?:were|was) (?:too )?(?:pushy|a lot)",
            "i"
        ),
        body,
    ) {
        errors.push("tone: do not apologize for or criticize earlier outreach".to_string());
    }
    if re!(
        r"\b(?:i|we) realize (?:that )?(?:my|our) (?:last|earlier|previous)\b|\bwithout knowing what you(?:'|’)?re\b|\b(?:i|we) (?:don(?:'|’)?t|do not) (?:know|have details on|have details about) (?:what|your|which)\b",
        "i"
    )
    .is_match(body)
    {
        errors.push("tone: do not comment on earlier messages or say we don't know their project — they may have visited the shop".to_string());
    }
    errors
}

fn text(value: Option<&String>) -> &str {
    value.map_or("", String::as_str)
}

fn customer_originated_scope_evidence(p: &FollowupContactPressure) -> String {
    [
        text(p.last_customer_originated_information.as_ref()),
        text(p.last_conversation_reply.as_ref()),
        text(p.last_customer_message_summary.as_ref()),
    ]
    .join("\n")
}

/// Every problem with AI-written copy for this customer.
#[allow(clippy::too_many_lines, clippy::cognitive_complexity)]
pub fn validate_generated_followup_copy(body: &str, p: &FollowupContactPressure) -> Vec<String> {
    let mut errors = followup_tone_errors(body, p.days_since_last_outbound.is_none());
    let evidence = [
        text(p.last_customer_originated_information.as_ref()).to_string(),
        text(p.last_conversation_reply.as_ref()).to_string(),
        text(p.last_customer_message_summary.as_ref()).to_string(),
        text(p.hold_reason_evidence.as_ref()).to_string(),
        text(p.construction_dependency_evidence.as_ref()).to_string(),
        text(p.known_scope.as_ref()).to_string(),
        p.project_details.join(" "),
        p.material_preferences.join(" "),
        p.historical_material_preferences.join(" "),
        p.countertop_preferences.join(" "),
        text(p.favorite_material.as_ref()).to_string(),
        text(p.current_confirmed_material.as_ref()).to_string(),
        text(p.unfinished_next_step.as_ref()).to_string(),
    ]
    .join("\n");
    let evidence_lower = evidence.to_lowercase();
    let allowed: Vec<String> = p
        .material_preferences
        .iter()
        .chain(&p.historical_material_preferences)
        .chain(&p.countertop_preferences)
        .chain(&p.cabinet_preferences)
        .chain(p.favorite_material.iter())
        .chain(p.current_confirmed_material.iter())
        .chain(p.known_material.iter())
        .filter(|item| !item.is_empty())
        .map(|item| item.to_lowercase())
        .collect();
    for caps in re!(
        r"\b(Cristallo|Turquoise|Viraldi|Idillio|Black Pearl|Vincent Black|Unique|Taj Mahal)\b",
        "gi"
    )
    .captures_iter(body)
    {
        let name = &caps[1];
        let lower = name.to_lowercase();
        if !name.is_empty() && !allowed.contains(&lower) && !evidence_lower.contains(&lower) {
            errors.push(format!(
                "state/copy inconsistency: material {name} not in structured state"
            ));
        }
    }
    if p.material_preferences
        .iter()
        .any(|name| re!(r"white spring", "i").is_match(name))
        && p.material_type_preferences
            .iter()
            .any(|item| re!(r"granite", "i").is_match(item))
        && re!(
            r"\bwhite spring\b[\s\S]{0,40}\bor\b[\s\S]{0,40}\bgranite\b",
            "i"
        )
        .is_match(body)
    {
        errors
            .push("named stone must not be compared against its parent material type".to_string());
    }
    let claims: [(&Regex, &Regex, &str); 5] = [
        (
            re!(r"home repairs", "i"),
            re!(r"home repairs|repairs? (still )?taking", "i"),
            "home repairs",
        ),
        (
            re!(
                r"unexpected bills|medical expense|money problems|home repairs still",
                "i"
            ),
            re!(r"never-match-financial-invented", "i"),
            "invented financial delay cause",
        ),
        (
            re!(r"new baby|about to have a baby", "i"),
            re!(r"baby", "i"),
            "new baby",
        ),
        (
            re!(r"visited (the )?showroom|came (in|by) the showroom", "i"),
            re!(r"showroom", "i"),
            "showroom visit",
        ),
        (
            re!(
                r"taking care of things at home|AC broke|spent (a lot of )?money",
                "i"
            ),
            re!(r"never-match-sensitive", "i"),
            "sensitive personal delay",
        ),
    ];
    for (claim, need, label) in claims {
        if claim.is_match(body) && !need.is_match(&evidence) {
            errors.push(format!("unsupported customer-specific claim: {label}"));
        }
    }
    let showroom_planned =
        p.planned_customer_event_type == Some(PlannedCustomerEventType::ShowroomVisit);
    if let Some(label) = p
        .planned_customer_event_date_label
        .as_deref()
        .filter(|l| !l.is_empty())
        && showroom_planned
        && re!(
            r"showroom|come (in|by)|stop by|visit (the )?(showroom|store)",
            "i"
        )
        .is_match(body)
        && !body_mentions_planned_event_date(body, label)
    {
        errors.push(format!("planned showroom date {label} must be mentioned"));
    }
    if showroom_planned && body_attributes_planned_showroom_visit_to_rep(body) {
        errors.push(
            "planned showroom visit must stay attributed to the customer, not the salesperson"
                .to_string(),
        );
    }
    if re!(
        r"cabinet (finish|look|style|paint)|painted cabinet|wood grain",
        "i"
    )
    .is_match(body)
        && !matches!(
            p.deal_product_scope,
            DealProductScope::Cabinets | DealProductScope::CabinetsAndCountertops
        )
    {
        errors.push("cabinet-design copy without cabinet product scope".to_string());
    }
    if p.deal_product_scope == DealProductScope::Unknown
        && re!(
            r#"countertop project|if countertops become relevant|quartz vs granite|stone options|natural stone|cabinet and countertop|reply ['"]?cabinets['"]? or ['"]?countertops['"]?|countertop options|compare quartz, granite|quartzite, and marble|your countertop"#,
            "i"
        )
        .is_match(body)
    {
        errors.push("product education without known scope".to_string());
    }
    if re!(
        r"still comparing (?:granite|quartz)|still leaning toward (?:granite|quartz)",
        "i"
    )
    .is_match(body)
        && !p.comparing_quotes
        && !re!(r"still comparing|comparing quotes|shop(ping)? around", "i").is_match(&evidence)
    {
        errors.push("still comparing copy without customer comparison evidence".to_string());
    }
    if let Some(quoted) = p
        .quoted_material_context
        .as_deref()
        .filter(|q| !q.is_empty())
    {
        let material = escape_reg_exp(quoted);
        let strong = p.favorite_material.is_some()
            || p.current_confirmed_material.is_some()
            || p.preferred_material.is_some()
            || re!(
                r"\b(lean|prefer|favorite|picked|selected|going with)\b",
                "i"
            )
            .is_match(&evidence);
        let stated = js_regex_try(
            &format!(
                r"\b(?:still )?(?:leaning toward|prefer(?:ring)?|favorite|your (?:favorite|choice))\b[^.!?]{{0,40}}\b{material}\b|\b{material}\b[^.!?]{{0,40}}\b(?:still )?(?:leaning toward|prefer(?:ring)?|favorite)\b"
            ),
            "i",
        )
        .is_some_and(|re| re.is_match(body));
        if !strong && stated {
            errors.push(
                "quote-context material must not be stated as preference without stronger evidence"
                    .to_string(),
            );
        }
    }
    let customer_scope = customer_originated_scope_evidence(p);
    if p.deal_product_scope == DealProductScope::Unknown {
        let claims: [(&Regex, &Regex, &str); 15] = [
            (
                re!(r"\byour kitchen\b", "i"),
                re!(r"\bkitchen\b", "i"),
                "your kitchen",
            ),
            (
                re!(r"\bmaterials for the kitchen\b", "i"),
                re!(r"\bkitchen\b", "i"),
                "materials for the kitchen",
            ),
            (
                re!(r"\bcountertop project\b", "i"),
                re!(r"countertop", "i"),
                "countertop project",
            ),
            (
                re!(r#"\breply ['"]?kitchen['"]?\b"#, "i"),
                re!(r"\bkitchen\b", "i"),
                "reply kitchen",
            ),
            (
                re!(r"\bfor the kitchen\b", "i"),
                re!(r"\bkitchen\b", "i"),
                "for the kitchen",
            ),
            (
                re!(r"\bworking on the kitchen\b", "i"),
                re!(r"\bkitchen\b", "i"),
                "working on the kitchen",
            ),
            (
                re!(r"\byour countertop\b", "i"),
                re!(r"countertop", "i"),
                "your countertop",
            ),
            (
                re!(r"\bcountertop options\b", "i"),
                re!(r"countertop", "i"),
                "countertop options",
            ),
            (
                re!(r"\bcountertops?\b", "i"),
                re!(r"countertop", "i"),
                "countertops",
            ),
            (re!(r"\bcabinets?\b", "i"), re!(r"cabinet", "i"), "cabinets"),
            (re!(r"\bquartz\b", "i"), re!(r"quartz", "i"), "quartz"),
            (re!(r"\bgranite\b", "i"), re!(r"granite", "i"), "granite"),
            (
                re!(r"\bstone\b", "i"),
                re!(r"stone|quartz|granite|marble|quartzite", "i"),
                "stone",
            ),
            (
                re!(r"\bcompare quartz, granite, quartzite, and marble\b", "i"),
                re!(r"quartz|granite|marble|quartzite", "i"),
                "stone material comparison",
            ),
            (
                re!(r"\bcabinet project\b", "i"),
                re!(r"cabinet", "i"),
                "cabinet project",
            ),
        ];
        let scope_body = re!(r"\bgranite depot\b", "gi").replace_all(body, "");
        for (claim, need, label) in claims {
            if claim.is_match(&scope_body) && !need.is_match(&customer_scope) {
                errors.push(format!(
                    "UNKNOWN scope copy asserts {label} without customer-originated evidence"
                ));
            }
        }
    }
    let has_kitchen_evidence = p
        .project_areas
        .iter()
        .any(|item| item.area == ProjectAreaKind::Kitchen)
        || re!(r"\bkitchen\b", "i").is_match(&customer_scope)
        || re!(r"\bkitchen\b", "i").is_match(text(p.known_scope.as_ref()));
    if !has_kitchen_evidence
        && re!(
            r"\byour kitchen project\b|\bfor the kitchen\b|\bworking on the kitchen\b",
            "i"
        )
        .is_match(body)
    {
        errors.push("kitchen copy without customer-originated room evidence".to_string());
    }
    if p.budget_obstacle_evidence.is_none()
        && p.decision_obstacle != DecisionObstacle::Budget
        && re!(r"fit the budget|besides budget|what besides budget|budget (is )?prevent|options that fit the budget", "i").is_match(body)
    {
        errors.push("budget obstacle copy without budget obstacle evidence".to_string());
    }
    if let (Some(backsplash), None) = (&p.backsplash_sqft, &p.countertop_sqft) {
        let value = js_number(backsplash.value);
        let used = js_regex_try(
            &format!(
                r"{value}\s*(?:sq\.?\s*ft|sqft).{{0,80}}\$\s*[\d,]+|\$\s*[\d,]+.{{0,80}}{value}\s*(?:sq\.?\s*ft|sqft)|{value}\s*[×x*]\s*\$?\d+"
            ),
            "i",
        )
        .is_some_and(|re| re.is_match(body));
        if used {
            errors.push("backsplash_sqft used as countertop price quantity".to_string());
        }
    }
    let no_commas = body.replace(',', "");
    if let Some(amount) = re!(r"(?:about |around |approximately )?\$\s*([1-9]\d{2,5})\b")
        .captures(&no_commas)
        .and_then(|caps| caps[1].parse::<f64>().ok())
        && let (Some(backsplash), None) = (&p.backsplash_sqft, &p.countertop_sqft)
        && backsplash.value > 0.0
        && amount % backsplash.value == 0.0
        && amount / backsplash.value >= 20.0
    {
        errors.push(
            "generated price calculation uses a mismatched measurement component".to_string(),
        );
    }
    if p.measurement_conflict
        && let Some(disputed) = p
            .customer_reported_area_sqft
            .or(p.customer_reported_island_sqft)
            .or(p.calculated_area_from_dimensions_sqft)
    {
        let quantity_used = js_regex_try(&format!(r"\b{}\b", js_number(disputed)), "")
            .is_some_and(|re| re.is_match(body));
        if quantity_used && re!(r"\$\s*[\d,]+|[×x*]\s*\$?\d+", "i").is_match(body) {
            errors.push("exact pricing must not use disputed measurement quantity".to_string());
        }
    }
    if let Some(found) = re!(r"\b(monday|tuesday|wednesday|thursday|friday|saturday|sunday|tomorrow|next week|next weekend)\b", "i").find(body) {
        let proposed = found.as_str().to_lowercase();
        if !js_regex_try(&format!(r"\b{proposed}\b"), "i").is_some_and(|re| re.is_match(&evidence)) {
            errors.push("generated message proposes a specific day/time without scheduling evidence".to_string());
        }
    }
    if let (Some(existing), [target]) = (
        p.existing_countertop_material.as_deref(),
        p.material_type_preferences.as_slice(),
    ) {
        let mentions = |name: &str| {
            js_regex_try(&format!(r"\b{name}\b"), "i").is_some_and(|re| re.is_match(body))
        };
        if mentions(existing)
            && mentions(target)
            && re!(r"\bor\b", "i").is_match(body)
            && re!(
                r"leaning toward|still deciding|quartz or granite|granite or quartz",
                "i"
            )
            .is_match(body)
        {
            errors.push(
                "material replacement must not ask existing material vs target material"
                    .to_string(),
            );
        }
    }
    let truly_new = p.communication_history_status == CommunicationHistoryStatus::TrulyNew;
    if truly_new
        && re!(r"kitchen", "i").is_match(text(p.known_scope.as_ref()))
        && re!(
            r"kitchen or bathroom|kitchen, bathroom|kitchen vs bathroom|bathroom, or another area|entire kitchen or just one area|entire kitchen or one area",
            "i"
        )
        .is_match(body)
    {
        errors.push("known kitchen scope must not ask kitchen vs bathroom".to_string());
    }
    let intent = p.latest_meaningful_customer_intent.as_ref().map(|i| i.kind);
    if truly_new
        && !matches!(intent, Some(I::Waiting | I::Delayed | I::NotNow))
        && p.estimated_ready_at.is_none()
        && p.hold_reason_evidence.as_deref().is_none_or(str::is_empty)
        && re!(
            r"whenever your renovation is ready|when the timing is right|has the timing changed|restart the conversation|picking up where (the conversation|we) left off|keeping (your|the) .{0,40}on my radar|if the project is still active|keeping the door open",
            "i"
        )
        .is_match(body)
    {
        errors.push("new unanswered lead must not use wait or reactivation copy".to_string());
    }
    if re!(r"\bjust reply\b", "i").is_match(body)
        || re!(
            r#"\breply(?:\s+with)?\s*[“"'‘’][^“"'‘’\n]{1,40}[“"'‘’]"#,
            "i"
        )
        .is_match(body)
        || re!(r#"\breply\s+[“"'‘’][^“"'‘’\n]+[“"'‘’]\s+or\s+[“"'‘’]"#, "i").is_match(body)
    {
        errors.push("must not use just-reply or keyword-reply CTAs".to_string());
    }
    if re!(
        r"leave the door open|keeping the door open|comes? back onto (your |the )?radar|back on (your |the )?radar|on my radar|take[s]? a back seat|fresh conversation with no assumptions|conversation with no assumptions|no assumptions be useful|restarting can be simple|one small first step|ideas or practical planning|fresh starting point|project becomes timely|whenever it makes sense",
        "i"
    )
    .is_match(body)
    {
        errors.push("must not use corporate or AI fluff phrasing".to_string());
    }
    errors
}

/// [`validate_generated_followup_copy`] plus checks that need the send date.
pub fn validate_generated_followup_copy_for_send_at(
    body: &str,
    p: &FollowupContactPressure,
    send_at: Instant,
) -> Vec<String> {
    let mut errors = validate_generated_followup_copy(body, p);
    if re!(
        r"window is here|your 10.?12 day window|10.?12 day window is here",
        "i"
    )
    .is_match(body)
        && let (Some(start), Some(end)) = (
            p.customer_requested_contact_window_start,
            p.customer_requested_contact_window_end,
        )
    {
        let end = end + Duration::milliseconds(24 * 60 * 60 * 1000 - 1);
        if send_at < start || send_at > end {
            errors
                .push("contact-window copy must match requested contact window timing".to_string());
        }
    }
    errors.extend(relative_timing_copy_errors(
        body,
        send_at,
        p.estimated_ready_at,
        p.estimated_ready_window_start,
        p.estimated_ready_window_end,
    ));
    if !timeline_copy_is_valid(
        body,
        send_at,
        p.estimated_ready_at,
        p.estimated_ready_window_start,
        p.estimated_ready_window_end,
    ) {
        errors.push("timeline copy must match where the send date falls relative to the expected project window".to_string());
    }
    errors
}
