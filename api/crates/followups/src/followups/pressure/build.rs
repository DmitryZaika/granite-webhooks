//! Assembles [`FollowupContactPressure`] from the timeline, the CRM profile
//! and holds (`buildFollowupContactPressure` and the helpers around it).

use super::super::attribution::{
    analyze_project_attribution, filter_customer_texts_for_target_deal,
    filter_profile_for_target_deal,
};
use super::super::classify::{InboundMessage, InboundMessageType, classify_inbound_message};
use super::super::contact_resume::{
    AutomationContactStatus, ChannelPermission, ContactResumeMode,
    extract_customer_resume_condition, infer_contact_resume,
};
use super::super::conversation::{RepOwnedInputs, rep_owned_next_step};
use super::super::crm_hold::effective_hold_until;
use super::super::dates::{Instant, day, month_index, utc_date, year};
use super::super::facts::extract::{FactOptions, QuoteFromHistory, extract_followup_facts};
use super::super::facts::manual::{
    ManualActionReconciliation, ManualActionTimelineMessage,
    clear_unfinished_step_when_manual_action_finished, extract_manual_action_status,
    extract_manual_phone_action_status, reconcile_manual_action_against_timeline,
};
use super::super::facts::quotes::{
    text_looks_like_company_scheduling_obligation, text_looks_like_payment_received,
    text_looks_like_signed_contract,
};
use super::super::facts::timing::{
    DayRange, extract_relative_day_range, extract_relative_week_range,
};
use super::super::facts::{
    BacksplashFinish, BacksplashType, ContactMethod, DealProductScope, DecisionObstacle,
    EvidenceConfidence, EvidenceSource, LatestCustomerState, ManualActionRequired,
    ManualActionStatus, MaterialDirectionStatus, PreferredNextStep, PreferredNextStepSource,
    PriceContext, ProjectScale,
};
use super::super::intent::{
    IntentChannel, IntentText, MeaningfulCustomerIntent, MeaningfulCustomerIntentType,
    ResolvedIntents, TimedText, classify_abandonment_reply, classify_intent_from_salesperson_notes,
    drop_will_recontact_superseded_by_later_visit, intent_stops_automated_pursuit,
    is_reaction_or_acknowledgment, omit_superseded_will_recontact_text,
    resolve_latest_meaningful_customer_intent, short_term_return_re,
    will_recontact_texts_superseded_by_later_visit,
};
use super::super::js::{js_len, js_slice, trim};
use super::super::lifecycle::{
    FollowupSequenceStatus, SequenceStatusInputs, TargetDealTerminal,
    infer_followup_sequence_status,
};
use super::super::phone::any_valid_nanp_phone;
use super::super::timeline::{Channel, Direction, NormalizedSenderRole, TimelineMessage};
use super::channels::{
    customer_replied_after_last_outbound, customer_requested_followup_at, email_channel_is_usable,
    email_channel_state, email_channel_status_from_history, global_pressure_level,
    infer_customer_intent, infer_phone_channel_state, looks_like_wait_request,
    phone_channel_is_usable, phone_channel_status_from_history, pressure_level,
    quote_evidence_from_history, sms_channel_is_usable, sms_channel_state,
    sms_channel_status_from_history, used_followup_angles_from_timeline,
};
use super::state::{
    StateExtras, classify_customer_followup_state, classify_hold_reason,
    has_crm_relationship_history, infer_communication_history_status, infer_crm_deal_stage,
    infer_deal_state, infer_engagement_state, infer_hold_source_type, infer_information_density,
    infer_operational_contact_state, infer_relationship_age, infer_relationship_state,
    last_conversational_rep_reply_at, last_outbound_text, latest_outbound_type_from_timeline,
    open_call_activity_requires_manual_action, scope_from_conversational_antecedent,
    scope_from_quote_discussion_context, select_followup_starting_phase, timeline_position_at,
};
use super::timeline::{
    ClassifiedMessage, MS_PER_DAY, annotate_target_identity, average_gap_days, classify_timeline,
    days_between, detect_sender_identity_conflict, extract_target_customer_name,
    is_semantic_customer_inbound, last_genuine_customer_reply_at, last_inbound_any,
    last_inbound_text, last_message_at, last_semantic_customer_reply_at, outbound_in_window,
    parse_message_at, pressure_eligible_timeline, semantic_evidence_timeline,
    summarize_timeline_contamination, unanswered_since_last_reply,
};
use super::types::{
    ChannelAttemptState, CommunicationHistoryStatus, ContactPressureLevel, CustomerFollowupState,
    CustomerIntent, CustomerMessageSemanticType, DealState, EmailChannelStatus, EngagementState,
    FollowupContactPressure, FollowupPhase, FollowupTerminalDeal, HoldReasonSource, HoldReasonType,
    InformationDensity, OperationalContactState, OtherDealSummary, PhoneChannelStatus,
    RelationshipAge, RelationshipState, SmsChannelStatus, TargetDealSummary, TerminalStatus,
};
use crate::re;

/// Everything [`build_followup_contact_pressure`] reads.
#[derive(Debug, Clone, Default)]
pub struct PressureInputs {
    pub timeline: Vec<TimelineMessage>,
    pub now: Option<Instant>,
    pub crm_hold_until: Option<Instant>,
    pub crm_note_hold_until: Option<Instant>,
    pub explicit_do_not_contact_until: Option<Instant>,
    pub terminal_deal: Option<FollowupTerminalDeal>,
    pub profile: String,
    /// `None` when unknown (not looked up), unlike an empty list.
    pub customer_emails: Option<Vec<String>>,
    pub customer_phones: Option<Vec<String>>,
    pub target_deal: Option<TargetDealSummary>,
    pub other_deals: Vec<OtherDealSummary>,
    pub explicit_reenroll: bool,
}

fn lead_form_customer_text(profile: &str) -> Option<String> {
    let bits: Vec<String> = profile
        .split('\n')
        .filter(|line| line.starts_with("Customer message:") || line.starts_with("Lead details:"))
        .map(|line| {
            re!(r"^(Customer message|Lead details):\s*", "u")
                .replace(line, "")
                .into_owned()
        })
        .collect();
    if bits.is_empty() {
        return None;
    }
    let joined = trim(&bits.join(" ")).to_string();
    (!joined.is_empty()).then_some(joined)
}

fn messages(timeline: &[ClassifiedMessage]) -> Vec<TimelineMessage> {
    timeline.iter().map(|m| m.message.clone()).collect()
}

fn add_utc_days(at: Instant, days: f64) -> Instant {
    #[allow(clippy::cast_possible_truncation)]
    let days = days.clamp(-1e8, 1e8) as i32;
    utc_date(year(at), month_index(at), day(at).saturating_add(days))
}

/// The customer's latest message names a day range ("3-5 days") or, while
/// still recent, a week phrase ("late next week").
fn timeline_contact_window(
    timeline: &[ClassifiedMessage],
    now: Instant,
) -> Option<(Instant, Instant)> {
    let mut latest_inbound = true;
    for message in timeline
        .iter()
        .rev()
        .filter(|m| is_semantic_customer_inbound(m))
    {
        let is_latest = latest_inbound;
        latest_inbound = false;
        let at = parse_message_at(&message.at);
        // Week phrases only from the latest message, and only while recent.
        let week_range = if is_latest {
            extract_relative_week_range(message.text())
        } else {
            None
        };
        #[allow(clippy::cast_precision_loss)]
        let recent_week_range = week_range.filter(|range| {
            at.is_some_and(|at| {
                at.timestamp_millis() as f64 + (range.end + 7.0) * MS_PER_DAY
                    >= now.timestamp_millis() as f64
            })
        });
        let Some(DayRange { start, end }) =
            extract_relative_day_range(message.text()).or(recent_week_range)
        else {
            continue;
        };
        let Some(at) = at else {
            continue;
        };
        return Some((add_utc_days(at, start), add_utc_days(at, end)));
    }
    None
}

