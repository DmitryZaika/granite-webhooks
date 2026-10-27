//! Builds [`FollowupFacts`] from a customer's history, then reconciles the
//! facts that contradict each other (`extractFollowupFacts` and
//! `reconcileFollowupFacts` of `smsFlowAiFollowupFacts.ts`).

use super::super::dates::Instant;
use super::super::js::{fancy_test, js_number, trim};
use super::contact::{
    UnresolvedFactInputs, budget_semantics, collect_unresolved_facts, extract_agreed_next_step,
    extract_appointment_companion, extract_backsplash_state, extract_decision_criteria,
    extract_decision_factors, extract_explicit_customer_request,
    extract_historical_quote_expiration_warning, extract_quote_expiration,
    extract_requested_contact_method, extract_requested_contact_methods,
    extract_salesperson_planned_contact_method, extract_sink_state, infer_latest_customer_state,
    material_selection_history_re, profile_has_completed_estimate,
};
use super::manual::{
    dedupe_normalized_facts, extract_alternative_material_offered, extract_manual_action_status,
    extract_project_areas, task_references_past_event,
};
use super::materials::{
    activity_name, apply_quote_state_implications, confirmed_selection_re,
    extract_cabinet_preferences, extract_countertop_color_preferences, extract_favorite_material,
    extract_historical_materials, extract_listed_material_preferences,
    extract_material_preferences, extract_material_type_preferences,
    extract_quoted_material_context, filter_valid_material_entities, generic_material_word_re,
    infer_material_type_for_named_stone, invalid_material_entity_re, material_name_re,
    measurement_or_grammar_material_re, parse_material_replacement_relationship,
    prefer_longest_material_names, title_case_material,
};
use super::project::{
    customer_request_evidence_text, extract_conversation_material_selections,
    extract_edge_preferences, extract_future_volume_sqft, extract_measurement_conflict,
    extract_project_dependencies, extract_project_details, extract_project_scale,
    extract_typed_measurements, measurement_of_type,
};
use super::quotes::{
    alternative_interest_re, comparing_quotes_re, construction_evidence_fancy,
    extract_budget_target_approx, extract_primary_price_context, infer_quote_type,
    negative_option_re, price_rejection_re, quote_discussed_re, quote_evidence_is_verified,
    quote_price_eval_re, quote_received_fancy, quote_sent_fancy, salesperson_task_only_re,
    text_looks_like_quote_created, text_looks_like_quote_request, timeline_evidence_re,
};
use super::scope::{
    activity_looks_satisfied_by_later_text, customer_originated_text,
    extract_customer_project_scope, infer_activity_actor, infer_deal_product_scope,
    infer_project_scope_evidence, priority_scope_evidence, unique_evidence_chunks,
};
use super::timing::{
    extract_customer_requested_contact_window, extract_estimated_ready_at,
    extract_planned_customer_event, extract_time_sensitive_facts, extract_visual_style,
};
use super::types::{
    ActivityActor, BacksplashFinish, BacksplashType, ContactMethod, DealProductScope,
    DecisionFactor, DecisionObstacle, EvidenceConfidence, EvidenceSource,
    ExplicitCustomerRequestKind, ExtractedMeasurement, FollowupFacts, LatestCustomerState,
    MaterialDirectionStatus, MeasurementComponent, MeasurementsStatus, PreferredNextStep,
    PreferredNextStepSource, PriceContext, ProjectDependencyType, ProjectScale, QuoteType,
    UnfinishedNextStepKind,
};
use crate::re;

/// Quote state already worked out from the message history.
#[derive(Debug, Clone, Copy)]
#[allow(clippy::struct_excessive_bools)]
pub struct QuoteFromHistory {
    pub quote_evidence: bool,
    pub quote_requested: Option<bool>,
    pub quote_created: Option<bool>,
    pub quote_sent: bool,
    pub quote_received_or_discussed: bool,
    pub source: EvidenceSource,
    pub confidence: EvidenceConfidence,
}

/// The conversation around the profile.
#[derive(Debug, Clone, Default)]
pub struct FactOptions {
    pub latest_customer_text: Option<String>,
    pub all_customer_texts: Vec<String>,
    pub later_outbound_text: Option<String>,
    pub all_outbound_texts: Vec<String>,
    pub now: Option<Instant>,
    pub quote_from_history: Option<QuoteFromHistory>,
}

fn eq_ci(a: &str, b: &str) -> bool {
    a.to_lowercase() == b.to_lowercase()
}

fn contains_ci(list: &[String], value: &str) -> bool {
    list.iter().any(|item| eq_ci(item, value))
}

/// `firstCapture`: the first group, trimmed, if not empty.
fn first_capture(text: &str, pattern: &regex::Regex) -> Option<String> {
    let value = pattern
        .captures(text)?
        .get(1)
        .map(|m| trim(m.as_str()).to_string())?;
    (!value.is_empty()).then_some(value)
}

fn extract_confirmed_material(latest_customer: &str, profile: &str) -> Option<String> {
    let text = customer_originated_text(latest_customer, profile);
    if trim(&text).is_empty()
        || alternative_interest_re().is_match(&text)
        || !confirmed_selection_re().is_match(&text)
    {
        return None;
    }
    first_capture(&text, material_name_re())
}

fn quote_evidence_source_from_profile(
    profile: &str,
    latest_customer: &str,
    later_outbound: &str,
) -> (EvidenceSource, EvidenceConfidence) {
    use EvidenceConfidence::{High, Low};
    if re!(r"\bquote\b", "i").is_match(latest_customer)
        && !text_looks_like_quote_request(latest_customer)
    {
        return (EvidenceSource::CustomerMessage, High);
    }
    if re!(
        r"existing quote|cabinet quote|countertop quote|dekton quote|got .{0,40}quote|quote (was )?(sent|created|emailed|attached)",
        "i"
    )
    .is_match(profile)
    {
        return (EvidenceSource::SalespersonNote, High);
    }
    if re!(r"\bhave a quote\b", "i").is_match(profile) && !text_looks_like_quote_request(profile) {
        return (EvidenceSource::CrmNote, High);
    }
    if profile.split('\n').any(|line| {
        line.starts_with("Activity:")
            && re!(
                r"\b(send|sent|expiration|revised|updated|check|remind).{0,40}quote\b|\bquote.{0,24}(sent|expiration|ready|attached|emailed|expir)",
                "i"
            )
            .is_match(line)
    }) {
        return (EvidenceSource::CrmActivity, High);
    }
    if re!(r"\bDeal:.*Got a Quote\b", "i").is_match(profile) {
        return (EvidenceSource::DealRecord, High);
    }
    if re!(
        r"\bquote I sent\b|\bquote is ready\b|\battached your quote\b|\bquote should be ready\b",
        "i"
    )
    .is_match(later_outbound)
    {
        return (EvidenceSource::Conversation, High);
    }
    if re!(r"\byour quote\b|\bthe quote\b", "i").is_match(later_outbound) {
        return (EvidenceSource::Conversation, Low);
    }
    (EvidenceSource::None, Low)
}

fn is_unknown_placeholder(value: &str) -> bool {
    re!(r"^(unknown|n\/?a|-|tbd|none)$", "i").is_match(value)
}

/// The unfinished CRM task the follow-up should move forward.
#[derive(Default)]
struct UnfinishedStep {
    step: Option<String>,
    kind: Option<UnfinishedNextStepKind>,
    possibly_satisfied: bool,
    actor: Option<ActivityActor>,
    object: Option<String>,
    confidence: Option<EvidenceConfidence>,
}

fn unfinished_step_from_activities(profile: &str, later_outbound: &str) -> UnfinishedStep {
    let mut found = UnfinishedStep::default();
    for line in profile.split('\n') {
        if !line.starts_with("Activity:") || re!(r"\bcompleted\b").is_match(line) {
            continue;
        }
        let name = activity_name(line);
        if name.is_empty() {
            continue;
        }
        found.actor = Some(infer_activity_actor(&name));
        if name.to_lowercase().contains("pictures") {
            if re!(r"\bpictures of the\s*$", "i").is_match(trim(&name))
                || re!(r"\bpictures of\s*$", "i").is_match(trim(&name))
            {
                found.object = None;
                found.confidence = Some(EvidenceConfidence::Low);
            } else if re!(r"pictures of the kitchen", "i").is_match(&name) {
                found.object = Some("kitchen pictures".into());
                found.confidence = Some(EvidenceConfidence::High);
            } else if let Some(object) = re!(r"pictures of (?:the )?([a-z][a-z\s]{2,30})", "i")
                .captures(&name)
                .and_then(|caps| caps.get(1))
                .filter(|m| !m.as_str().is_empty())
            {
                found.object = Some(format!("{} pictures", trim(object.as_str())));
                found.confidence = Some(EvidenceConfidence::High);
            }
        }
        if salesperson_task_only_re().is_match(&name)
            && !re!(r"quote|black mist|pictures|prices|estimate|appointment|bundle").is_match(&name)
        {
            continue;
        }
        if re!(
            r"schedule.{0,24}(appoinment|appointment)|take a look at.{0,40}bundle|see the new bundle|see what we have",
            "i"
        )
        .is_match(&name)
        {
            found.step = Some(name);
            found.kind = Some(UnfinishedNextStepKind::ShowroomAppointment);
            break;
        }
        if re!(r"in-?home est|schedule.{0,24}estimate", "i").is_match(&name) {
            found.step = Some(name);
            found.kind = Some(UnfinishedNextStepKind::InHomeEstimate);
            break;
        }
        if re!(
            r"black mist|order|cabinet quote|quote|template|pictures|prices? are going up|find ",
            "i"
        )
        .is_match(&name)
        {
            found.possibly_satisfied =
                activity_looks_satisfied_by_later_text(&name, later_outbound);
            if !found.possibly_satisfied {
                found.step = Some(name);
                found.kind = Some(UnfinishedNextStepKind::SalespersonTask);
            }
            break;
        }
        if re!(
            r"should send .{0,30}measur|measurments|measurements|check if .{0,24}(got|has).{0,24}measur",
            "i"
        )
        .is_match(&name)
        {
            if re!(r"cabinet", "i").is_match(&name) {
                found.object = Some("kitchen cabinets".into());
            }
            found.step = Some(name);
            found.kind = Some(UnfinishedNextStepKind::CustomerMeasurements);
            break;
        }
    }
    found
}

#[allow(clippy::too_many_arguments)]
fn known_scope(
    profile: &str,
    customer_corpus: &str,
    customer_originated_scope: &str,
    activity_scope_evidence: &str,
    scope_evidence_text: &str,
) -> Option<String> {
    let project_type = first_capture(profile, re!(r"^Project type:\s*(.+)$", "m"));
    let has_customer_scope_evidence = !trim(customer_originated_scope).is_empty();
    let structured_lead_details = re!(
        r"^(Sink:|Backsplash:|Project size:|Remove and dispose:)",
        "im"
    )
    .is_match(profile)
        || re!(r"\d+\s*sqft", "im").is_match(profile);
    if let Some(project_type) = project_type.filter(|t| {
        !is_unknown_placeholder(t) && (has_customer_scope_evidence || structured_lead_details)
    }) {
        return Some(project_type);
    }
    let (scale, units) = extract_project_scale(&format!("{profile}\n{customer_corpus}"));
    if scale == ProjectScale::MultiUnit {
        return Some(units.map_or_else(
            || "multi-unit apartments".to_string(),
            |units| format!("approximately {} apartments/units", js_number(units)),
        ));
    }
    if re!(r"\bisland\b", "i").is_match(scope_evidence_text) {
        return Some("countertop/island".into());
    }
    if re!(r"\bcountertops?\b|\bcounterops\b", "i").is_match(&format!(
        "{customer_corpus}\n{}\n{activity_scope_evidence}",
        priority_scope_evidence(profile)
    )) {
        return Some("countertops".into());
    }
    if re!(r"laundry", "i").is_match(scope_evidence_text) {
        return Some("laundry".into());
    }
    if re!(r"\bkitchen\b", "i").is_match(scope_evidence_text) {
        return Some("Kitchen".into());
    }
    first_capture(
        scope_evidence_text,
        re!(r"\b(kitchen|vanity|bathroom|island)\b", "im"),
    )
}