fn semantic_type(
    latest_inbound: Option<&ClassifiedMessage>,
    abandonment: bool,
    inbound_text: Option<&str>,
    prior_outbound_text: &str,
) -> CustomerMessageSemanticType {
    use CustomerMessageSemanticType as T;
    if latest_inbound.is_some_and(|m| is_reaction_or_acknowledgment(m.text())) {
        return T::AcknowledgmentReaction;
    }
    if abandonment {
        return T::Decline;
    }
    let Some(text) = inbound_text.filter(|t| !t.is_empty()) else {
        return T::None;
    };
    let trimmed = trim(text);
    if re!(r"unsubscribe|stop contacting|not interested", "i").is_match(text) {
        T::Decline
    } else if looks_like_wait_request(text) {
        T::Delay
    } else if re!(r"\?\s*$").is_match(trimmed) {
        T::Question
    } else if re!(
        r"^(yes|yeah|yep|sure|sounds good|that would be (awesome|great)|yes that would be awesome)",
        "i"
    )
    .is_match(trimmed)
        && re!(
            r"specials|check back|follow up|closer to that date|reach out|send you",
            "i"
        )
        .is_match(prior_outbound_text)
    {
        T::FollowupPermission
    } else if re!(
        r"^(yes|yeah|yep|ok|okay|thanks|thank you|got it|understood)[.!]?\s*$",
        "i"
    )
    .is_match(trimmed)
    {
        T::Acknowledgment
    } else {
        T::ProjectStatement
    }
}

/// Merges the latest customer state with the intents and the hold reason.
fn reconcile_latest_customer_state(
    state: LatestCustomerState,
    intent: Option<&MeaningfulCustomerIntent>,
    additional: &[MeaningfulCustomerIntent],
    hold_type: Option<HoldReasonType>,
    hold_source: Option<HoldReasonSource>,
    hold_evidence: Option<&str>,
) -> LatestCustomerState {
    use LatestCustomerState as S;
    use MeaningfulCustomerIntentType as I;
    let kind = intent.map(|i| i.kind);
    let extra = |k: I| additional.iter().any(|item| item.kind == k);
    if state == S::Declined || kind == Some(I::NotInterested) {
        return S::Declined;
    }
    // A customer-sourced short-term delay is a known state, never UNKNOWN.
    if state == S::Unknown
        && hold_type == Some(HoldReasonType::HigherPriorityDelay)
        && (matches!(
            hold_source,
            Some(HoldReasonSource::CustomerSms | HoldReasonSource::CustomerEmail)
        ) || short_term_return_re().is_match(hold_evidence.unwrap_or("")))
    {
        return S::Delayed;
    }
    if state == S::PausedIndefinite || kind == Some(I::NotNow) {
        return S::PausedIndefinite;
    }
    if state == S::WaitingOnDependency || kind == Some(I::AlmostReady) || extra(I::AlmostReady) {
        return S::WaitingOnDependency;
    }
    if matches!(state, S::PriceObjection | S::NearTermProject) {
        return state;
    }
    if kind == Some(I::CustomerWillRecontact) && state == S::Unknown {
        return S::Waiting;
    }
    if kind == Some(I::Delayed) {
        return S::Delayed;
    }
    if kind == Some(I::FutureProject) || (kind == Some(I::Waiting) && extra(I::FutureProject)) {
        return S::FutureProject;
    }
    if kind == Some(I::Waiting) && state == S::Unknown {
        return S::Waiting;
    }
    if kind == Some(I::Planning) {
        return S::Planning;
    }
    state
}