/// Everything the engine reads from a customer's history.
// Branch order mirrors the CRM, even where two branches agree.
#[allow(
    clippy::too_many_lines,
    clippy::cognitive_complexity,
    clippy::if_same_then_else
)]
pub fn extract_followup_facts(
    profile: &str,
    quote_evidence: bool,
    options: &FactOptions,
) -> FollowupFacts {
    let now = options.now.unwrap_or_else(chrono::Utc::now);
    let latest_customer = options.latest_customer_text.as_deref().unwrap_or("");
    let later_outbound = options.later_outbound_text.as_deref().unwrap_or("");
    let all_outbound = options.all_outbound_texts.join("\n");
    let originated = customer_originated_text(latest_customer, profile);
    let customer_corpus = std::iter::once(latest_customer)
        .chain(options.all_customer_texts.iter().map(String::as_str))
        .chain(std::iter::once(originated.as_str()))
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    let profile_and_corpus = format!("{profile}\n{customer_corpus}");
    let material_replacement = parse_material_replacement_relationship(&profile_and_corpus);
    let existing_type = material_replacement
        .as_ref()
        .and_then(|r| r.existing_material_type.clone());
    let target_type = material_replacement
        .as_ref()
        .and_then(|r| r.target_material_type.clone());
    let target_color = material_replacement
        .as_ref()
        .and_then(|r| r.target_color.clone());
    let (listed_names, _) = extract_listed_material_preferences(profile);

    // Every stone named in the history, longest names first.
    let mut historical_candidates: Vec<String> = Vec::new();
    for name in extract_historical_materials(profile)
        .into_iter()
        .chain(extract_historical_materials(&customer_corpus))
        .chain(extract_conversation_material_selections(&format!(
            "{customer_corpus}\n{latest_customer}\n{all_outbound}"
        )))
        .chain(listed_names.iter().cloned())
        .map(|name| title_case_material(&name))
    {
        if name.is_empty()
            || invalid_material_entity_re().is_match(&name)
            || measurement_or_grammar_material_re().is_match(&name)
            || contains_ci(&historical_candidates, &name)
        {
            continue;
        }
        historical_candidates.push(name);
    }
    let historical_material_preferences: Vec<String> =
        prefer_longest_material_names(&historical_candidates)
            .into_iter()
            .filter(|name| {
                let lower = name.to_lowercase();
                !listed_names.iter().any(|item| {
                    let item = item.to_lowercase();
                    item != lower
                        && (item.ends_with(&format!(" {lower}"))
                            || item.starts_with(&format!("{lower} ")))
                })
            })
            .collect();

    let named_material_preferences = extract_material_preferences(&profile_and_corpus);
    let mut material_type_preferences_early =
        extract_material_type_preferences(&profile_and_corpus);
    if let Some(existing) = &existing_type {
        material_type_preferences_early.retain(|name| !eq_ci(name, existing));
    }
    if let Some(target) = &target_type
        && !contains_ci(&material_type_preferences_early, target)
    {
        material_type_preferences_early.push(target.clone());
    }
    let mut material_preference_candidates = named_material_preferences.clone();
    material_preference_candidates.extend(
        material_type_preferences_early
            .iter()
            .filter(|name| !contains_ci(&named_material_preferences, name))
            .cloned(),
    );
    material_preference_candidates.extend(
        extract_conversation_material_selections(&customer_corpus)
            .into_iter()
            .filter(|name| !contains_ci(&named_material_preferences, name)),
    );
    let mut material_preferences: Vec<String> =
        filter_valid_material_entities(&material_preference_candidates)
            .into_iter()
            .filter(|name| !contains_ci(&material_type_preferences_early, name))
            .collect();
    if let Some(existing) = &existing_type {
        material_preferences.retain(|name| !eq_ci(name, existing));
    }
    let edge_preferences = extract_edge_preferences(&profile_and_corpus);
    let favorite_material = extract_favorite_material(profile);
    let quoted_material_context = extract_quoted_material_context(&profile_and_corpus);
    let mut countertop_color_preferences =
        extract_countertop_color_preferences(&profile_and_corpus);
    if let Some(color) = &target_color
        && !contains_ci(&countertop_color_preferences, color)
    {
        countertop_color_preferences.push(color.clone());
    }
    let cabinet_preferences = extract_cabinet_preferences(&profile_and_corpus);
    let mut countertop_preferences = material_preferences.clone();
    countertop_preferences.extend(
        countertop_color_preferences
            .iter()
            .filter(|name| !contains_ci(&material_preferences, name))
            .cloned(),
    );
    let current_confirmed_material = extract_confirmed_material(latest_customer, profile);
    let known_material = current_confirmed_material.clone().or_else(|| {
        if quoted_material_context.is_some()
            && favorite_material.is_none()
            && listed_names.is_empty()
            && named_material_preferences.len() <= 1
        {
            quoted_material_context.clone()
        } else if listed_names.is_empty()
            && favorite_material.is_none()
            && named_material_preferences.len() == 1
        {
            named_material_preferences.first().cloned()
        } else {
            None
        }
    });
    let (quote_expiration_evidence, quote_expiration_date) = extract_quote_expiration(profile);
    // An expiration date that has already passed proves an earlier quote
    // existed, never that the current quote or its pricing is still valid.
    let quote_expiration_is_past = quote_expiration_date.is_some_and(|date| date < now);
    let when_start_useful = first_capture(profile, re!(r"^When they want to start:\s*(.+)$", "m"))
        .filter(|w| !is_unknown_placeholder(w));
    let original_timeline_evidence = when_start_useful.clone().or_else(|| {
        timeline_evidence_re()
            .find(profile)
            .map(|m| m.as_str().to_string())
    });
    let timeline_from_customer =
        !latest_customer.is_empty() && timeline_evidence_re().is_match(latest_customer);

    let activity_scope_evidence = profile
        .split('\n')
        .filter(|line| line.starts_with("Activity:"))
        .map(activity_name)
        .collect::<Vec<_>>()
        .join("\n");
    let profile_scope_hints = profile
        .split('\n')
        .filter(|line| {
            re!(r"^Address:", "i").is_match(line) || re!(r"^Deal note ", "i").is_match(line)
        })
        .collect::<Vec<_>>()
        .join("\n");
    let scope_evidence_text =
        format!("{originated}\n{activity_scope_evidence}\n{profile_scope_hints}");
    let known_scope = known_scope(
        profile,
        &customer_corpus,
        &originated,
        &activity_scope_evidence,
        &scope_evidence_text,
    );

    let manual_action = extract_manual_action_status(profile);
    let mut unfinished = unfinished_step_from_activities(profile, later_outbound);
    if unfinished.kind.is_none()
        && re!(
            r"schedule another time|have to schedule|schedule a (measurement|measure|time)",
            "i"
        )
        .is_match(&format!("{latest_customer}\n{customer_corpus}"))
        && re!(r"measure", "i").is_match(&format!("{later_outbound}\n{all_outbound}"))
    {
        unfinished.step = Some("Schedule measurement".into());
        unfinished.kind = Some(UnfinishedNextStepKind::ScheduleMeasurement);
    }
    // A task about an event that has already passed is history, not a next step.
    let historical_task = unfinished.step.as_deref().is_some_and(|step| {
        task_references_past_event(step, now)
            || (quote_expiration_is_past
                && re!(r"quote expir|expiration quote", "i").is_match(step))
    });
    if historical_task {
        unfinished.step = None;
        unfinished.kind = None;
    }

    let scope_text = format!("{profile}\n{later_outbound}\n{latest_customer}");
    let quote_type_scope = format!("{profile}\n{latest_customer}");
    let history = options.quote_from_history;
    let (quote_source, quote_source_confidence) = history.map_or_else(
        || quote_evidence_source_from_profile(profile, latest_customer, later_outbound),
        |h| (h.source, h.confidence),
    );
    let resolved_quote_evidence = history.map_or(quote_evidence, |h| h.quote_evidence);
    let quote_type = infer_quote_type(&quote_type_scope, resolved_quote_evidence);
    let construction_match = construction_evidence_fancy()
        .find(profile)
        .ok()
        .flatten()
        .or_else(|| {
            construction_evidence_fancy()
                .find(&profile_and_corpus)
                .ok()
                .flatten()
        })
        .map(|m| m.as_str().to_string());
    let budget_source = format!("{customer_corpus}\n{profile}");
    let package_match = re!(
        r"\$[\d,]+(?:\.\d+)?\s+(?:kitchen remodel package|special)|looking for (?:the )?\$?[\d,]+(?:\.\d+)?(?:\s|$)|(?:interested|intrested) in (?:the )?\$?[\d,]+(?:\.\d+)?(?:.{0,60}(?:package|special|kitchen))?|want'?s? to have \$\s*[\d,]+ special|kitchen remodel package|\$11,999|\$11999|\b11,999\b.{0,40}(?:package|special|kitchen)",
        "i"
    )
    .find(&profile_and_corpus)
    .map(|m| m.as_str().to_string());
    let (explicit_request, explicit_kind) = extract_explicit_customer_request(
        &unique_evidence_chunks(&[&originated, latest_customer, &customer_corpus]).join("\n"),
    );
    let quote_requested_from_explicit = matches!(
        explicit_kind,
        Some(
            ExplicitCustomerRequestKind::QuoteRequest
                | ExplicitCustomerRequestKind::EstimatePricing
                | ExplicitCustomerRequestKind::EstimateRequest
                | ExplicitCustomerRequestKind::OnsiteQuote
        )
    );
    let quote_requested = history.and_then(|h| h.quote_requested).unwrap_or_else(|| {
        text_looks_like_quote_request(&format!("{profile}\n{latest_customer}\n{customer_corpus}"))
            || quote_requested_from_explicit
    });
    let quote_sent =
        history.is_some_and(|h| h.quote_sent) || fancy_test(quote_sent_fancy(), &scope_text);
    let quote_received_or_discussed = history.is_some_and(|h| h.quote_received_or_discussed)
        || fancy_test(
            quote_received_fancy(),
            &format!("{customer_corpus}\n{profile}"),
        )
        || quote_price_eval_re().is_match(&customer_corpus)
        || quote_discussed_re()
            .is_match(&format!("{customer_corpus}\n{profile}\n{later_outbound}"));
    let quote_created = history.and_then(|h| h.quote_created).unwrap_or_else(|| {
        text_looks_like_quote_created(profile) || quote_sent || quote_received_or_discussed
    });
    let (quote_received_or_discussed, quote_sent, quote_created) =
        apply_quote_state_implications(quote_received_or_discussed, quote_sent, quote_created);

    let measurement_text = format!("{latest_customer}\n{customer_corpus}\n{profile}");
    let (project_size_raw, measurements) = extract_typed_measurements(&measurement_text);
    let conflict = extract_measurement_conflict(&measurement_text);
    let future_volume_sqft = extract_future_volume_sqft(&measurement_text);
    let derived_island_sqft = if conflict.measurement_conflict {
        None
    } else if let Some(island) = measurement_of_type(&measurements, MeasurementComponent::Island) {
        Some(island)
    } else {
        match (
            conflict.calculated_area_from_dimensions_sqft,
            conflict.measurement_dimensions_length_in,
            conflict.measurement_dimensions_width_in,
        ) {
            (Some(area), Some(length), Some(width))
                if re!(r"\bisland\b", "i").is_match(&profile_and_corpus) =>
            {
                Some(ExtractedMeasurement {
                    value: area,
                    measurement_type: MeasurementComponent::Island,
                    source: EvidenceSource::CustomerMessage,
                    source_text: conflict.measurement_raw_text.clone().unwrap_or_else(|| {
                        format!("{}in x {}in", js_number(length), js_number(width))
                    }),
                    confidence: EvidenceConfidence::High,
                })
            }
            _ => None,
        }
    };
    let project_dependencies =
        extract_project_dependencies(&format!("{profile}\n{customer_corpus}\n{latest_customer}"));
    let mut project_details = extract_project_details(&profile_and_corpus, &originated);
    let has_detail = |details: &[String], pattern: &regex::Regex| {
        details.iter().any(|item| pattern.is_match(item))
    };
    if re!(
        r"countertop project|fabricated .{0,40}countertop|looking for a fabricated",
        "i"
    )
    .is_match(&customer_corpus)
        && !has_detail(&project_details, re!(r"countertops?", "i"))
    {
        project_details.push("countertops".into());
    }
    if let Some(thickness) = re!(
        r#"(\d+(?:\.\d+)?)\s*["”]\s*x\s*(\d+(?:\.\d+)?)\s*["”]?\s*x\s*(\d+(?:\.\d+)?)\s*["”]"#,
        "i"
    )
    .captures(&format!("{latest_customer}\n{customer_corpus}"))
    .and_then(|caps| caps.get(3).map(|m| m.as_str().to_string()))
    .filter(|t| !t.is_empty())
        && !has_detail(&project_details, re!(r"thick", "i"))
    {
        project_details.push(format!("{thickness}-inch thickness"));
    }
    for measurement in &measurements {
        match measurement.measurement_type {
            MeasurementComponent::Backsplash
                if !has_detail(&project_details, re!(r"backsplash sqft", "i")) =>
            {
                project_details.push(format!("{} sqft backsplash", js_number(measurement.value)));
            }
            MeasurementComponent::Countertop
                if !has_detail(&project_details, re!(r"countertop sqft", "i")) =>
            {
                project_details.push(format!("{} sqft countertop", js_number(measurement.value)));
            }
            _ => {}
        }
    }
    if project_dependencies
        .iter()
        .any(|d| d.kind == ProjectDependencyType::CabinetLeveling)
        && !has_detail(&project_details, re!(r"not leveled", "i"))
    {
        project_details.push("cabinets nearly ready".into());
        project_details.push("cabinets not leveled".into());
    }
    if project_dependencies
        .iter()
        .any(|d| d.kind == ProjectDependencyType::FloorInstallation)
        && !has_detail(&project_details, re!(r"floor", "i"))
    {
        project_details.push("waiting for floor installation".into());
    }
    let (project_scale, approx_unit_count) = extract_project_scale(&profile_and_corpus);
    let request_evidence = customer_request_evidence_text(profile, &customer_corpus);
    let (requested_method, requested_next_step) =
        extract_requested_contact_method(&request_evidence);
    let requested_contact_methods = extract_requested_contact_methods(&request_evidence);
    let salesperson_planned_contact_method = extract_salesperson_planned_contact_method(profile);
    let appointment_companion = extract_appointment_companion(&profile_and_corpus);
    let completed_estimate_satisfied = profile_has_completed_estimate(profile);
    let budget = budget_semantics(&customer_corpus, profile);
    let price_context = extract_primary_price_context(&budget_source);
    let budget_target_approx = if matches!(
        price_context,
        PriceContext::BudgetTarget | PriceContext::BudgetMax
    ) {
        extract_budget_target_approx(&budget_source)
    } else {
        None
    };
    let budget_known = budget.known_evidence.is_some();
    let budget_constraint = budget.constraint;
    let comparing_quotes = comparing_quotes_re().is_match(&customer_corpus)
        || comparing_quotes_re().is_match(profile)
        || quote_price_eval_re().is_match(&customer_corpus);
    let active_decision_factors: Vec<DecisionFactor> = extract_decision_factors(&budget_source)
        .into_iter()
        .filter(|factor| {
            *factor != DecisionFactor::Budget
                || budget_constraint
                || budget.obstacle_evidence.is_some()
                || matches!(
                    price_context,
                    PriceContext::BudgetTarget | PriceContext::BudgetMax
                )
        })
        .collect();
    let decision_criteria = extract_decision_criteria(&format!("{customer_corpus}\n{profile}"));
    let deal_note_corpus = profile
        .split('\n')
        .filter(|line| re!(r"^Deal note ", "i").is_match(line))
        .collect::<Vec<_>>()
        .join("\n");
    let activity_names = profile
        .split('\n')
        .filter(|line| re!(r"^Activity:", "i").is_match(line))
        .map(activity_name)
        .collect::<Vec<_>>()
        .join("\n");
    let customer_state_corpus = format!(
        "{latest_customer}\n{customer_corpus}\n{}\n{deal_note_corpus}\n{activity_names}",
        priority_scope_evidence(profile)
    );
    let latest_customer_state = match infer_latest_customer_state(&customer_state_corpus) {
        LatestCustomerState::Unknown => {
            let planning = &customer_state_corpus;
            if !project_dependencies.is_empty() {
                LatestCustomerState::WaitingOnDependency
            } else if comparing_quotes {
                LatestCustomerState::Evaluating
            } else if (quote_requested
                || re!(r"showroom|ready to proceed|looking at granite|looking at quartz", "i").is_match(planning))
                && (!material_type_preferences_early.is_empty()
                    || !material_preferences.is_empty()
                    || !countertop_color_preferences.is_empty())
            {
                LatestCustomerState::Planning
            } else if re!(r"waiting on stone samples|working through design options|waiting on samples", "i").is_match(planning) {
                LatestCustomerState::Planning
            } else if quote_received_or_discussed
                && re!(r"(?:have not|haven't) .{0,40}review (?:the )?quote|not sat down to fully review", "i").is_match(planning)
            {
                LatestCustomerState::Evaluating
            } else {
                LatestCustomerState::Unknown
            }
        }
        state => state,
    };
    let sink = extract_sink_state(&profile_and_corpus);
    let backsplash = extract_backsplash_state(&profile_and_corpus);
    let estimated_ready = extract_estimated_ready_at(profile, now, &customer_corpus);
    let (contact_window_start, contact_window_end) =
        extract_customer_requested_contact_window(profile, &customer_corpus, now);
    let planned_event = extract_planned_customer_event(&profile_and_corpus, now);
    let visual_style = extract_visual_style(&profile_and_corpus);
    let showroom_interest = re!(
        r"showroom|come (in|by) the store|visit (us|the store|the showroom)",
        "i"
    )
    .is_match(&profile_and_corpus);
    let (agreed_next_step, agreed_source, agreed_confidence) =
        extract_agreed_next_step(&customer_corpus, profile, later_outbound);
    let did_not_find_suitable_option = negative_option_re().is_match(&profile_and_corpus);
    let rejected_previous_options = re!(
        r"rejected|doesn'?t like (the |those |these )?options|not (a )?fan of",
        "i"
    )
    .is_match(&profile_and_corpus);
    let does_not_like_current_selection = re!(
        r"doesn'?t like (the |her |his )?(current )?selection|doesn'?t like what (she|he|they) (saw|picked)",
        "i"
    )
    .is_match(&profile_and_corpus);
    let material_selection_history = material_selection_history_re().is_match(&format!(
        "{profile}\n{later_outbound}\n{all_outbound}\n{customer_corpus}"
    ));
    let budget_obstacle_evidence = budget.obstacle_evidence.clone();
    let material_fit_evidence = did_not_find_suitable_option.then(|| {
        negative_option_re().find(&profile_and_corpus).map_or_else(
            || "did not find a suitable option".to_string(),
            |m| m.as_str().to_string(),
        )
    });
    let price_rejection_evidence = price_rejection_re()
        .find(&customer_corpus)
        .map(|m| m.as_str().to_string());
    let decision_obstacle = if material_fit_evidence.is_some() {
        DecisionObstacle::MaterialFit
    } else if budget_obstacle_evidence.is_some() {
        DecisionObstacle::Budget
    } else if price_rejection_evidence.is_some() {
        DecisionObstacle::Price
    } else if comparing_quotes {
        DecisionObstacle::PriceComparison
    } else if construction_match.is_some() || !project_dependencies.is_empty() {
        DecisionObstacle::ProjectDependency
    } else {
        DecisionObstacle::None
    };
    let decision_obstacle_evidence = material_fit_evidence
        .or_else(|| budget_obstacle_evidence.clone())
        .or(price_rejection_evidence)
        .or_else(|| construction_match.clone())
        .or_else(|| project_dependencies.first().map(|d| d.evidence.clone()));
    let preferred_material = (current_confirmed_material.is_none()
        && re!(r"taj mahal", "i").is_match(profile)
        && re!(r"appointment|bundle|wants to see", "i").is_match(profile))
    .then(|| "Taj Mahal".to_string());
    let mut material_type_preferences = material_type_preferences_early.clone();
    add_implied_material_types(&material_preferences, &mut material_type_preferences);
    if let Some(existing) = &existing_type {
        material_type_preferences.retain(|item| !eq_ci(item, existing));
    }
    material_preferences.retain(|name| !contains_ci(&material_type_preferences, name));
    let named_stone_options: Vec<&String> = material_preferences
        .iter()
        .filter(|name| {
            !contains_ci(&material_type_preferences, name)
                && !contains_ci(&countertop_color_preferences, name)
                && !generic_material_word_re().is_match(name)
        })
        .collect();
    let named_stone_competes_with_parent_type = named_stone_options.len() == 1
        && material_type_preferences.len() == 1
        && infer_material_type_for_named_stone(named_stone_options[0])
            .is_some_and(|implied| eq_ci(implied, &material_type_preferences[0]));
    let named_stone_competes_with_explicit_type = !named_stone_options.is_empty()
        && !material_type_preferences.is_empty()
        && !named_stone_competes_with_parent_type;
    let either_or_re = re!(
        r"quartz or (real )?stone|real stone or quartz|decide if i want quartz|trying to decide if i want",
        "i"
    );
    let multiple_open_materials = named_stone_options.len() > 1
        || material_type_preferences.len() > 1
        || listed_names.len() > 1
        || named_stone_competes_with_explicit_type
        || (material_preferences.len() > 1
            && material_preferences
                .iter()
                .any(|name| !contains_ci(&material_type_preferences, name))
            && !material_type_preferences.is_empty()
            && !named_stone_competes_with_parent_type)
        || either_or_re.is_match(&profile_and_corpus);
    let replacement_direction = existing_type.is_some() && target_type.is_some();
    let uncertain_material_direction = !replacement_direction
        && (re!(r"not entirely sure|not sure (what|which)? ?(material|stone|color|direction)?", "i").is_match(profile)
            || re!(
                r"undecided|not decided|still deciding|still comparing|haven't decided|have not decided",
                "i"
            )
            .is_match(&profile_and_corpus)
            || multiple_open_materials
            || re!(r"quartz or soap", "i").is_match(&profile_and_corpus)
            || either_or_re.is_match(&profile_and_corpus)
            || (re!(r"or other pretty neutral|or a soapstone|\bor\b", "i").is_match(&profile_and_corpus)
                && !re!(r"chang(?:ing|e)\s+(?:my\s+)?(?:current\s+)?\w+\s+(?:countertops?\s+)?to", "i")
                    .is_match(&profile_and_corpus)));
    let single_direction_unconfirmed = current_confirmed_material.is_none()
        && !multiple_open_materials
        && (replacement_direction
            || material_type_preferences.len() == 1
            || material_preferences.len() == 1
            || (material_type_preferences.len() == 1 && countertop_color_preferences.len() <= 1));
    let material_direction_status = if current_confirmed_material.is_some() {
        MaterialDirectionStatus::Confirmed
    } else if multiple_open_materials
        || (uncertain_material_direction && material_preferences.len() > 1)
    {
        MaterialDirectionStatus::MultipleOpenOptions
    } else if uncertain_material_direction || single_direction_unconfirmed {
        MaterialDirectionStatus::SingleOpenOption
    } else {
        MaterialDirectionStatus::None
    };
    let active_unresolved_facts = collect_unresolved_facts(&UnresolvedFactInputs {
        latest_customer,
        all_customer_text: &profile_and_corpus,
        unfinished_next_step_kind: unfinished.kind,
        unfinished_next_step: unfinished.step.as_deref(),
        budget_obstacle_evidence: budget_obstacle_evidence.as_deref(),
        budget_target_approx,
        comparing_quotes,
        active_decision_factors: &active_decision_factors,
        decision_obstacle_evidence: decision_obstacle_evidence.as_deref(),
        did_not_find_suitable_option,
        countertop_color_preferences: &countertop_color_preferences,
        material_type_preferences: &material_type_preferences,
    });
    let project_scope_parts = extract_customer_project_scope(&profile_and_corpus, later_outbound);
    let historical_quote_expiration_warning =
        extract_historical_quote_expiration_warning(profile) || quote_expiration_is_past;
    let alternative_material_offered = extract_alternative_material_offered(&format!(
        "{later_outbound}\n{all_outbound}\n{profile}"
    ));
    let installation_requested = re!(r"\binstalled\b", "i").is_match(&profile_and_corpus);
    let removal_required = has_detail(&project_details, re!(r"removal required|removal", "i"));
    let removal_component = if has_detail(
        &project_details,
        re!(r"existing countertop removal|existing granite removal", "i"),
    ) {
        Some(MeasurementComponent::Countertop)
    } else if removal_required {
        Some(MeasurementComponent::Unknown)
    } else {
        None
    };
    let material_selection_evidence_source = if !material_selection_history {
        EvidenceSource::None
    } else if re!(r"stones you picked out|picked out", "i")
        .is_match(&format!("{later_outbound}\n{all_outbound}"))
    {
        EvidenceSource::SalespersonEmail
    } else {
        EvidenceSource::SalespersonNote
    };
    let known_scope_text = known_scope.clone().unwrap_or_default();
    let deal_scope_base = format!(
        "{}\n{customer_corpus}\n{known_scope_text}\n{}",
        priority_scope_evidence(profile),
        material_preferences.join(" ")
    );
    let deal_scope_outbound = if !later_outbound.is_empty()
        && !re!(r"save \d+%|kitchen remodel package|promo|this august", "i")
            .is_match(later_outbound)
    {
        format!("\n{later_outbound}")
    } else {
        String::new()
    };
    let mut deal_product_scope = infer_deal_product_scope(
        &format!("{deal_scope_base}{deal_scope_outbound}"),
        quote_type,
    );
    let corpus_and_scope = format!("{customer_corpus}\n{known_scope_text}");
    if resolved_quote_evidence
        && re!(r"\bcabinets?\b", "i").is_match(&corpus_and_scope)
        && re!(r"\bcountertops?\b", "i").is_match(&corpus_and_scope)
    {
        deal_product_scope = DealProductScope::CabinetsAndCountertops;
    }
    let (project_scope_source, project_scope_confidence) =
        infer_project_scope_evidence(&profile_and_corpus, quote_type);
    let construction_or_dependency =
        construction_match.is_some() || !project_dependencies.is_empty();
    let agreed = agreed_next_step != PreferredNextStep::None;
    let existing_countertop_removal = has_detail(
        &project_details,
        re!(
            r"existing granite removal|existing countertop removal|remove .{0,24}granite",
            "i"
        ),
    );
    let countertop_measurements_status = if (conflict.measurement_dimensions_length_in.is_some()
        && conflict.measurement_dimensions_width_in.is_some())
        || has_detail(&project_details, re!(r"measurements provided", "i"))
    {
        Some(MeasurementsStatus::Provided)
    } else if unfinished.kind == Some(UnfinishedNextStepKind::CustomerMeasurements)
        || has_detail(&project_details, re!(r"measurements needed", "i"))
    {
        Some(MeasurementsStatus::Needed)
    } else {
        None
    };
    let facts = FollowupFacts {
        quote_type,
        quote_requested,
        quote_evidence: resolved_quote_evidence,
        quote_created,
        quote_sent,
        quote_received_or_discussed,
        quote_evidence_source: if resolved_quote_evidence {
            quote_source
        } else {
            EvidenceSource::None
        },
        quote_evidence_confidence: if resolved_quote_evidence {
            quote_source_confidence
        } else {
            EvidenceConfidence::Low
        },
        explicit_customer_request: explicit_request,
        explicit_customer_request_kind: explicit_kind,
        needs_sink: sink.needs_sink_from_company,
        sink_required: sink.sink_required,
        sink_source: sink.sink_source,
        needs_sink_from_company: sink.needs_sink_from_company,
        backsplash_known: backsplash.known,
        backsplash_type: backsplash.kind,
        stone_backsplash_required: backsplash.stone_backsplash_required,
        backsplash_finish: backsplash.backsplash_finish,
        backsplash_material: backsplash.backsplash_material,
        backsplash_description: backsplash.backsplash_description,
        uncertain_material_direction,
        material_direction_status,
        preferred_material,
        did_not_find_suitable_option,
        rejected_previous_options,
        does_not_like_current_selection,
        quote_expiration_evidence: if quote_expiration_is_past {
            None
        } else {
            quote_expiration_evidence
        },
        quote_expiration_date: if quote_expiration_is_past {
            None
        } else {
            quote_expiration_date
        },
        known_material,
        quoted_material_context,
        historical_material_preferences,
        material_preferences,
        favorite_material,
        countertop_preferences,
        cabinet_preferences,
        current_confirmed_material,
        material_selection_history,
        material_selection_evidence: material_selection_history,
        material_selection_evidence_source,
        original_timeline_evidence: if estimated_ready.at.is_some() {
            Some(
                original_timeline_evidence
                    .clone()
                    .unwrap_or_else(|| "customer project timeline".into()),
            )
        } else {
            original_timeline_evidence.clone()
        },
        timeline_evidence_source: if estimated_ready.at.is_some() {
            estimated_ready.source
        } else if original_timeline_evidence.is_none() {
            EvidenceSource::None
        } else if timeline_from_customer {
            EvidenceSource::CustomerMessage
        } else if when_start_useful.is_some() {
            EvidenceSource::DealRecord
        } else {
            EvidenceSource::SalespersonNote
        },
        timeline_evidence_confidence: if estimated_ready.at.is_some() {
            estimated_ready.confidence
        } else if original_timeline_evidence.is_none() {
            EvidenceConfidence::Low
        } else if timeline_from_customer {
            EvidenceConfidence::High
        } else {
            EvidenceConfidence::Medium
        },
        known_scope,
        project_scope_source,
        project_scope_confidence,
        unfinished_next_step: unfinished.step,
        unfinished_next_step_kind: unfinished.kind,
        unfinished_possibly_satisfied: unfinished.possibly_satisfied,
        construction_dependency_evidence: project_dependencies
            .first()
            .map(|d| d.evidence.clone())
            .or_else(|| construction_match.clone()),
        construction_delay_source: if !construction_or_dependency {
            EvidenceSource::None
        } else if re!(r"customer message:|almost ready for counter", "i")
            .is_match(&format!("{profile}\n{latest_customer}"))
        {
            EvidenceSource::CustomerMessage
        } else {
            EvidenceSource::SalespersonNote
        },
        construction_delay_confidence: if construction_or_dependency {
            EvidenceConfidence::Medium
        } else {
            EvidenceConfidence::Low
        },
        budget_known,
        budget_constraint,
        budget_evidence: budget_known
            || budget_obstacle_evidence.is_some()
            || budget_target_approx.is_some(),
        budget_obstacle_evidence,
        budget_evidence_source: budget.source,
        budget_evidence_confidence: budget.confidence,
        financial_delay_source: budget.source,
        financial_delay_confidence: budget.confidence,
        deal_product_scope,
        activity_actor: unfinished.actor,
        activity_object: unfinished.object,
        activity_confidence: unfinished.confidence,
        manual_action_required: manual_action.required,
        manual_action_description: manual_action.description,
        manual_action_tasks: manual_action.tasks,
        time_sensitive_facts: extract_time_sensitive_facts(
            &format!("{profile}\n{later_outbound}"),
            now,
        ),
        known_package_interest: package_match,
        decision_obstacle_evidence,
        decision_criteria,
        decision_obstacle,
        project_areas: extract_project_areas(&profile_and_corpus),
        existing_countertop_removal,
        removal_required,
        removal_component,
        project_details,
        budget_target_approx,
        comparing_quotes,
        active_decision_factors,
        latest_customer_state,
        active_unresolved_facts,
        existing_countertop_material: existing_type,
        customer_project_components: project_scope_parts.customer_project_components,
        unsupported_requested_components: project_scope_parts.unsupported_requested_components,
        historical_quote_expiration_warning,
        historical_task,
        alternative_material_offered,
        installation_requested,
        price_context,
        material_type_preferences,
        countertop_color_preferences,
        estimated_ready_at: estimated_ready.at,
        estimated_ready_at_source: estimated_ready.source,
        estimated_ready_at_confidence: estimated_ready.confidence,
        estimated_ready_at_precision: estimated_ready.precision,
        estimated_ready_window_start: estimated_ready.window_start,
        estimated_ready_window_end: estimated_ready.window_end,
        project_scale,
        approx_unit_count,
        preferred_next_step: if agreed {
            agreed_next_step
        } else {
            requested_next_step
        },
        preferred_next_step_source: if agreed {
            agreed_source
        } else {
            PreferredNextStepSource::None
        },
        preferred_next_step_confidence: if agreed {
            agreed_confidence
        } else {
            EvidenceConfidence::Low
        },
        customer_requested_contact_method: if requested_contact_methods
            .contains(&ContactMethod::Sms)
        {
            Some(ContactMethod::Sms)
        } else if requested_contact_methods.contains(&ContactMethod::Email) {
            Some(ContactMethod::Email)
        } else {
            requested_method
        },
        customer_requested_contact_methods: requested_contact_methods,
        customer_resume_condition: None,
        edge_preferences,
        salesperson_planned_contact_method,
        appointment_companion,
        completed_estimate_satisfied,
        customer_requested_contact_window_start: contact_window_start,
        customer_requested_contact_window_end: contact_window_end,
        planned_customer_event_type: planned_event.kind,
        planned_customer_event_window_start: planned_event.window_start,
        planned_customer_event_window_end: planned_event.window_end,
        planned_customer_event_confidence: planned_event.confidence,
        planned_customer_event_date_label: planned_event.date_label,
        visual_style,
        showroom_interest,
        countertop_measurements_status,
        project_size_raw,
        countertop_sqft: measurement_of_type(&measurements, MeasurementComponent::Countertop),
        backsplash_sqft: measurement_of_type(&measurements, MeasurementComponent::Backsplash),
        island_sqft: derived_island_sqft,
        vanity_sqft: measurement_of_type(&measurements, MeasurementComponent::Vanity),
        total_project_sqft: measurement_of_type(&measurements, MeasurementComponent::TotalProject),
        unknown_sqft: if conflict.measurement_conflict {
            None
        } else {
            measurement_of_type(&measurements, MeasurementComponent::Unknown)
        },
        measurement_conflict: conflict.measurement_conflict,
        measurement_dimensions_length_in: conflict.measurement_dimensions_length_in,
        measurement_dimensions_width_in: conflict.measurement_dimensions_width_in,
        calculated_area_from_dimensions_sqft: conflict.calculated_area_from_dimensions_sqft,
        customer_reported_area_sqft: conflict.customer_reported_area_sqft,
        customer_reported_island_sqft: conflict.customer_reported_island_sqft,
        measurement_raw_text: conflict.measurement_raw_text,
        future_volume_sqft,
        project_dependencies,
    };
    reconcile_followup_facts(facts)
}

/// Named stones imply their type (Taj Mahal is a quartzite) when no named
/// product stone is among the preferences.
fn add_implied_material_types(
    material_preferences: &[String],
    material_type_preferences: &mut Vec<String>,
) {
    let has_named_product = material_preferences.iter().any(|name| {
        infer_material_type_for_named_stone(name).is_some()
            && !generic_material_word_re().is_match(name)
    });
    if has_named_product {
        return;
    }
    for name in material_preferences {
        if let Some(implied) = infer_material_type_for_named_stone(name)
            && !contains_ci(material_type_preferences, implied)
        {
            material_type_preferences.push(implied.to_string());
        }
    }
}

/// Resolves facts that contradict each other (a package price is not a
/// budget, an undecided customer has no known material...).
#[allow(
    clippy::too_many_lines,
    clippy::cognitive_complexity,
    clippy::if_same_then_else
)]
pub fn reconcile_followup_facts(mut facts: FollowupFacts) -> FollowupFacts {
    let mut project_details = dedupe_normalized_facts(&facts.project_details);
    let joined_details = |details: &[String]| details.join(" ");
    if project_details
        .iter()
        .any(|item| re!(r"no backsplash", "i").is_match(item))
        && facts.backsplash_finish != BacksplashFinish::Tile
    {
        facts.backsplash_known = true;
        facts.backsplash_type = BacksplashType::None;
    }
    let components = &facts.customer_project_components;
    let has_component = |name: &str| components.iter().any(|item| item == name);
    let known_scope = facts.known_scope.clone().unwrap_or_default();
    let explicit_request = facts.explicit_customer_request.clone().unwrap_or_default();
    let package_interest_text = facts.known_package_interest.clone().unwrap_or_default();
    let package_remodel_evidence = facts.known_package_interest.is_some()
        && (re!(r"(?:11,999|11999|kitchen remodel|remodel package)", "i")
            .is_match(&package_interest_text)
            || has_component("kitchen remodel")
            || re!(r"redo .{0,12}kitchen|kitchen remodel package", "i")
                .is_match(&format!("{explicit_request} {known_scope}")));
    let mut scope = facts.deal_product_scope;
    let details_text = joined_details(&project_details);
    if package_remodel_evidence {
        scope = DealProductScope::FullRemodel;
    } else if facts
        .known_package_interest
        .as_deref()
        .is_some_and(|p| !p.is_empty() && re!(r"remodel package", "i").is_match(p))
        && (facts.explicit_customer_request.is_some()
            || facts.project_scope_source != EvidenceSource::None
            || has_component("kitchen remodel"))
    {
        scope = DealProductScope::FullRemodel;
    } else if re!(r"wants to get the kitchen cabinets|kitchen cabinets", "i")
        .is_match(facts.unfinished_next_step.as_deref().unwrap_or(""))
    {
        scope = DealProductScope::Cabinets;
    } else if has_component("cabinets")
        && (has_component("countertops") || has_component("kitchen remodel"))
    {
        scope = DealProductScope::CabinetsAndCountertops;
    } else if scope == DealProductScope::Unknown && has_component("kitchen remodel") {
        scope = DealProductScope::FullRemodel;
    } else if re!(r"cabinetr", "i").is_match(&known_scope) {
        if scope != DealProductScope::CabinetsAndCountertops {
            scope = DealProductScope::Cabinets;
        }
    } else if !facts.cabinet_preferences.is_empty()
        && (!facts.countertop_preferences.is_empty()
            || !facts.material_preferences.is_empty()
            || !facts.material_type_preferences.is_empty())
    {
        scope = DealProductScope::CabinetsAndCountertops;
    } else if scope == DealProductScope::Unknown
        && (!facts.material_preferences.is_empty()
            || !facts.material_type_preferences.is_empty()
            || (re!(r"island|countertop", "i").is_match(&details_text)
                && !re!(r"countertops? removal|granite removal", "i").is_match(&details_text))
            || re!(r"countertop|island", "i").is_match(&known_scope))
        && facts.project_scale != ProjectScale::MultiUnit
        && !re!(r"apartment|multi-unit|cabinetr", "i").is_match(&known_scope)
        && facts
            .known_package_interest
            .as_deref()
            .is_none_or(str::is_empty)
    {
        scope = DealProductScope::Countertops;
    }
    if facts
        .known_package_interest
        .as_deref()
        .is_some_and(|p| !p.is_empty())
        && scope == DealProductScope::Countertops
        && (package_remodel_evidence
            || has_component("kitchen remodel")
            || has_component("cabinets"))
    {
        scope = DealProductScope::FullRemodel;
    }
    if facts.quote_type == Some(QuoteType::Cabinet)
        && scope == DealProductScope::CabinetsAndCountertops
        && !facts.existing_countertop_removal
        && facts.countertop_preferences.is_empty()
        && !re!(r"countertop|island", "i").is_match(&details_text)
        && !re!(r"countertop|island", "i").is_match(&known_scope)
        && !re!(r"countertop project|fabricated .{0,40}countertop", "i")
            .is_match(&format!("{known_scope} {details_text} {explicit_request}"))
        && !(facts.quote_evidence && facts.quote_created)
    {
        scope = DealProductScope::Cabinets;
    }
    facts.deal_product_scope = scope;
    if project_details
        .iter()
        .any(|item| re!(r"4-inch backsplash", "i").is_match(item))
    {
        facts.backsplash_known = true;
        facts.backsplash_type = BacksplashType::FourInch;
    }
    if facts.budget_target_approx.is_some()
        && (!facts.budget_evidence
            || facts.budget_evidence_source == EvidenceSource::None
            || facts.budget_evidence_confidence == EvidenceConfidence::Low)
    {
        facts.budget_target_approx = None;
    }
    if matches!(
        facts.price_context,
        PriceContext::PackagePrice | PriceContext::PromotionPrice
    ) {
        facts.budget_target_approx = None;
        facts.budget_evidence = false;
    }
    if facts
        .known_package_interest
        .as_deref()
        .is_some_and(|p| !p.is_empty())
        && !matches!(
            facts.price_context,
            PriceContext::BudgetTarget | PriceContext::BudgetMax
        )
    {
        facts.price_context = PriceContext::PackagePrice;
        facts.budget_target_approx = None;
        facts.budget_evidence = false;
    }
    if let Some(package) = facts
        .known_package_interest
        .clone()
        .filter(|p| !p.is_empty())
        && re!(r"(?:11,999|11999)", "i").is_match(&package)
        && re!(r"kitchen|remodel|package", "i").is_match(&format!(
            "{package} {explicit_request} {}",
            facts.customer_project_components.join(" ")
        ))
    {
        facts.known_package_interest = Some("$11,999 kitchen remodel package".into());
    }
    let confirmed = facts
        .current_confirmed_material
        .clone()
        .filter(|m| !m.is_empty());
    let original_material_count = facts.material_preferences.len();
    let mut material_preferences: Vec<String> =
        filter_valid_material_entities(&facts.material_preferences)
            .into_iter()
            .filter(|name| !contains_ci(&facts.material_type_preferences, name))
            .collect();
    let mut material_type_preferences = facts.material_type_preferences.clone();
    add_implied_material_types(&material_preferences, &mut material_type_preferences);
    if let Some(existing) = facts
        .existing_countertop_material
        .as_deref()
        .filter(|e| !e.is_empty())
    {
        material_type_preferences.retain(|item| !eq_ci(item, existing));
    }
    material_preferences.retain(|name| !contains_ci(&material_type_preferences, name));
    if material_preferences
        .iter()
        .any(|name| invalid_material_entity_re().is_match(name))
    {
        material_preferences.retain(|name| !invalid_material_entity_re().is_match(name));
        facts.known_material = None;
    }
    if confirmed.is_none() && original_material_count > 1 {
        facts.known_material = None;
    }
    if facts.uncertain_material_direction && confirmed.is_none() {
        facts.known_material = None;
        facts.current_confirmed_material = None;
    }
    if confirmed.is_some() && !facts.uncertain_material_direction {
        facts
            .known_material
            .clone_from(&facts.current_confirmed_material);
    }
    if let (Some(quoted), Some(favorite)) =
        (&facts.quoted_material_context, &facts.favorite_material)
        && !quoted.is_empty()
        && confirmed.is_none()
        && eq_ci(favorite, quoted)
        && original_material_count == 0
    {
        facts.favorite_material = None;
    }
    let add_detail = |details: &mut Vec<String>, value: &str| {
        if !contains_ci(details, value) {
            details.push(value.to_string());
        }
    };
    let mut needs_sink = facts.needs_sink_from_company;
    if facts.sink_source == Some(super::types::SinkSource::CustomerExternal) {
        needs_sink = false;
        add_detail(&mut project_details, "customer sourcing sink elsewhere");
    }
    if !re!(r"granite removal", "i").is_match(&joined_details(&project_details))
        && !known_scope.is_empty()
        && re!(r"kitchen", "i").is_match(&known_scope)
        && re!(r"granite", "i").is_match(&known_scope)
        && re!(
            r"remove.{0,24}(granite|countertops?)|granite removal|countertop removal",
            "i"
        )
        .is_match(&format!(
            "{known_scope} {}",
            joined_details(&project_details)
        ))
    {
        add_detail(&mut project_details, "existing granite removal");
    }
    facts.existing_countertop_removal = facts.existing_countertop_removal
        || project_details.iter().any(|item| {
            re!(
                r"granite removal|countertop removal|remove .{0,24}granite",
                "i"
            )
            .is_match(item)
        });
    facts.project_details = project_details;
    facts.material_preferences = material_preferences;
    facts.material_type_preferences = material_type_preferences;
    facts.needs_sink = needs_sink;
    facts.needs_sink_from_company = needs_sink;
    let budget_target = facts.budget_target_approx;
    facts.active_unresolved_facts.retain(|item| {
        budget_target.is_some() || !re!(r"budget target around", "i").is_match(item)
    });
    facts
}