/// The contact-pressure picture of a customer.
#[allow(clippy::too_many_lines, clippy::cognitive_complexity)]
pub fn build_followup_contact_pressure(params: &PressureInputs) -> FollowupContactPressure {
    let now = params.now.unwrap_or_else(chrono::Utc::now);
    let profile = params.profile.as_str();
    let emails: &[String] = params.customer_emails.as_deref().unwrap_or(&[]);
    let phones: &[String] = params.customer_phones.as_deref().unwrap_or(&[]);
    let target_name = extract_target_customer_name(profile);
    let mut timeline_with_identity = classify_timeline(&params.timeline, emails, phones);
    annotate_target_identity(&mut timeline_with_identity, target_name.as_deref());
    let contamination = summarize_timeline_contamination(&timeline_with_identity);
    let timeline = pressure_eligible_timeline(&timeline_with_identity);
    let timeline_messages = messages(&timeline);
    let last_outbound = last_message_at(timeline_messages.iter(), Direction::Outbound);
    let last_inbound = last_semantic_customer_reply_at(&timeline);
    let last_identity_matched_inbound = last_genuine_customer_reply_at(&timeline);
    let latest_inbound = last_inbound_any(&timeline_with_identity);
    let inbound_classification = latest_inbound.map(|message| {
        classify_inbound_message(&InboundMessage {
            sms: message.channel == Some(Channel::Sms),
            from_email: message.from_email.as_deref(),
            from_phone: message.from_phone.as_deref(),
            text: message.text.as_deref(),
            customer_emails: emails,
            customer_phones: phones,
        })
    });
    let inbound_text = last_inbound_text(&timeline);
    let unanswered = unanswered_since_last_reply(&timeline);
    let requested = customer_requested_followup_at(&timeline, now);
    let effective_hold = effective_hold_until(&[
        params.crm_hold_until,
        params.crm_note_hold_until,
        requested,
        params.explicit_do_not_contact_until,
    ]);
    let days_since_last_outbound = last_outbound.map(|at| days_between(now, at).max(0.0));
    let days_since_last_customer_reply = last_inbound.map(|at| days_between(now, at).max(0.0));
    let last_customer_message_wait = inbound_text
        .as_deref()
        .is_some_and(|t| !t.is_empty() && looks_like_wait_request(t));
    let first_outbound = timeline
        .iter()
        .find(|m| m.direction == Direction::Outbound)
        .and_then(|m| parse_message_at(&m.at));
    let chase_days = first_outbound.map_or(days_since_last_outbound, |first| {
        Some(days_between(now, first).max(0.0))
    });
    let sms_state = sms_channel_state(&timeline);
    let email_state = email_channel_state(&timeline);
    let sms_status = sms_channel_status_from_history(profile, &timeline_messages, emails, phones);
    let email_status =
        email_channel_status_from_history(profile, &timeline_messages, emails, phones);
    let phone_status =
        phone_channel_status_from_history(profile, &timeline_messages, emails, phones);
    let phone_state = match phone_status {
        PhoneChannelStatus::WrongNumber
        | PhoneChannelStatus::InvalidFormat
        | PhoneChannelStatus::Unreachable => ChannelAttemptState::Unsuccessful,
        PhoneChannelStatus::AttemptedNoResponse => ChannelAttemptState::AttemptedNoResponse,
        _ => infer_phone_channel_state(profile),
    };
    let evidence_timeline = semantic_evidence_timeline(&timeline_with_identity);
    let quote_history = quote_evidence_from_history(&messages(&evidence_timeline), profile);
    let target_terminal = params
        .target_deal
        .as_ref()
        .and_then(|deal| deal.terminal_status);
    let attribution = analyze_project_attribution(
        profile,
        params.target_deal.as_ref().and_then(|deal| deal.id),
        params
            .target_deal
            .as_ref()
            .map(|deal| deal.status_label.as_str()),
        &[],
        params
            .target_deal
            .as_ref()
            .is_some_and(|deal| deal.is_terminal)
            || target_terminal.is_some(),
    );
    let fact_profile = filter_profile_for_target_deal(profile, &attribution);
    let quote_evidence = quote_history.quote_evidence;
    let later_outbound_text = last_outbound_text(&evidence_timeline);
    let target_deal_only =
        attribution.multi_project_customer && attribution.target_deal_is_terminal;
    let inbound_texts_for_intent: Vec<IntentText> = {
        let raw: Vec<IntentText> = timeline
            .iter()
            .rev()
            .filter(|m| is_semantic_customer_inbound(m))
            .filter_map(|m| {
                let text = trim(m.text());
                (!text.is_empty()).then(|| IntentText {
                    text: text.to_string(),
                    channel: Some(if m.channel == Some(Channel::Email) {
                        IntentChannel::Email
                    } else {
                        IntentChannel::Sms
                    }),
                    at: Some(m.at.clone()),
                })
            })
            .collect();
        let allowed: std::collections::HashSet<String> = filter_customer_texts_for_target_deal(
            &raw.iter().map(|item| item.text.clone()).collect::<Vec<_>>(),
            &attribution,
            target_deal_only,
        )
        .into_iter()
        .map(|text| trim(&text).to_string())
        .collect();
        raw.into_iter()
            .filter(|item| allowed.contains(trim(&item.text)))
            .collect()
    };
    let attributed_inbound_text = inbound_texts_for_intent
        .first()
        .map(|item| item.text.clone())
        .unwrap_or_default();
    let rep_texts: Vec<String> = evidence_timeline
        .iter()
        .filter(|m| {
            m.direction == Direction::Outbound
                || m.normalized_sender_role == Some(NormalizedSenderRole::SalesRep)
        })
        .map(|m| m.text().to_string())
        .collect();
    let mut facts = extract_followup_facts(
        &fact_profile,
        quote_evidence,
        &FactOptions {
            latest_customer_text: Some(attributed_inbound_text.clone()),
            all_customer_texts: inbound_texts_for_intent
                .iter()
                .map(|item| item.text.clone())
                .collect(),
            later_outbound_text: Some(later_outbound_text.clone()),
            all_outbound_texts: rep_texts.clone(),
            now: Some(now),
            quote_from_history: Some(QuoteFromHistory {
                quote_evidence: quote_history.quote_evidence,
                quote_requested: Some(quote_history.quote_requested),
                quote_created: Some(quote_history.quote_created),
                quote_sent: quote_history.quote_sent,
                quote_received_or_discussed: quote_history.quote_received_or_discussed,
                source: quote_history.source,
                confidence: quote_history.confidence,
            }),
        },
    );
    if let Some((start, end)) = timeline_contact_window(&timeline, now) {
        facts.customer_requested_contact_window_start = Some(start);
        facts.customer_requested_contact_window_end = Some(end);
    }
    let timed: Vec<TimedText> = timeline
        .iter()
        .map(|m| TimedText {
            text: m.text().to_string(),
            at: Some(m.at.clone()),
        })
        .collect();
    let superseded = will_recontact_texts_superseded_by_later_visit(&timed);
    let intent_inbound_texts: Vec<IntentText> = inbound_texts_for_intent
        .iter()
        .filter(|item| !superseded.contains(trim(&item.text)))
        .cloned()
        .collect();
    let mut resume_parts: Vec<String> = vec![profile.to_string()];
    resume_parts.extend(intent_inbound_texts.iter().map(|item| item.text.clone()));
    resume_parts.push(lead_form_customer_text(profile).unwrap_or_default());
    let resume_text = omit_superseded_will_recontact_text(&resume_parts.join("\n"), &superseded);
    let resume = infer_contact_resume(
        &resume_text,
        requested.is_some() || effective_hold.is_some(),
    );
    let customer_resume_condition = extract_customer_resume_condition(&resume_text);
    let prior_outbound_text = timeline
        .iter()
        .rev()
        .filter(|m| m.direction == Direction::Outbound)
        .find(|m| !matches!((last_inbound, parse_message_at(&m.at)), (Some(inbound), Some(at)) if at > inbound))
        .map(|m| m.text().to_string())
        .unwrap_or_default();
    let abandonment_reply =
        classify_abandonment_reply(inbound_text.as_deref().unwrap_or(""), &prior_outbound_text);
    let resolution = if !inbound_texts_for_intent.is_empty()
        || re!(r"Activity:|Deal note ").is_match(&fact_profile)
    {
        resolve_latest_meaningful_customer_intent(
            &intent_inbound_texts,
            Some(&fact_profile),
            Some(&prior_outbound_text),
        )
    } else {
        ResolvedIntents {
            latest: classify_intent_from_salesperson_notes(&fact_profile),
            additional: Vec::new(),
        }
    };
    let resolved_intents = drop_will_recontact_superseded_by_later_visit(
        resolution.latest,
        resolution.additional,
        &timed,
        &superseded,
    );
    let latest_intent = resolved_intents.latest;
    let additional_intents = resolved_intents.additional;
    let latest_kind = latest_intent.as_ref().map(|i| i.kind);
    let customer_will_recontact = latest_kind
        == Some(MeaningfulCustomerIntentType::CustomerWillRecontact)
        || additional_intents
            .iter()
            .any(|item| item.kind == MeaningfulCustomerIntentType::CustomerWillRecontact);
    // A direct terminal statement ends automation whether or not it answered
    // an abandonment probe, and it outranks any CRM activity date.
    let terminal_abandonment = latest_kind == Some(MeaningfulCustomerIntentType::NotInterested)
        || facts.latest_customer_state == LatestCustomerState::Declined;
    let resolved_latest_terminal_status_at =
        params
            .terminal_deal
            .and_then(|deal| deal.at)
            .or(if terminal_abandonment {
                last_inbound
            } else {
                None
            });
    let contact_resume_mode = if terminal_abandonment {
        ContactResumeMode::ManualReenable
    } else if customer_will_recontact {
        ContactResumeMode::CustomerInitiates
    } else {
        resume.contact_resume_mode
    };
    let last_conversational_rep_at = last_conversational_rep_reply_at(&timeline);
    let customer_message_semantic_type = semantic_type(
        latest_inbound,
        abandonment_reply.is_some(),
        inbound_text.as_deref(),
        &prior_outbound_text,
    );
    let exchange_complete = matches!(
        customer_message_semantic_type,
        CustomerMessageSemanticType::FollowupPermission
            | CustomerMessageSemanticType::Acknowledgment
            | CustomerMessageSemanticType::AcknowledgmentReaction
    );
    let customer_reply_handled = if contact_resume_mode == ContactResumeMode::CustomerInitiates {
        params.explicit_reenroll
    } else {
        matches!((last_inbound, last_conversational_rep_at), (Some(inbound), Some(rep)) if rep >= inbound)
            || exchange_complete
    };
    let terminal_customer_intent = intent_stops_automated_pursuit(latest_intent.as_ref());
    let target_deal_terminal_status =
        target_terminal.or_else(|| params.terminal_deal.map(|deal| deal.status));
    let customer_waiting_for_human_reply = last_inbound.is_some()
        && !customer_reply_handled
        && !exchange_complete
        && !terminal_customer_intent
        && target_deal_terminal_status.is_none();
    let timeline_with_identity_messages = messages(&timeline_with_identity);
    let rep_owned = rep_owned_next_step(&RepOwnedInputs {
        timeline: &timeline_with_identity_messages,
        latest_customer_state: Some(facts.latest_customer_state),
        latest_intent: latest_intent.as_ref(),
        latest_inbound_type: inbound_classification.as_ref().map(|c| c.kind),
        customer_name: target_name.as_deref(),
        now_ms: now.timestamp_millis(),
    });
    let opted_out =
        sms_status == SmsChannelStatus::OptedOut || email_status == EmailChannelStatus::OptedOut;
    let previous_sequence_status =
        last_inbound.map(|_| FollowupSequenceStatus::StoppedCustomerReplied);
    let followup_sequence_status = infer_followup_sequence_status(&SequenceStatusInputs {
        genuine_customer_reply_at: last_identity_matched_inbound,
        target_deal_terminal: target_deal_terminal_status.map(|status| match status {
            TerminalStatus::Won => TargetDealTerminal::Won,
            TerminalStatus::Lost => TargetDealTerminal::Lost,
        }),
        opted_out,
        explicit_reenroll: params.explicit_reenroll,
        customer_reply_handled,
        crm_hold_until: effective_hold,
        now: Some(now),
        last_outbound_at: last_outbound,
        last_conversational_rep_reply_at: last_conversational_rep_at,
    });
    let deal_stage_hold = re!(r"\bDeal:.*On Hold", "i").is_match(profile);
    let pricing_change_valid_until = facts
        .time_sensitive_facts
        .iter()
        .find(|f| f.kind == "pricing_change")
        .and_then(|f| f.valid_until);
    let quote_unanswered = quote_evidence && unanswered.total > 0.0 && last_inbound.is_none();
    let sms_contact_pressure = pressure_level(unanswered.sms, sms_state);
    let email_contact_pressure = pressure_level(unanswered.email, email_state);
    let phone_contact_pressure = pressure_level(
        if phone_state == ChannelAttemptState::Unsuccessful {
            3.0
        } else {
            0.0
        },
        phone_state,
    );
    let outbound_last30_days = outbound_in_window(timeline_messages.iter(), now, 30.0);
    let global_contact_pressure =
        global_pressure_level(unanswered.total, sms_state, outbound_last30_days);
    let crm_relationship_history = has_crm_relationship_history(profile);
    let has_relationship_history = crm_relationship_history
        || last_outbound.is_some()
        || last_inbound.is_some()
        || !timeline.is_empty();
    let hold_evidence = classify_hold_reason(profile, &timeline, effective_hold.is_some());
    let manual_phone_action = extract_manual_phone_action_status(profile);
    let manual_action = extract_manual_action_status(profile);
    let sender_identity_conflict = detect_sender_identity_conflict(profile, &timeline_messages);
    let allow_cross_message_scope = !target_deal_only;
    let conversational_scope = if allow_cross_message_scope {
        scope_from_conversational_antecedent(&timeline)
    } else {
        None
    };
    let quote_discussion_scope = if allow_cross_message_scope {
        scope_from_quote_discussion_context(&timeline)
    } else {
        None
    };
    let seed_history = infer_communication_history_status(
        last_outbound.is_some(),
        last_inbound.is_some(),
        crm_relationship_history,
        CustomerFollowupState::FreshLead,
    );
    let customer_requested_phone_in_timeline = timeline_with_identity.iter().any(|m| {
        is_semantic_customer_inbound(m)
            && re!(r"give .{0,16}a call|call me\b|please call|phone call", "i").is_match(m.text())
    });
    let mut workflow_parts: Vec<String> = vec![attributed_inbound_text.clone()];
    workflow_parts.extend(
        inbound_texts_for_intent
            .iter()
            .map(|item| item.text.clone()),
    );
    workflow_parts.push(later_outbound_text);
    workflow_parts.extend(rep_texts.iter().cloned());
    workflow_parts.push(fact_profile.clone());
    let workflow_corpus = workflow_parts.join("\n");
    let company_owes_scheduling = text_looks_like_company_scheduling_obligation(&workflow_corpus);
    let contract_signed = text_looks_like_signed_contract(&workflow_corpus);
    let payment_received = text_looks_like_payment_received(&workflow_corpus);
    let quote_tasks_superseded = facts.latest_customer_state
        == LatestCustomerState::TemplateApproved
        || contract_signed
        || (payment_received && company_owes_scheduling);
    let open_call_needs_manual_action = open_call_activity_requires_manual_action(profile);
    let phones_ok = params.customer_phones.is_none() || any_valid_nanp_phone(phones);
    let manual_action_required = if (facts.customer_requested_contact_method
        == Some(ContactMethod::Phone)
        || customer_requested_phone_in_timeline)
        && phone_channel_is_usable(phone_status)
        && phones_ok
    {
        Some(ManualActionRequired::PhoneCall)
    } else if company_owes_scheduling {
        Some(ManualActionRequired::SalesTask)
    } else if manual_action.required.is_some() && !quote_tasks_superseded {
        manual_action.required
    } else if manual_phone_action.0 == ManualActionStatus::Pending && open_call_needs_manual_action
    {
        Some(ManualActionRequired::PhoneCall)
    } else {
        None
    };
    let status_before_reconcile = if company_owes_scheduling {
        ManualActionStatus::Pending
    } else if quote_tasks_superseded {
        ManualActionStatus::Superseded
    } else if manual_action.required == Some(ManualActionRequired::SalesTask) {
        manual_action.status
    } else {
        manual_phone_action.0
    };
    let reconcile_tasks = if company_owes_scheduling {
        vec!["SCHEDULING_CONTACT".to_string()]
    } else {
        manual_action.tasks.clone()
    };
    let reconcile_timeline: Vec<ManualActionTimelineMessage> = timeline_with_identity
        .iter()
        .map(|m| ManualActionTimelineMessage {
            at: m.at.clone(),
            inbound: m.inbound(),
            text: m.text.clone(),
            is_mirrored_duplicate: m.is_mirrored_duplicate == Some(true),
        })
        .collect();
    let reconciled_status = reconcile_manual_action_against_timeline(&ManualActionReconciliation {
        required: manual_action_required,
        status: status_before_reconcile,
        tasks: &reconcile_tasks,
        profile,
        timeline: &reconcile_timeline,
        quote_sent: quote_history.quote_sent,
        customer_requested_contact_method: facts.customer_requested_contact_method,
    });
    clear_unfinished_step_when_manual_action_finished(
        &mut facts,
        reconciled_status,
        manual_action.description.as_deref(),
        profile,
    );
    let effective_sequence_status = if customer_will_recontact
        && matches!(
            followup_sequence_status,
            FollowupSequenceStatus::Active | FollowupSequenceStatus::StoppedCustomerReplied
        ) {
        FollowupSequenceStatus::Paused
    } else {
        followup_sequence_status
    };
    let call_activity_description = profile
        .split('\n')
        .find(|line| {
            line.starts_with("Activity:")
                && re!(r"\bcall (him|her|them|back|the customer)\b", "i").is_match(line)
                && !re!(r"\bcompleted\b").is_match(line)
        })
        .map(|line| {
            let stripped = re!(r"^Activity:\s*", "i").replace(line, "");
            trim(stripped.split(" ·").next().unwrap_or("")).to_string()
        });
    let seed_relationship = infer_relationship_state(profile, seed_history);
    let latest_inbound_type = latest_inbound.and_then(|message| {
        if message.normalized_sender_role == Some(NormalizedSenderRole::SalesRep) {
            Some(InboundMessageType::InternalMessage)
        } else {
            inbound_classification
                .as_ref()
                .map(|c| c.kind)
                .or(message.inbound_type)
        }
    });
    let matched_customer_identity = latest_inbound.is_some_and(|message| {
        message.normalized_sender_role == Some(NormalizedSenderRole::Customer)
            && message
                .matched_customer_identity
                .or_else(|| {
                    inbound_classification
                        .as_ref()
                        .map(|c| c.matched_customer_identity)
                })
                .unwrap_or(false)
    });
    let phone_call_pending = manual_action_required == Some(ManualActionRequired::PhoneCall)
        && facts.preferred_next_step == PreferredNextStep::None
        && reconciled_status == ManualActionStatus::Pending;
    let phone_preference_dropped = facts.preferred_next_step == PreferredNextStep::PhoneCall
        && !open_call_needs_manual_action
        && manual_action_required.is_none();
    let superseded_action = reconciled_status == ManualActionStatus::Superseded;
    let sms_status_out = if resume.sms_restricted {
        SmsChannelStatus::CustomerRequestedNoText
    } else {
        sms_status
    };
    let automation_contact_status = if contact_resume_mode == ContactResumeMode::CustomerInitiates {
        AutomationContactStatus::WaitForCustomer
    } else if terminal_abandonment
        || effective_sequence_status == FollowupSequenceStatus::StoppedTerminalDeal
        || target_deal_terminal_status.is_some()
    {
        AutomationContactStatus::Stopped
    } else {
        resume
            .automation_contact_status
            .unwrap_or(match effective_sequence_status {
                FollowupSequenceStatus::Paused => AutomationContactStatus::Paused,
                FollowupSequenceStatus::Active => AutomationContactStatus::Active,
                _ => AutomationContactStatus::Stopped,
            })
    };
    let summary = if attributed_inbound_text.is_empty() {
        None
    } else if js_len(&attributed_inbound_text) > 200 {
        Some(format!("{}…", js_slice(&attributed_inbound_text, 0, 199)))
    } else {
        Some(attributed_inbound_text.clone())
    };
    let last_customer_originated_information = {
        let mut parts: Vec<String> =
            vec![lead_form_customer_text(&fact_profile).unwrap_or_default()];
        parts.extend(
            inbound_texts_for_intent
                .iter()
                .map(|item| item.text.clone()),
        );
        let parts: Vec<String> = parts
            .into_iter()
            .filter(|part| !trim(part).is_empty())
            .collect();
        (!parts.is_empty()).then(|| parts.join("\n"))
    };
    let deal_product_scope = if target_deal_only {
        facts.deal_product_scope
    } else if facts.deal_product_scope == DealProductScope::Unknown {
        conversational_scope
            .as_ref()
            .or(quote_discussion_scope.as_ref())
            .map_or(facts.deal_product_scope, |(scope, _)| *scope)
    } else {
        facts.deal_product_scope
    };
    let base = FollowupContactPressure {
        days_since_last_customer_reply,
        days_since_last_outbound,
        unanswered_sms_since_last_reply: unanswered.sms,
        unanswered_email_since_last_reply: unanswered.email,
        unanswered_total_since_last_reply: unanswered.total,
        outbound_last7_days: outbound_in_window(timeline_messages.iter(), now, 7.0),
        outbound_last30_days,
        outbound_last90_days: outbound_in_window(timeline_messages.iter(), now, 90.0),
        average_gap_last5_outbounds: average_gap_days(timeline_messages.iter()),
        customer_requested_followup_at: requested,
        crm_hold_until: effective_hold,
        last_customer_message_at: last_inbound,
        last_customer_message_summary: summary,
        latest_inbound_event_at: latest_inbound.and_then(|m| parse_message_at(&m.at)),
        latest_meaningful_customer_message_at: last_inbound,
        latest_terminal_status_at: resolved_latest_terminal_status_at,
        latest_terminal_status: params.terminal_deal.map(|deal| deal.status),
        customer_state: CustomerFollowupState::FreshLead,
        customer_intent: infer_customer_intent(profile),
        global_contact_pressure,
        sms_contact_pressure,
        email_contact_pressure,
        phone_contact_pressure,
        sms_channel_state: sms_state,
        email_channel_state: email_state,
        phone_channel_state: phone_state,
        used_followup_angles: used_followup_angles_from_timeline(&timeline_messages),
        quote_requested: facts.quote_requested,
        quote_evidence,
        quote_created: facts.quote_created,
        quote_sent: facts.quote_sent,
        quote_received_or_discussed: facts.quote_received_or_discussed,
        quote_evidence_source: facts.quote_evidence_source,
        quote_evidence_confidence: facts.quote_evidence_confidence,
        quote_expiration_evidence: facts.quote_expiration_evidence.clone(),
        quote_expiration_date: facts.quote_expiration_date,
        quote_type: facts.quote_type,
        explicit_customer_request: facts.explicit_customer_request.clone(),
        explicit_customer_request_kind: facts.explicit_customer_request_kind,
        needs_sink: facts.needs_sink,
        sink_required: facts.sink_required,
        sink_source: facts.sink_source,
        needs_sink_from_company: facts.needs_sink_from_company,
        backsplash_known: facts.backsplash_known,
        backsplash_type: facts.backsplash_type,
        stone_backsplash_required: facts.stone_backsplash_required,
        backsplash_finish: facts.backsplash_finish,
        backsplash_material: facts.backsplash_material.clone(),
        backsplash_description: facts.backsplash_description.clone(),
        uncertain_material_direction: facts.uncertain_material_direction,
        material_direction_status: facts.material_direction_status,
        preferred_material: facts.preferred_material.clone(),
        did_not_find_suitable_option: facts.did_not_find_suitable_option,
        rejected_previous_options: facts.rejected_previous_options,
        does_not_like_current_selection: facts.does_not_like_current_selection,
        crm_deal_stage: infer_crm_deal_stage(params.target_deal.as_ref(), profile),
        operational_contact_state: infer_operational_contact_state(
            last_outbound.is_some(),
            last_inbound.is_some(),
            has_relationship_history,
        ),
        countertop_preferences: facts.countertop_preferences.clone(),
        cabinet_preferences: facts.cabinet_preferences.clone(),
        unfinished_next_step_kind: facts.unfinished_next_step_kind,
        timeline_evidence_source: facts.timeline_evidence_source,
        timeline_evidence_confidence: facts.timeline_evidence_confidence,
        project_scope_source: if conversational_scope.is_some() {
            EvidenceSource::CustomerReplyToContext
        } else {
            facts.project_scope_source
        },
        project_scope_confidence: if conversational_scope.is_some() {
            EvidenceConfidence::Medium
        } else {
            facts.project_scope_confidence
        },
        construction_delay_source: facts.construction_delay_source,
        construction_delay_confidence: facts.construction_delay_confidence,
        financial_delay_source: facts.financial_delay_source,
        financial_delay_confidence: facts.financial_delay_confidence,
        // Seed value only: the real phase is computed at the end.
        followup_starting_phase: FollowupPhase::ActivePursuit,
        customer_replied_after_last_outbound: customer_replied_after_last_outbound(
            &timeline_messages,
            emails,
            phones,
        ),
        latest_inbound_type,
        latest_inbound_sender: inbound_classification
            .as_ref()
            .map(|c| c.sender_label.clone()),
        matched_customer_identity,
        reply_classification_confidence: inbound_classification.as_ref().map(|c| c.confidence),
        reply_classification_reason: inbound_classification
            .as_ref()
            .map(|c| c.reason.to_string()),
        original_timeline_evidence: facts.original_timeline_evidence.clone(),
        known_material: facts.known_material.clone(),
        quoted_material_context: facts.quoted_material_context.clone(),
        favorite_material: facts.favorite_material.clone(),
        known_scope: facts.known_scope.clone(),
        unfinished_next_step: facts.unfinished_next_step.clone(),
        unfinished_possibly_satisfied: facts.unfinished_possibly_satisfied,
        construction_dependency_evidence: facts.construction_dependency_evidence.clone(),
        communication_history_status: seed_history,
        relationship_state: seed_relationship,
        relationship_age: infer_relationship_age(seed_relationship),
        engagement_state: infer_engagement_state(
            CustomerFollowupState::FreshLead,
            seed_relationship,
            days_since_last_outbound,
            last_inbound,
        ),
        deal_state: infer_deal_state(
            profile,
            quote_evidence,
            target_deal_terminal_status,
            last_outbound.is_some(),
        ),
        followup_sequence_status: effective_sequence_status,
        latest_meaningful_customer_intent: latest_intent.clone(),
        additional_customer_intents: additional_intents.clone(),
        last_conversational_rep_reply_at: last_conversational_rep_at,
        customer_reply_handled,
        customer_waiting_for_human_reply,
        rep_owned_next_step: rep_owned,
        sms_available: !resume.sms_restricted && phones_ok && sms_channel_is_usable(sms_status),
        email_available: !resume.email_restricted
            && (params.customer_emails.is_none() || !emails.is_empty())
            && email_channel_is_usable(email_status),
        phone_available: !resume.phone_restricted
            && phones_ok
            && phone_channel_is_usable(phone_status),
        known_package_interest: facts.known_package_interest.clone(),
        decision_obstacle_evidence: facts.decision_obstacle_evidence.clone(),
        previous_sequence_status,
        sequence_generated_at: params.explicit_reenroll.then_some(now),
        latest_outbound_type: latest_outbound_type_from_timeline(&timeline),
        historical_material_preferences: facts.historical_material_preferences.clone(),
        material_preferences: facts.material_preferences.clone(),
        current_confirmed_material: facts.current_confirmed_material.clone(),
        material_selection_history: facts.material_selection_history,
        material_selection_evidence: facts.material_selection_evidence,
        material_selection_evidence_source: facts.material_selection_evidence_source,
        deal_product_scope,
        target_deal_id: attribution
            .target_deal_id
            .or_else(|| params.target_deal.as_ref().and_then(|deal| deal.id)),
        target_deal_status: params
            .target_deal
            .as_ref()
            .map(|deal| deal.status_label.clone()),
        other_deals: params.other_deals.clone(),
        multi_project_customer: attribution.multi_project_customer,
        other_project_markers: attribution.other_project_markers.clone(),
        target_project_address: attribution.target_address.clone(),
        other_deal_ids: attribution.other_deal_ids,
        deal_stage_hold,
        hard_contact_hold_until: effective_hold,
        activity_actor: facts.activity_actor,
        activity_object: facts.activity_object.clone(),
        pricing_change_valid_until,
        hold_reason_type: hold_evidence.as_ref().map(|h| h.hold_reason_type),
        hold_reason_source: hold_evidence.as_ref().and_then(|h| h.hold_reason_source),
        hold_reason_evidence: hold_evidence
            .as_ref()
            .and_then(|h| h.hold_reason_evidence.clone()),
        hold_reason_at: hold_evidence.as_ref().and_then(|h| h.hold_reason_at),
        hold_reason_confidence: hold_evidence.as_ref().map(|h| h.hold_reason_confidence),
        hold_source_type: infer_hold_source_type(
            hold_evidence.as_ref().map(|h| h.hold_reason_type),
            hold_evidence.as_ref().and_then(|h| h.hold_reason_source),
            profile,
        ),
        last_conversation_reply: (!attributed_inbound_text.is_empty())
            .then(|| attributed_inbound_text.clone()),
        last_customer_originated_information,
        sms_channel_status: sms_status_out,
        email_channel_status: email_status,
        phone_channel_status: phone_status,
        has_relationship_history,
        contact_resume_mode,
        automation_contact_status,
        sms_permission: if resume.sms_restricted {
            ChannelPermission::Restricted
        } else if matches!(
            sms_status,
            SmsChannelStatus::InvalidFormat | SmsChannelStatus::WrongNumber
        ) {
            ChannelPermission::Unavailable
        } else {
            ChannelPermission::Allowed
        },
        email_permission: if resume.email_restricted {
            ChannelPermission::Restricted
        } else if params.customer_emails.is_some() && emails.is_empty() {
            ChannelPermission::Unavailable
        } else {
            ChannelPermission::Allowed
        },
        phone_permission: if resume.phone_restricted {
            ChannelPermission::Restricted
        } else if matches!(
            phone_status,
            PhoneChannelStatus::InvalidFormat | PhoneChannelStatus::WrongNumber
        ) {
            ChannelPermission::Unavailable
        } else if phone_status == PhoneChannelStatus::Untested {
            ChannelPermission::Unknown
        } else {
            ChannelPermission::Allowed
        },
        project_details: facts.project_details.clone(),
        project_areas: facts.project_areas.clone(),
        timeline_contamination_detected: contamination.timeline_contamination_detected,
        excluded_event_count: contamination.excluded_event_count,
        identity_conflict_count: contamination.identity_conflict_count,
        system_event_count: contamination.system_event_count,
        unmatched_event_count: contamination.unmatched_event_count,
        budget_target_approx: facts.budget_target_approx,
        budget_known: facts.budget_known,
        budget_constraint: facts.budget_constraint,
        budget_obstacle_evidence: facts.budget_obstacle_evidence.clone(),
        budget_evidence: facts.budget_evidence,
        budget_evidence_source: facts.budget_evidence_source,
        budget_evidence_confidence: facts.budget_evidence_confidence,
        comparing_quotes: facts.comparing_quotes,
        active_decision_factors: facts.active_decision_factors.clone(),
        decision_criteria: facts.decision_criteria.clone(),
        decision_obstacle: facts.decision_obstacle,
        latest_customer_state: reconcile_latest_customer_state(
            facts.latest_customer_state,
            latest_intent.as_ref(),
            &additional_intents,
            hold_evidence.as_ref().map(|h| h.hold_reason_type),
            hold_evidence.as_ref().and_then(|h| h.hold_reason_source),
            hold_evidence
                .as_ref()
                .and_then(|h| h.hold_reason_evidence.as_deref()),
        ),
        active_unresolved_facts: facts.active_unresolved_facts.clone(),
        never_tried_sms: sms_status == SmsChannelStatus::Untested
            && !timeline
                .iter()
                .any(|m| m.direction == Direction::Outbound && m.channel == Some(Channel::Sms))
            && !re!(
                r"\btexted\b|\bsent (an? )?sms\b|\btexted (him|her|them|the customer)\b",
                "i"
            )
            .is_match(profile),
        existing_countertop_material: facts.existing_countertop_material.clone(),
        existing_countertop_removal: facts.existing_countertop_removal,
        removal_required: facts.removal_required,
        removal_component: facts.removal_component,
        customer_project_components: facts.customer_project_components.clone(),
        unsupported_requested_components: facts.unsupported_requested_components.clone(),
        historical_quote_expiration_warning: facts.historical_quote_expiration_warning,
        historical_task: facts.historical_task,
        alternative_material_offered: facts.alternative_material_offered.clone(),
        installation_requested: facts.installation_requested,
        price_context: facts.price_context,
        material_type_preferences: facts.material_type_preferences.clone(),
        countertop_color_preferences: facts.countertop_color_preferences.clone(),
        estimated_ready_at: facts.estimated_ready_at,
        estimated_ready_at_source: facts.estimated_ready_at_source,
        estimated_ready_at_confidence: facts.estimated_ready_at_confidence,
        estimated_ready_at_precision: facts.estimated_ready_at_precision,
        estimated_ready_window_start: facts.estimated_ready_window_start,
        estimated_ready_window_end: facts.estimated_ready_window_end,
        timeline_position: timeline_position_at(
            now,
            facts.estimated_ready_at,
            facts.estimated_ready_window_start,
            facts.estimated_ready_window_end,
        ),
        project_scale: facts.project_scale,
        approx_unit_count: facts.approx_unit_count,
        preferred_next_step: if superseded_action || phone_preference_dropped {
            PreferredNextStep::None
        } else if phone_call_pending {
            PreferredNextStep::PhoneCall
        } else {
            facts.preferred_next_step
        },
        preferred_next_step_source: if superseded_action || phone_preference_dropped {
            PreferredNextStepSource::None
        } else if phone_call_pending {
            PreferredNextStepSource::SalespersonNote
        } else {
            facts.preferred_next_step_source
        },
        preferred_next_step_confidence: if superseded_action || phone_preference_dropped {
            EvidenceConfidence::Low
        } else if phone_call_pending {
            EvidenceConfidence::High
        } else {
            facts.preferred_next_step_confidence
        },
        customer_requested_contact_method: facts.customer_requested_contact_method,
        customer_requested_contact_methods: facts.customer_requested_contact_methods.clone(),
        customer_resume_condition: customer_resume_condition
            .or_else(|| facts.customer_resume_condition.clone()),
        edge_preferences: facts.edge_preferences.clone(),
        salesperson_planned_contact_method: facts.salesperson_planned_contact_method,
        sender_identity_conflict,
        manual_action_required,
        manual_action_description: manual_action.description.clone().or_else(|| {
            if manual_action_required == Some(ManualActionRequired::PhoneCall) {
                call_activity_description.clone()
            } else {
                None
            }
        }),
        manual_action_tasks: manual_action.tasks.clone(),
        manual_action_due_at: match manual_action_required {
            Some(ManualActionRequired::PhoneCall) if phone_channel_is_usable(phone_status) => {
                Some(effective_hold.or(facts.estimated_ready_at).unwrap_or(now))
            }
            Some(ManualActionRequired::SalesTask) => Some(now),
            _ => None,
        },
        manual_action_status: reconciled_status,
        manual_action_completed_at: if manual_action.required
            == Some(ManualActionRequired::SalesTask)
        {
            manual_action.completed_at
        } else {
            manual_phone_action.1
        },
        appointment_companion: facts.appointment_companion,
        completed_estimate_satisfied: facts.completed_estimate_satisfied,
        customer_requested_contact_window_start: facts.customer_requested_contact_window_start,
        customer_requested_contact_window_end: facts.customer_requested_contact_window_end,
        planned_customer_event_type: facts.planned_customer_event_type,
        planned_customer_event_window_start: facts.planned_customer_event_window_start,
        planned_customer_event_window_end: facts.planned_customer_event_window_end,
        planned_customer_event_confidence: facts.planned_customer_event_confidence,
        planned_customer_event_date_label: facts.planned_customer_event_date_label.clone(),
        visual_style: facts.visual_style,
        showroom_interest: facts.showroom_interest,
        countertop_measurements_status: facts.countertop_measurements_status,
        project_size_raw: facts.project_size_raw,
        countertop_sqft: facts.countertop_sqft.clone(),
        backsplash_sqft: facts.backsplash_sqft.clone(),
        island_sqft: facts.island_sqft.clone(),
        vanity_sqft: facts.vanity_sqft.clone(),
        total_project_sqft: facts.total_project_sqft.clone(),
        unknown_sqft: facts.unknown_sqft.clone(),
        measurement_conflict: facts.measurement_conflict,
        measurement_dimensions_length_in: facts.measurement_dimensions_length_in,
        measurement_dimensions_width_in: facts.measurement_dimensions_width_in,
        calculated_area_from_dimensions_sqft: facts.calculated_area_from_dimensions_sqft,
        customer_reported_area_sqft: facts.customer_reported_area_sqft,
        customer_reported_island_sqft: facts.customer_reported_island_sqft,
        measurement_raw_text: facts.measurement_raw_text.clone(),
        future_volume_sqft: facts.future_volume_sqft,
        project_dependencies: facts.project_dependencies.clone(),
        information_density: infer_information_density(&facts),
        customer_message_semantic_type,
        latest_genuine_inbound_at: last_identity_matched_inbound,
        last_outbound_at: last_outbound,
        sequence_stop_reason: previous_sequence_status,
        previous_sequence_stop_reason: previous_sequence_status,
        current_sequence_stop_reason: None,
        sequence_stopped_at: previous_sequence_status.and(last_identity_matched_inbound),
    };
    let mut for_state = base.clone();
    for_state.has_relationship_history = crm_relationship_history || last_inbound.is_some();
    let customer_state = classify_customer_followup_state(
        &for_state,
        &StateExtras {
            last_customer_message_wait,
            quote_unanswered,
            chase_days,
            quote_type: facts.quote_type,
        },
        now,
    );
    let resolved_history = infer_communication_history_status(
        last_outbound.is_some(),
        last_inbound.is_some(),
        crm_relationship_history,
        customer_state,
    );
    let relationship_state = infer_relationship_state(profile, resolved_history);
    let mut assembled = base;
    assembled.customer_state = customer_state;
    assembled.communication_history_status = resolved_history;
    assembled.relationship_state = relationship_state;
    assembled.relationship_age = infer_relationship_age(relationship_state);
    assembled.engagement_state = infer_engagement_state(
        customer_state,
        relationship_state,
        days_since_last_outbound,
        last_inbound,
    );
    assembled.deal_state = infer_deal_state(
        profile,
        quote_evidence,
        target_deal_terminal_status,
        last_outbound.is_some(),
    );
    let mut resolved = resolve_followup_pressure_inconsistencies(assembled);
    resolved.followup_starting_phase = select_followup_starting_phase(&resolved);
    resolved
}