/// Whether the facts back a follow-up angle (no quote check-in without a quote).
#[allow(clippy::too_many_lines)]
pub fn angle_has_required_evidence(angle: &str, facts: &FollowupFacts) -> bool {
    let verified = quote_evidence_is_verified(
        facts.quote_evidence,
        facts.quote_evidence_source,
        facts.quote_evidence_confidence,
    );
    match angle {
        "original_timeline" => return facts.original_timeline_evidence.is_some(),
        "quote_status" | "quote_review" => {
            return facts.latest_customer_state != LatestCustomerState::TemplateApproved
                && verified;
        }
        "comparison_support" => {
            return facts
                .decision_obstacle_evidence
                .as_deref()
                .is_some_and(|e| !e.is_empty())
                || facts.comparing_quotes;
        }
        "estimate_offer" => {
            use UnfinishedNextStepKind as K;
            if matches!(
                facts.unfinished_next_step_kind,
                Some(K::InHomeEstimate | K::ScheduleMeasurement | K::CustomerMeasurements)
            ) || facts.countertop_measurements_status == Some(MeasurementsStatus::Needed)
                || facts
                    .active_unresolved_facts
                    .iter()
                    .any(|item| re!(r"measurement|dimension|square footage", "i").is_match(item))
                || facts.explicit_customer_request_kind
                    == Some(ExplicitCustomerRequestKind::PricePerSqft)
            {
                return true;
            }
            if facts.completed_estimate_satisfied && facts.quote_evidence {
                return false;
            }
            return matches!(
                facts.explicit_customer_request_kind,
                Some(
                    ExplicitCustomerRequestKind::EstimatePricing
                        | ExplicitCustomerRequestKind::SquareFootage
                        | ExplicitCustomerRequestKind::OnsiteQuote
                )
            ) || facts.quote_requested;
        }
        "material_preference" => {
            if facts.latest_customer_state == LatestCustomerState::TemplateApproved {
                return false;
            }
            return facts.known_material.is_some()
                || !facts.material_preferences.is_empty()
                || facts.favorite_material.is_some()
                || !facts.material_type_preferences.is_empty();
        }
        "project_scope" => {
            return facts.deal_product_scope == DealProductScope::Unknown
                && facts.known_scope.is_none()
                && !verified
                && !(facts.quote_evidence
                    && facts.quote_evidence_source == EvidenceSource::CrmActivity)
                && facts.known_material.as_deref().is_none_or(str::is_empty)
                && facts.material_preferences.is_empty()
                && facts
                    .decision_obstacle_evidence
                    .as_deref()
                    .is_none_or(str::is_empty)
                && facts
                    .explicit_customer_request
                    .as_deref()
                    .is_none_or(str::is_empty)
                && !facts.quote_requested;
        }
        "project_dependency" => {
            return facts.construction_dependency_evidence.is_some()
                || !facts.project_dependencies.is_empty();
        }
        "budget_obstacle" => {
            return facts.budget_obstacle_evidence.is_some()
                || facts.budget_target_approx.is_some();
        }
        "decision_obstacle" => {
            return facts.decision_obstacle_evidence.is_some()
                || facts.comparing_quotes
                || !facts.active_decision_factors.is_empty();
        }
        "package_interest" => return facts.known_package_interest.is_some(),
        "previous_stone_selection" => {
            return facts.material_selection_history && facts.material_preferences.is_empty();
        }
        "helpful_information" => {
            let filled = |value: &Option<String>| value.as_deref().is_some_and(|v| !v.is_empty());
            if (facts.deal_product_scope == DealProductScope::Unknown
                && !filled(&facts.known_scope))
                || filled(&facts.construction_dependency_evidence)
                || !facts.project_dependencies.is_empty()
                || facts.deal_product_scope == DealProductScope::Cabinets
                || filled(&facts.explicit_customer_request)
                || facts.quote_requested
                || (filled(&facts.known_scope) && (facts.needs_sink || facts.backsplash_known))
                || filled(&facts.known_material)
                || !facts.material_preferences.is_empty()
                || facts.material_selection_history
                || filled(&facts.known_package_interest)
                || !facts.cabinet_preferences.is_empty()
                || !facts.material_type_preferences.is_empty()
                || filled(&facts.budget_obstacle_evidence)
                || facts.comparing_quotes
                || facts
                    .active_decision_factors
                    .contains(&DecisionFactor::Budget)
                || facts
                    .active_decision_factors
                    .contains(&DecisionFactor::PriceComparison)
            {
                return false;
            }
        }
        "design_help" => {
            let filled_scope = facts.known_scope.as_deref().is_some_and(|v| !v.is_empty());
            if (facts.deal_product_scope == DealProductScope::Unknown && !filled_scope)
                || facts.deal_product_scope == DealProductScope::Cabinets
                || facts
                    .project_dependencies
                    .iter()
                    .any(|d| d.kind == ProjectDependencyType::CabinetLeveling)
            {
                return false;
            }
        }
        _ => {}
    }
    true
}

/// How specific an angle is for these facts (higher wins; -1 is unusable).
pub fn angle_specificity_score(angle: &str, facts: &FollowupFacts) -> i32 {
    if !angle_has_required_evidence(angle, facts) {
        return -1;
    }
    let filled = |value: &Option<String>| value.as_deref().is_some_and(|v| !v.is_empty());
    let kind = facts.unfinished_next_step_kind;
    if kind == Some(UnfinishedNextStepKind::CustomerRequest)
        && matches!(angle, "project_status" | "preferred_next_step")
    {
        return 100;
    }
    match angle {
        "project_dependency" if filled(&facts.construction_dependency_evidence) => return 108,
        "estimate_offer" if kind == Some(UnfinishedNextStepKind::ScheduleMeasurement) => {
            return 107;
        }
        "estimate_offer" if kind == Some(UnfinishedNextStepKind::InHomeEstimate) => return 109,
        "estimate_offer" if filled(&facts.explicit_customer_request) => return 110,
        "package_interest" if filled(&facts.known_package_interest) => return 98,
        "budget_obstacle" if filled(&facts.budget_obstacle_evidence) => return 96,
        "comparison_support" if facts.comparing_quotes => return 94,
        "original_timeline" => return 95,
        "previous_stone_selection" if facts.material_selection_history => return 88,
        "material_preference"
            if filled(&facts.favorite_material)
                || filled(&facts.known_material)
                || !facts.material_preferences.is_empty() =>
        {
            return 90;
        }
        "design_help" if facts.needs_sink || facts.backsplash_known => return 86,
        _ => {}
    }
    match angle {
        "comparison_support" => 85,
        "quote_status" | "quote_review" => 80,
        "hold_reactivation" => 75,
        "decision_obstacle" => 72,
        "preferred_next_step" | "channel_preference" => 70,
        "project_dependency" => 65,
        "budget_obstacle" => 60,
        "project_status" | "project_timing" => 45,
        "project_scope" => 40,
        "helpful_information" | "design_help" => {
            if filled(&facts.known_material) || !facts.countertop_preferences.is_empty() {
                50
            } else {
                25
            }
        }
        "reactivation" => 20,
        "close_loop" => 15,
        "long_term_nurture" => 10,
        _ => 25,
    }
}