/// The later of the cadence minimum and any hold.
pub fn earliest_followup_send_at(
    cadence_minimum_at: Instant,
    holds: &[Option<Instant>],
) -> Instant {
    match effective_hold_until(holds) {
        Some(hold) if hold > cadence_minimum_at => hold,
        _ => cadence_minimum_at,
    }
}

pub fn newer_customer_statement_beats_terminal_note(pressure: &FollowupContactPressure) -> bool {
    let Some(message_at) = pressure.last_customer_message_at else {
        return false;
    };
    pressure
        .latest_terminal_status_at
        .is_none_or(|terminal| message_at > terminal)
}

/// Fixes contradictions: blocked channels are unavailable, a verified quote
/// implies the earlier quote states, a fresh reply stops the sequence.
pub fn resolve_followup_pressure_inconsistencies(
    mut pressure: FollowupContactPressure,
) -> FollowupContactPressure {
    let phone_blocked = matches!(
        pressure.phone_channel_status,
        PhoneChannelStatus::WrongNumber | PhoneChannelStatus::DoNotCall
    );
    let sms_blocked = matches!(
        pressure.sms_channel_status,
        SmsChannelStatus::WrongNumber
            | SmsChannelStatus::Invalid
            | SmsChannelStatus::OptedOut
            | SmsChannelStatus::Undeliverable
            | SmsChannelStatus::CustomerRequestedNoText
    );
    let inbound_at = pressure
        .latest_genuine_inbound_at
        .or(pressure.last_customer_message_at);
    let historical_reply_already_passed = pressure.sequence_generated_at.is_some()
        && inbound_at.is_some_and(|inbound| {
            pressure
                .last_conversational_rep_reply_at
                .is_some_and(|rep| inbound < rep)
                || pressure.last_outbound_at.is_some_and(|out| inbound < out)
                || matches!(
                    (pressure.days_since_last_customer_reply, pressure.days_since_last_outbound),
                    (Some(reply), Some(out)) if reply > out
                )
        });
    let new_sequence_after_handled_reply = pressure.sequence_generated_at.is_some()
        && (pressure.customer_reply_handled || historical_reply_already_passed);
    let unhandled_inbound_after_sequence_start = matches!(
        (inbound_at, pressure.sequence_generated_at),
        (Some(inbound), Some(started)) if inbound >= started
    ) && !pressure.customer_reply_handled;
    let reply_must_stop_sequence = pressure.latest_inbound_type
        == Some(InboundMessageType::CustomerReply)
        && !matches!(
            pressure.followup_sequence_status,
            FollowupSequenceStatus::StoppedOptOut | FollowupSequenceStatus::StoppedTerminalDeal
        )
        && !new_sequence_after_handled_reply
        && (unhandled_inbound_after_sequence_start
            || (!historical_reply_already_passed && !pressure.customer_reply_handled));
    let verified_quote =
        pressure.quote_received_or_discussed || pressure.quote_created || pressure.quote_sent;
    let quote_was_only_requested =
        pressure.quote_requested && pressure.quote_evidence && !verified_quote;
    if pressure.quote_received_or_discussed && !pressure.quote_sent {
        pressure.quote_sent = true;
        pressure.quote_created = true;
    } else if pressure.quote_sent && !pressure.quote_created {
        pressure.quote_created = true;
    }
    pressure.quote_created = verified_quote || pressure.quote_created;
    if sms_blocked {
        pressure.sms_available = false;
    }
    if pressure.email_permission == ChannelPermission::Restricted {
        pressure.email_available = false;
    }
    if phone_blocked {
        pressure.phone_available = false;
    }
    if pressure.contact_resume_mode == ContactResumeMode::CustomerInitiates {
        pressure.automation_contact_status = AutomationContactStatus::WaitForCustomer;
    }
    if verified_quote {
        pressure.quote_evidence = true;
    } else if quote_was_only_requested {
        pressure.quote_evidence = false;
    }
    if quote_was_only_requested {
        pressure.quote_evidence_source = EvidenceSource::None;
        if pressure.deal_state == DealState::Quoted {
            pressure.deal_state = DealState::Contacted;
        }
    }
    if reply_must_stop_sequence {
        pressure.followup_sequence_status = FollowupSequenceStatus::StoppedCustomerReplied;
        pressure.current_sequence_stop_reason =
            Some(FollowupSequenceStatus::StoppedCustomerReplied);
    } else {
        pressure.current_sequence_stop_reason = None;
    }
    pressure
}

/// Salesperson clicked Generate anyway: keep the facts, drop the
/// wait-for-customer and hold gates.
pub fn contact_pressure_for_forced_generate(
    pressure: &FollowupContactPressure,
) -> FollowupContactPressure {
    let mut forced = pressure.clone();
    forced.hard_contact_hold_until = None;
    forced.customer_requested_followup_at = None;
    forced.customer_waiting_for_human_reply = false;
    forced.rep_owned_next_step = None;
    if forced.automation_contact_status == AutomationContactStatus::WaitForCustomer {
        forced.automation_contact_status = AutomationContactStatus::Active;
    }
    if forced.contact_resume_mode == ContactResumeMode::CustomerInitiates {
        forced.contact_resume_mode = ContactResumeMode::None;
    }
    if !matches!(
        forced.followup_sequence_status,
        FollowupSequenceStatus::StoppedOptOut | FollowupSequenceStatus::StoppedTerminalDeal
    ) {
        forced.followup_sequence_status = FollowupSequenceStatus::Active;
    }
    forced
}

/// A blank pressure (no history), for tests and fallbacks.
#[allow(clippy::too_many_lines)]
pub const fn empty_contact_pressure() -> FollowupContactPressure {
    FollowupContactPressure {
        days_since_last_customer_reply: None,
        days_since_last_outbound: None,
        unanswered_sms_since_last_reply: 0.0,
        unanswered_email_since_last_reply: 0.0,
        unanswered_total_since_last_reply: 0.0,
        outbound_last7_days: 0.0,
        outbound_last30_days: 0.0,
        outbound_last90_days: 0.0,
        average_gap_last5_outbounds: None,
        customer_requested_followup_at: None,
        crm_hold_until: None,
        last_customer_message_at: None,
        last_customer_message_summary: None,
        latest_inbound_event_at: None,
        latest_meaningful_customer_message_at: None,
        latest_terminal_status_at: None,
        latest_terminal_status: None,
        customer_state: CustomerFollowupState::FreshLead,
        customer_intent: CustomerIntent::Normal,
        global_contact_pressure: ContactPressureLevel::None,
        sms_contact_pressure: ContactPressureLevel::None,
        email_contact_pressure: ContactPressureLevel::None,
        phone_contact_pressure: ContactPressureLevel::None,
        sms_channel_state: ChannelAttemptState::Untested,
        email_channel_state: ChannelAttemptState::Untested,
        phone_channel_state: ChannelAttemptState::Untested,
        used_followup_angles: Vec::new(),
        quote_requested: false,
        quote_evidence: false,
        quote_created: false,
        quote_sent: false,
        quote_received_or_discussed: false,
        quote_evidence_source: EvidenceSource::None,
        quote_evidence_confidence: EvidenceConfidence::Low,
        quote_expiration_evidence: None,
        quote_expiration_date: None,
        quote_type: None,
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
        crm_deal_stage: None,
        operational_contact_state: OperationalContactState::Uncontacted,
        countertop_preferences: Vec::new(),
        cabinet_preferences: Vec::new(),
        unfinished_next_step_kind: None,
        timeline_evidence_source: EvidenceSource::None,
        timeline_evidence_confidence: EvidenceConfidence::Low,
        project_scope_source: EvidenceSource::None,
        project_scope_confidence: EvidenceConfidence::Low,
        construction_delay_source: EvidenceSource::None,
        construction_delay_confidence: EvidenceConfidence::Low,
        financial_delay_source: EvidenceSource::None,
        financial_delay_confidence: EvidenceConfidence::Low,
        followup_starting_phase: FollowupPhase::ActivePursuit,
        customer_replied_after_last_outbound: false,
        latest_inbound_type: None,
        latest_inbound_sender: None,
        matched_customer_identity: false,
        reply_classification_confidence: None,
        reply_classification_reason: None,
        original_timeline_evidence: None,
        known_material: None,
        quoted_material_context: None,
        favorite_material: None,
        known_scope: None,
        unfinished_next_step: None,
        unfinished_possibly_satisfied: false,
        construction_dependency_evidence: None,
        communication_history_status: CommunicationHistoryStatus::TrulyNew,
        relationship_state: RelationshipState::None,
        relationship_age: RelationshipAge::New,
        engagement_state: EngagementState::Unreached,
        deal_state: DealState::NotContacted,
        followup_sequence_status: FollowupSequenceStatus::Active,
        latest_meaningful_customer_intent: None,
        additional_customer_intents: Vec::new(),
        last_conversational_rep_reply_at: None,
        customer_reply_handled: false,
        customer_waiting_for_human_reply: false,
        rep_owned_next_step: None,
        sms_available: true,
        email_available: true,
        phone_available: true,
        known_package_interest: None,
        decision_obstacle_evidence: None,
        previous_sequence_status: None,
        sequence_generated_at: None,
        latest_outbound_type: None,
        historical_material_preferences: Vec::new(),
        material_preferences: Vec::new(),
        current_confirmed_material: None,
        material_selection_history: false,
        material_selection_evidence: false,
        material_selection_evidence_source: EvidenceSource::None,
        deal_product_scope: DealProductScope::Unknown,
        target_deal_id: None,
        target_deal_status: None,
        other_deals: Vec::new(),
        multi_project_customer: false,
        other_project_markers: Vec::new(),
        target_project_address: None,
        other_deal_ids: Vec::new(),
        deal_stage_hold: false,
        hard_contact_hold_until: None,
        activity_actor: None,
        activity_object: None,
        pricing_change_valid_until: None,
        hold_reason_type: None,
        hold_reason_source: None,
        hold_reason_evidence: None,
        hold_reason_at: None,
        hold_reason_confidence: None,
        hold_source_type: None,
        last_conversation_reply: None,
        last_customer_originated_information: None,
        sms_channel_status: SmsChannelStatus::Untested,
        email_channel_status: EmailChannelStatus::Untested,
        phone_channel_status: PhoneChannelStatus::Untested,
        has_relationship_history: false,
        contact_resume_mode: ContactResumeMode::None,
        automation_contact_status: AutomationContactStatus::Active,
        sms_permission: ChannelPermission::Allowed,
        email_permission: ChannelPermission::Allowed,
        phone_permission: ChannelPermission::Allowed,
        project_details: Vec::new(),
        project_areas: Vec::new(),
        timeline_contamination_detected: false,
        excluded_event_count: 0.0,
        identity_conflict_count: 0.0,
        system_event_count: 0.0,
        unmatched_event_count: 0.0,
        budget_target_approx: None,
        budget_known: false,
        budget_constraint: false,
        budget_obstacle_evidence: None,
        budget_evidence: false,
        budget_evidence_source: EvidenceSource::None,
        budget_evidence_confidence: EvidenceConfidence::Low,
        comparing_quotes: false,
        active_decision_factors: Vec::new(),
        decision_criteria: Vec::new(),
        decision_obstacle: DecisionObstacle::None,
        latest_customer_state: LatestCustomerState::Unknown,
        active_unresolved_facts: Vec::new(),
        never_tried_sms: true,
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
        timeline_position: None,
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
        sender_identity_conflict: false,
        manual_action_required: None,
        manual_action_description: None,
        manual_action_tasks: Vec::new(),
        manual_action_due_at: None,
        manual_action_status: ManualActionStatus::Pending,
        manual_action_completed_at: None,
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
        information_density: InformationDensity::Low,
        customer_message_semantic_type: CustomerMessageSemanticType::None,
        latest_genuine_inbound_at: None,
        last_outbound_at: None,
        sequence_stop_reason: None,
        previous_sequence_stop_reason: None,
        current_sequence_stop_reason: None,
        sequence_stopped_at: None,
    }
}