/// Facts with nothing known.
#[allow(clippy::too_many_lines)]
pub const fn empty_followup_facts() -> FollowupFacts {
    FollowupFacts {
        quote_type: None,
        quote_requested: false,
        quote_evidence: false,
        quote_created: false,
        quote_sent: false,
        quote_received_or_discussed: false,
        quote_evidence_source: EvidenceSource::None,
        quote_evidence_confidence: EvidenceConfidence::Low,
        quote_expiration_evidence: None,
        quote_expiration_date: None,
        explicit_customer_request: None,
        explicit_customer_request_kind: None,
        needs_sink: false,
        sink_required: false,
        sink_source: None,
        needs_sink_from_company: false,
        backsplash_known: false,
        backsplash_type: BacksplashType::Unknown,
        stone_backsplash_required: None,
        backsplash_finish: BacksplashFinish::Unknown,
        backsplash_material: None,
        backsplash_description: None,
        uncertain_material_direction: false,
        material_direction_status: MaterialDirectionStatus::None,
        preferred_material: None,
        did_not_find_suitable_option: false,
        rejected_previous_options: false,
        does_not_like_current_selection: false,
        known_material: None,
        quoted_material_context: None,
        historical_material_preferences: Vec::new(),
        material_preferences: Vec::new(),
        favorite_material: None,
        countertop_preferences: Vec::new(),
        cabinet_preferences: Vec::new(),
        current_confirmed_material: None,
        material_selection_history: false,
        material_selection_evidence: false,
        material_selection_evidence_source: EvidenceSource::None,
        original_timeline_evidence: None,
        timeline_evidence_source: EvidenceSource::None,
        timeline_evidence_confidence: EvidenceConfidence::Low,
        known_scope: None,
        project_scope_source: EvidenceSource::None,
        project_scope_confidence: EvidenceConfidence::Low,
        unfinished_next_step: None,
        unfinished_next_step_kind: None,
        unfinished_possibly_satisfied: false,
        construction_dependency_evidence: None,
        construction_delay_source: EvidenceSource::None,
        construction_delay_confidence: EvidenceConfidence::Low,
        budget_known: false,
        budget_constraint: false,
        budget_obstacle_evidence: None,
        budget_evidence: false,
        budget_evidence_source: EvidenceSource::None,
        budget_evidence_confidence: EvidenceConfidence::Low,
        financial_delay_source: EvidenceSource::None,
        financial_delay_confidence: EvidenceConfidence::Low,
        deal_product_scope: DealProductScope::Unknown,
        activity_actor: None,
        activity_object: None,
        activity_confidence: None,
        manual_action_required: None,
        manual_action_description: None,
        manual_action_tasks: Vec::new(),
        time_sensitive_facts: Vec::new(),
        known_package_interest: None,
        decision_obstacle_evidence: None,
        decision_criteria: Vec::new(),
        decision_obstacle: DecisionObstacle::None,
        project_details: Vec::new(),
        project_areas: Vec::new(),
        budget_target_approx: None,
        comparing_quotes: false,
        active_decision_factors: Vec::new(),
        latest_customer_state: LatestCustomerState::Unknown,
        active_unresolved_facts: Vec::new(),
        existing_countertop_material: None,
        existing_countertop_removal: false,
        removal_required: false,
        removal_component: None,
        customer_project_components: Vec::new(),
        unsupported_requested_components: Vec::new(),
        historical_quote_expiration_warning: false,
        historical_task: false,
        alternative_material_offered: None,
        installation_requested: false,
        price_context: PriceContext::None,
        material_type_preferences: Vec::new(),
        countertop_color_preferences: Vec::new(),
        estimated_ready_at: None,
        estimated_ready_at_source: EvidenceSource::None,
        estimated_ready_at_confidence: EvidenceConfidence::Low,
        estimated_ready_at_precision: None,
        estimated_ready_window_start: None,
        estimated_ready_window_end: None,
        project_scale: ProjectScale::Unknown,
        approx_unit_count: None,
        preferred_next_step: PreferredNextStep::None,
        preferred_next_step_source: PreferredNextStepSource::None,
        preferred_next_step_confidence: EvidenceConfidence::Low,
        customer_requested_contact_method: None,
        customer_requested_contact_methods: Vec::new(),
        customer_resume_condition: None,
        edge_preferences: Vec::new(),
        salesperson_planned_contact_method: None,
        appointment_companion: None,
        completed_estimate_satisfied: false,
        customer_requested_contact_window_start: None,
        customer_requested_contact_window_end: None,
        planned_customer_event_type: None,
        planned_customer_event_window_start: None,
        planned_customer_event_window_end: None,
        planned_customer_event_confidence: None,
        planned_customer_event_date_label: None,
        visual_style: None,
        showroom_interest: false,
        countertop_measurements_status: None,
        project_size_raw: None,
        countertop_sqft: None,
        backsplash_sqft: None,
        island_sqft: None,
        vanity_sqft: None,
        total_project_sqft: None,
        unknown_sqft: None,
        measurement_conflict: false,
        measurement_dimensions_length_in: None,
        measurement_dimensions_width_in: None,
        calculated_area_from_dimensions_sqft: None,
        customer_reported_area_sqft: None,
        customer_reported_island_sqft: None,
        measurement_raw_text: None,
        future_volume_sqft: None,
        project_dependencies: Vec::new(),
    }
}
