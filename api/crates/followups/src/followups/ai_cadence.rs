//! The AI follow-up a rep previews, edits and starts from the flow picker.
//!
//! The preview plans the cadence for the customer, has the model write it
//! and schedules each message (holds, old-customer gaps, Sundays, the
//! company's instruction). Starting saves the approved messages as a
//! personal "Generated with AI" flow and enrolls the customer; the rep's
//! edits teach the writer their style. Port of `smsFlowAiCadence.server.ts`.

use super::cadence::ramp::{
    PreviewDelayInputs, classify_ai_cadence_mode, first_outbound_at_from_timeline,
    is_email_only_outbound_history, is_established_conversation_customer, is_old_followup_customer,
    last_outbound_at_from_timeline, never_tried_sms_yet, old_customer_uses_thirty_day_gap,
    ramp_outbound_count, resolve_cadence_preview_delay_minutes, should_force_first_message_today,
    stale_conversation_gap_days,
};
use super::cadence::schedule::{
    AiCadenceFlowPreviewStep, build_ai_cadence_flow_steps, plan_ai_followup_cadence,
    planned_steps_violate_hard_hold,
};
use super::cadence::types::{
    AI_CADENCE_FLOW_NAME, AiCadenceMode, AiCadencePreviewMessage, FollowupSituation,
    PlanAiFollowupCadenceOptions, PlannedAiCadenceStep,
};
use super::dates::{Instant, iso};
use super::gather::{FollowupRequest, GatheredContext, gather_followup_context};
use super::generation::{
    CopyGenerationMode, GenerationError, RegenerateFailure, Writer, complete_cadence,
    regenerate_cadence_message,
};
use super::instruction::{
    admin_instruction_gap_minutes, admin_instruction_sends_first_message_today,
};
use super::lifecycle::FollowupGenerationDecision;
use super::pipeline::PipelineTimer;
use super::pressure::build::contact_pressure_for_forced_generate;
use super::pressure::types::{FollowupAngle, FollowupContactPressure, FollowupPhase};
use super::prompt::GeneratedCadenceFollowup;
use super::send_window::{FollowupChannel, StepTiming, followup_delay_minutes_avoiding_sunday};
use super::style::{
    FollowupStyleSample, collect_edited_followup_pairs, compare_followup_edit,
    derive_followup_style_profile, first_name_from_customer_profile,
};
use crate::auth::session::SessionUser;
use crate::routes::sms::flows::{StepChannel, StepInput, insert_personal_flow};
use crate::state::AppState;
use chrono::Duration;
use lambda_http::tracing;
use regex::Regex;
use std::sync::LazyLock;

const MINUTES_PER_DAY: f64 = 24.0 * 60.0;
const STYLE_SAMPLE_LIMIT: i64 = 40;

/// Whose follow-up: the customer, or the phone of an SMS thread.
#[derive(Debug, Clone, Default)]
pub struct CadenceTarget {
    pub customer_id: Option<i32>,
    pub phone_digits: Option<String>,
}

struct Resolved {
    customer_id: Option<i32>,
    phone_digits: Option<String>,
    customer_email: Option<String>,
    channel: FollowupChannel,
    mix_sms_and_email: bool,
}

/// Why nothing was written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CadenceRefusal {
    /// A token: `customer_not_found`, `missing_target`, `invalid_phone`,
    /// `missing_email`.
    Invalid(&'static str),
    /// The customer should not be followed up now (the picker explains it).
    Blocked {
        reason_code: String,
        reason_message: String,
        can_force_generate: bool,
    },
}

/// A written preview.
#[derive(Debug, Clone)]
pub struct CadencePreview {
    pub channel: FollowupChannel,
    pub last_contact_at: Option<String>,
    pub cadence_mode: AiCadenceMode,
    pub preview: Vec<AiCadencePreviewMessage>,
}

fn has_sms_line(user: &SessionUser) -> bool {
    let filled = |value: &Option<String>| value.as_deref().is_some_and(|v| !v.trim().is_empty());
    (filled(&user.cloudtalk_agent_id) && filled(&user.cloudtalk_phone_number))
        || (filled(&user.ringcentral_extension_id) && filled(&user.ringcentral_phone_number))
}

/// SMS when the company texts and the rep has a line (a Telnyx line when
/// they text through Telnyx), else email.
async fn cadence_channel(
    state: &AppState,
    user: &SessionUser,
) -> Result<FollowupChannel, sqlx::Error> {
    let provider =
        crate::routes::sms::send::user_provider(&state.pool, user.company_id, user.id).await?;
    let texts = match provider {
        Some(crate::routes::sms::provider::SmsProvider::Telnyx) => {
            crate::phone::texting::user_line(&state.pool, user.company_id, user.id)
                .await?
                .is_some()
        }
        Some(_) => has_sms_line(user),
        None => false,
    };
    Ok(if texts {
        FollowupChannel::Sms
    } else {
        FollowupChannel::Email
    })
}

async fn resolve(
    state: &AppState,
    user: &SessionUser,
    target: &CadenceTarget,
) -> Result<Result<Resolved, &'static str>, sqlx::Error> {
    use super::enrollments::{canonical_phone10, customer_contact};
    let pool = &state.pool;
    let mut customer_id = target.customer_id;
    let mut phone = target
        .phone_digits
        .as_deref()
        .map(canonical_phone10)
        .filter(|p| p.len() >= 5);
    let mut email = None;
    if let Some(id) = customer_id {
        let Some((customer_phone, customer_email)) =
            customer_contact(pool, user.company_id, id).await?
        else {
            return Ok(Err("customer_not_found"));
        };
        phone = phone.or(customer_phone);
        email = customer_email;
    } else if let Some(digits) = &phone {
        // `fetchCustomerByPhone` of the CloudTalk service, whatever the provider.
        let provider = crate::routes::sms::provider::SmsProvider::Cloudtalk;
        customer_id =
            crate::routes::sms::thread::customer_by_phone(pool, provider, user.company_id, digits)
                .await?
                .map(|c| c.id);
        if let Some(id) = customer_id {
            email = customer_contact(pool, user.company_id, id)
                .await?
                .and_then(|(_, e)| e);
        }
    } else {
        return Ok(Err("missing_target"));
    }
    let mut channel = cadence_channel(state, user).await?;
    if channel == FollowupChannel::Sms && phone.is_none() {
        if email.is_some() {
            channel = FollowupChannel::Email;
        } else {
            return Ok(Err("invalid_phone"));
        }
    }
    if channel == FollowupChannel::Email && email.is_none() {
        return Ok(Err("missing_email"));
    }
    Ok(Ok(Resolved {
        customer_id,
        mix_sms_and_email: channel == FollowupChannel::Sms && email.is_some(),
        phone_digits: phone,
        customer_email: email,
        channel,
    }))
}

fn request_for(
    user: &SessionUser,
    resolved: &Resolved,
    channel: FollowupChannel,
    force: bool,
) -> FollowupRequest {
    let mut request = FollowupRequest::new(user.company_id, user.id, channel);
    request.customer_id = resolved.customer_id;
    request.phone_digits.clone_from(&resolved.phone_digits);
    request.customer_email.clone_from(&resolved.customer_email);
    request.explicit_reenroll = true;
    request.force_generate = force;
    request
}

/// The planned steps for the gathered customer (forced: holds ignored).
fn plan(
    gathered: &GatheredContext,
    resolved: &Resolved,
    force: bool,
    now: Instant,
) -> (Vec<PlannedAiCadenceStep>, Option<Instant>) {
    let timeline: Vec<_> = gathered
        .timeline
        .iter()
        .map(super::prompt::FollowupTimelineMessage::to_timeline)
        .collect();
    let pressure = &gathered.contact_pressure;
    let hold_until_at = if force { None } else { gathered.hold_until() };
    let options = PlanAiFollowupCadenceOptions {
        mix_sms_and_email: resolved.mix_sms_and_email,
        channel: Some(resolved.channel),
        outbound_count: Some(ramp_outbound_count(
            &timeline,
            resolved.mix_sms_and_email,
            Some(resolved.channel),
        )),
        first_outbound_at: first_outbound_at_from_timeline(&timeline),
        hold_until_at,
        customer_requested_followup_at: if force {
            None
        } else {
            pressure.customer_requested_followup_at
        },
        customer_state: Some(pressure.customer_state),
        unanswered_total: Some(pressure.unanswered_total_since_last_reply),
        contact_pressure: Some(if force {
            contact_pressure_for_forced_generate(pressure)
        } else {
            pressure.clone()
        }),
    };
    (
        plan_ai_followup_cadence(last_outbound_at_from_timeline(&timeline), now, &options),
        hold_until_at,
    )
}

static SKIP_WORD: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\bSKIP\b").expect("valid pattern"));

/// "Immediately", "+2 days", "+1 day +3h".
pub fn delay_label_for_model_delay(delay_minutes: f64) -> String {
    if delay_minutes <= 0.0 {
        return "Immediately".into();
    }
    let days = (delay_minutes / MINUTES_PER_DAY).floor();
    let hours = ((delay_minutes - days * MINUTES_PER_DAY) / 60.0).round();
    let mut parts = Vec::new();
    if days > 0.0 {
        parts.push(format!(
            "+{days} day{}",
            if (days - 1.0).abs() < f64::EPSILON {
                ""
            } else {
                "s"
            }
        ));
    }
    if hours > 0.0 {
        parts.push(format!("+{hours}h"));
    }
    if parts.is_empty() {
        "Immediately".into()
    } else {
        parts.join(" ")
    }
}

/// How the preview's waits are decided.
#[derive(Debug, Clone, Default)]
struct ScheduleOptions {
    floor_to_planned_delay: bool,
    allow_sunday_on: Option<Instant>,
    instruction_gap_minutes: Option<f64>,
    widen_gap_for_old_customer: bool,
    use_thirty_day_gap: bool,
    stale_gap_days: Option<Vec<f64>>,
    send_first_message_today: bool,
    days_since_last_contact: Option<f64>,
}

/// Days since the last message either way; `None` when the customer asked
/// for a contact date (that date wins).
fn days_since_last_contact(pressure: &FollowupContactPressure) -> Option<f64> {
    if pressure.customer_requested_followup_at.is_some() {
        return None;
    }
    [
        pressure.days_since_last_outbound,
        pressure.days_since_last_customer_reply,
    ]
    .into_iter()
    .flatten()
    .filter(|d| d.is_finite())
    .reduce(f64::min)
}

fn schedule_options(gathered: &GatheredContext, force: bool, now: Instant) -> ScheduleOptions {
    let pressure = Some(&gathered.contact_pressure);
    #[allow(clippy::cast_precision_loss)]
    let instruction_gap_minutes =
        admin_instruction_gap_minutes(&gathered.admin_instruction).map(|m| m as f64);
    ScheduleOptions {
        floor_to_planned_delay: is_established_conversation_customer(pressure),
        widen_gap_for_old_customer: is_old_followup_customer(pressure),
        use_thirty_day_gap: old_customer_uses_thirty_day_gap(
            pressure,
            Some(&gathered.profile),
            now,
        ),
        stale_gap_days: stale_conversation_gap_days(pressure),
        send_first_message_today: admin_instruction_sends_first_message_today(
            &gathered.admin_instruction,
        ),
        instruction_gap_minutes,
        days_since_last_contact: days_since_last_contact(&gathered.contact_pressure),
        allow_sunday_on: if force {
            None
        } else {
            gathered.contact_pressure.customer_requested_followup_at
        },
    }
}

fn with_delay(message: &AiCadencePreviewMessage, delay_minutes: f64) -> AiCadencePreviewMessage {
    AiCadencePreviewMessage {
        delay_minutes,
        delay_label: delay_label_for_model_delay(delay_minutes),
        ..message.clone()
    }
}

/// A hold is a floor: the first send never goes out earlier.
fn apply_hold_floor(
    preview: Vec<AiCadencePreviewMessage>,
    hold_until: Option<Instant>,
    now: Instant,
) -> Vec<AiCadencePreviewMessage> {
    let Some(hold) = hold_until else {
        return preview;
    };
    #[allow(clippy::cast_precision_loss)]
    let needed = (((hold - now).num_milliseconds() as f64) / 60_000.0)
        .ceil()
        .max(0.0);
    match preview.first() {
        Some(first) if needed > 0.0 && first.delay_minutes < needed => {
            let mut floored = preview.clone();
            floored[0] = with_delay(first, needed);
            floored
        }
        _ => preview,
    }
}

/// The generated messages as the picker shows them, each with its wait
/// (`previewFromGenerated`).
fn preview_from_generated(
    planned: &[PlannedAiCadenceStep],
    messages: &[GeneratedCadenceFollowup],
    hold: Option<Instant>,
    now: Instant,
    options: &ScheduleOptions,
) -> Vec<AiCadencePreviewMessage> {
    if messages.is_empty() {
        return Vec::new();
    }
    let send_first_today = options.send_first_message_today || options.use_thirty_day_gap;
    let mut index = 0usize;
    let preview: Vec<AiCadencePreviewMessage> = planned
        .iter()
        .zip(messages.iter().map(Some).chain(std::iter::repeat(None)))
        .filter_map(|(step, generated)| {
            let generated = generated.filter(|g| !g.skip && !SKIP_WORD.is_match(&g.body))?;
            #[allow(clippy::cast_precision_loss)]
            let delay_minutes = resolve_cadence_preview_delay_minutes(&PreviewDelayInputs {
                generated_delay_minutes: generated.delay_minutes,
                planned_delay_minutes: step.delay_minutes,
                floor_to_planned_delay: options.floor_to_planned_delay,
                instruction_gap_minutes: options.instruction_gap_minutes,
                widen_gap_for_old_customer: options.widen_gap_for_old_customer,
                use_thirty_day_gap: options.use_thirty_day_gap,
                is_first_message: index == 0,
                send_first_message_today: send_first_today,
                stale_gap_days: options.stale_gap_days.clone(),
                message_index: Some(index as f64),
                days_since_last_contact: options.days_since_last_contact,
            });
            index += 1;
            Some(AiCadencePreviewMessage {
                delay_minutes,
                delay_label: delay_label_for_model_delay(delay_minutes),
                goal: step.goal.clone(),
                channel: step.channel,
                skip: false,
                skip_reason: None,
                body: generated.body.clone(),
                subject: generated.subject.clone(),
                generated_body: Some(generated.body.clone()),
                generated_subject: Some(generated.subject.clone()),
                angle: Some(step.angle.as_str().to_string()),
                phase: step.phase.map(|p| p.as_str().to_string()),
            })
        })
        .collect();
    let locks_gap = options
        .instruction_gap_minutes
        .is_some_and(|g| g.is_finite() && g >= 0.0);
    let floored = if locks_gap {
        preview
    } else {
        apply_hold_floor(preview, hold, now)
    };
    let scheduled: Vec<AiCadencePreviewMessage> = if locks_gap {
        floored
    } else {
        let timings: Vec<StepTiming> = floored
            .iter()
            .map(|m| StepTiming {
                delay_minutes: m.delay_minutes,
                channel: m.channel,
            })
            .collect();
        let delays = followup_delay_minutes_avoiding_sunday(&timings, now, options.allow_sunday_on);
        floored
            .iter()
            .zip(delays.iter().map(Some).chain(std::iter::repeat(None)))
            .map(|(message, delay)| match delay {
                Some(delay) if (*delay - message.delay_minutes).abs() > f64::EPSILON => {
                    with_delay(message, *delay)
                }
                _ => message.clone(),
            })
            .collect()
    };
    let force_today = should_force_first_message_today(
        options.send_first_message_today,
        options.use_thirty_day_gap,
        hold,
        now,
    );
    match scheduled.first() {
        Some(first) if force_today && first.delay_minutes != 0.0 => {
            let mut today = scheduled.clone();
            today[0] = with_delay(first, 0.0);
            today
        }
        _ => scheduled,
    }
}

/// The day each message goes out under the preview's schedule, so the copy
/// never names a day that will have passed.
fn with_expected_send_dates(
    planned: &[PlannedAiCadenceStep],
    hold: Option<Instant>,
    options: &ScheduleOptions,
    now: Instant,
) -> Vec<PlannedAiCadenceStep> {
    let placeholders: Vec<GeneratedCadenceFollowup> = planned
        .iter()
        .map(|_| GeneratedCadenceFollowup {
            skip: false,
            reason: None,
            body: "-".into(),
            subject: None,
            delay_minutes: None,
        })
        .collect();
    let schedule = preview_from_generated(planned, &placeholders, hold, now, options);
    let mut elapsed = 0.0;
    planned
        .iter()
        .enumerate()
        .map(|(index, step)| {
            elapsed += schedule
                .get(index)
                .map_or(step.delay_minutes, |m| m.delay_minutes);
            #[allow(clippy::cast_possible_truncation)]
            let send_on = now + Duration::milliseconds((elapsed * 60_000.0).round() as i64);
            PlannedAiCadenceStep {
                expected_send_on: Some(iso(send_on)[..10].to_string()),
                ..step.clone()
            }
        })
        .collect()
}

fn block_evidence(gathered: &GatheredContext) -> Option<String> {
    let trimmed = |s: &str| Some(s.trim().to_string()).filter(|s| !s.is_empty());
    gathered
        .contact_pressure
        .latest_meaningful_customer_intent
        .as_ref()
        .and_then(|intent| trimmed(&intent.evidence))
        .or_else(|| {
            gathered
                .crm_note_hold
                .as_ref()
                .and_then(|h| trimmed(&h.reason))
        })
        .or_else(|| gathered.crm_hold.as_ref().and_then(|h| trimmed(&h.reason)))
}

/// The picker's explanation of a block (`smsFlowGenerationBlock.ts`).
pub fn friendly_generation_block_message(reason_code: &str, evidence: Option<&str>) -> String {
    let code = Some(reason_code.trim())
        .filter(|c| !c.is_empty())
        .unwrap_or("NOTHING_TO_SEND");
    let nothing = "There is nothing left to send in this follow-up sequence.";
    let mapped = match code {
        "CUSTOMER_WILL_RECONTACT" => {
            "This customer said they will reach out when ready, so automated follow-ups are paused until they contact you."
        }
        "CUSTOMER_REQUESTED_NO_TEXT_WAITING_FOR_CUSTOMER" => {
            "This customer asked not to be texted and said they will reach out — automated contact is paused."
        }
        "CUSTOMER_WAITING_FOR_HUMAN_REPLY" => {
            "They are waiting for a human reply. Automated follow-ups stay off until someone responds manually."
        }
        "STOPPED_CUSTOMER_REPLIED" => {
            "The customer already replied. Start a new sequence only if you intentionally want to follow up again."
        }
        "STOPPED_OPT_OUT" => {
            "This customer opted out of messaging. Automated follow-ups cannot be sent."
        }
        "TARGET_DEAL_CLOSED_WON" => {
            "The target deal is closed/won, so automated follow-ups are not generated."
        }
        "TARGET_DEAL_CLOSED_LOST" => {
            "The target deal is closed/lost, so automated follow-ups are not generated."
        }
        "CUSTOMER_DECLINED" => {
            "This customer declined further contact, so automated follow-ups are not generated."
        }
        "CUSTOMER_MOVED_FORWARD_ELSEWHERE" => {
            "This customer moved forward with someone else, so automated follow-ups are not generated."
        }
        "NO_ALLOWED_AUTOMATED_CONTACT" => {
            "No allowed SMS or email channel is available for automated contact."
        }
        "HARD_HOLD_ACTIVE" => {
            "CRM notes or activities say not to contact them yet, so no follow-up schedule was built."
        }
        "NOTHING_TO_SEND" => nothing,
        c if c.starts_with("CUSTOMER_INTENT_") => {
            "Customer intent indicates automated follow-ups should stop."
        }
        c if c.starts_with("VALIDATION_FAILED:") => {
            "Follow-up generation was blocked by validation."
        }
        _ => nothing,
    };
    let Some(detail) = evidence.map(str::trim).filter(|d| !d.is_empty()) else {
        return mapped.to_string();
    };
    let units: Vec<u16> = detail.encode_utf16().collect();
    let clipped = if units.len() > 180 {
        format!("{}…", String::from_utf16_lossy(&units[..180]))
    } else {
        detail.to_string()
    };
    format!("{mapped} Note: “{clipped}”")
}

/// Whether "Generate anyway" may override the block.
pub fn can_force_generate_despite_block(reason_code: &str) -> bool {
    reason_code != "STOPPED_OPT_OUT" && !reason_code.starts_with("VALIDATION_FAILED:")
}

fn blocked(reason_code: &str, gathered: &GatheredContext) -> CadenceRefusal {
    CadenceRefusal::Blocked {
        reason_message: friendly_generation_block_message(
            reason_code,
            block_evidence(gathered).as_deref(),
        ),
        can_force_generate: can_force_generate_despite_block(reason_code),
        reason_code: reason_code.to_string(),
    }
}

/// Plans and writes the follow-up for the picker.
pub async fn preview(
    state: &AppState,
    user: &SessionUser,
    target: &CadenceTarget,
    force: bool,
    now: Instant,
) -> Result<Result<CadencePreview, CadenceRefusal>, GenerationError> {
    let resolved = match resolve(state, user, target).await? {
        Ok(resolved) => resolved,
        Err(token) => return Ok(Err(CadenceRefusal::Invalid(token))),
    };
    let request = request_for(user, &resolved, resolved.channel, force);
    let gathered = gather_followup_context(&state.pool, &request, now).await?;
    let (planned, hold) = plan(&gathered, &resolved, force, now);
    if let FollowupGenerationDecision::DoNotGenerate { reason } = &gathered.generation_decision
        && !force
    {
        return Ok(Err(blocked(reason, &gathered)));
    }
    if planned.is_empty() {
        let code = if hold.is_some() {
            "HARD_HOLD_ACTIVE"
        } else {
            "NOTHING_TO_SEND"
        };
        return Ok(Err(blocked(code, &gathered)));
    }
    if !force && planned_steps_violate_hard_hold(&planned, hold, now) {
        return Ok(Err(blocked("HARD_HOLD_ACTIVE", &gathered)));
    }
    let options = schedule_options(&gathered, force, now);
    let steps = with_expected_send_dates(&planned, hold, &options, now);
    let mode = match CopyGenerationMode::resolve(None) {
        CopyGenerationMode::Shadow => CopyGenerationMode::Legacy,
        mode => mode,
    };
    let writer = Writer {
        pool: &state.pool,
        ai: &state.ai,
        company_id: user.company_id,
        user_id: user.id,
    };
    let mut timer = PipelineTimer::default();
    let messages =
        complete_cadence(writer, &request, &gathered, &steps, mode, now, &mut timer).await?;
    let preview = preview_from_generated(&planned, &messages, hold, now, &options);
    if preview.is_empty() {
        return Ok(Err(blocked("NOTHING_TO_SEND", &gathered)));
    }
    tracing::info!(
        operation = "preview",
        company_id = user.company_id,
        requested = timer.followups_requested,
        retry_used = timer.retry_used,
        "ai follow-up cadence written"
    );
    let timeline: Vec<_> = gathered
        .timeline
        .iter()
        .map(super::prompt::FollowupTimelineMessage::to_timeline)
        .collect();
    let last_contact_at = last_outbound_at_from_timeline(&timeline);
    let first_outbound_at = first_outbound_at_from_timeline(&timeline);
    let outbound_count = ramp_outbound_count(
        &timeline,
        resolved.mix_sms_and_email,
        Some(resolved.channel),
    );
    #[allow(clippy::cast_precision_loss)]
    let days = |at: Instant| (now - at).num_milliseconds() as f64 / (MINUTES_PER_DAY * 60_000.0);
    let days_since = last_contact_at.map(days);
    let never_tried_sms = never_tried_sms_yet(
        outbound_count,
        resolved.mix_sms_and_email,
        Some(resolved.channel),
        days_since,
    ) || (resolved.channel != FollowupChannel::Email
        && is_email_only_outbound_history(&timeline));
    let chase_days = first_outbound_at.or(last_contact_at).map(days);
    Ok(Ok(CadencePreview {
        channel: resolved.channel,
        last_contact_at: last_contact_at.map(iso),
        cadence_mode: classify_ai_cadence_mode(
            outbound_count,
            chase_days,
            days_since,
            never_tried_sms,
        ),
        preview,
    }))
}

/// Saves what the rep changed in the drafts and refreshes their style
/// profile (`recordAiFollowupEdits`).
async fn record_edits(
    state: &AppState,
    user: &SessionUser,
    gathered: &GatheredContext,
    flow_id: i32,
    preview: &[AiCadencePreviewMessage],
) -> Result<(), sqlx::Error> {
    let pool = &state.pool;
    let first_name = first_name_from_customer_profile(&gathered.profile);
    let pairs = collect_edited_followup_pairs(preview, first_name.as_deref());
    if pairs.is_empty() {
        return Ok(());
    }
    let steps: Vec<(i32, Option<String>)> = sqlx::query_as(
        "SELECT id, custom_text FROM sms_flow_steps WHERE flow_id = ? ORDER BY position ASC",
    )
    .bind(flow_id)
    .fetch_all(pool)
    .await?;
    let mut matched = std::collections::HashSet::new();
    for pair in &pairs {
        let body = pair
            .final_text
            .find("\n\n")
            .map_or(pair.final_text.as_str(), |at| &pair.final_text[at + 2..]);
        let followup_id = steps
            .iter()
            .find(|(id, text)| {
                !matched.contains(id) && text.as_deref().unwrap_or("").trim() == body.trim()
            })
            .map(|(id, _)| *id);
        if let Some(id) = followup_id {
            matched.insert(id);
        }
        sqlx::query(
            "INSERT INTO ai_followup_edit_pairs
               (user_id, company_id, deal_id, followup_id, channel, generated_text, final_text, angle, phase, customer_first_name)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(user.id)
        .bind(user.company_id)
        .bind(gathered.contact_pressure.target_deal_id)
        .bind(followup_id)
        .bind(pair.channel.as_str())
        .bind(&pair.generated_text)
        .bind(&pair.final_text)
        .bind(&pair.angle)
        .bind(&pair.phase)
        .bind(&pair.customer_first_name)
        .execute(pool)
        .await?;
    }
    let rows: Vec<(String, String, String, Option<String>)> = sqlx::query_as(
        "SELECT generated_text, final_text, CAST(channel AS CHAR), customer_first_name
           FROM ai_followup_edit_pairs
          WHERE user_id = ? AND company_id = ?
            -- Compose edits feed the rep's notes, not these stats.
            AND (phase IS NULL OR phase != 'compose')
          ORDER BY id DESC LIMIT ?",
    )
    .bind(user.id)
    .bind(user.company_id)
    .bind(STYLE_SAMPLE_LIMIT)
    .fetch_all(pool)
    .await?;
    let samples: Vec<FollowupStyleSample> = rows
        .iter()
        .map(
            |(generated, final_text, channel, first_name)| FollowupStyleSample {
                channel: if channel == "email" {
                    FollowupChannel::Email
                } else {
                    FollowupChannel::Sms
                },
                comparison: compare_followup_edit(generated, final_text, first_name.as_deref()),
            },
        )
        .collect();
    let mut profile = derive_followup_style_profile(&samples);
    // The rep's notes come from compose edits; keep them.
    if let Some(notes) = super::gather::load_style_profile(pool, user.id, user.company_id)
        .await?
        .and_then(|stored| stored.notes)
    {
        profile.notes = Some(notes);
    }
    let json = serde_json::to_string(&profile).unwrap_or_else(|_| "{}".into());
    sqlx::query(
        "INSERT INTO ai_followup_style_preferences (user_id, company_id, preferences_json, sample_count)
         VALUES (?, ?, ?, ?)
         ON DUPLICATE KEY UPDATE preferences_json = VALUES(preferences_json), sample_count = VALUES(sample_count),
                                 updated_at = UTC_TIMESTAMP()",
    )
    .bind(user.id)
    .bind(user.company_id)
    .bind(json)
    .bind(i64::try_from(rows.len()).unwrap_or_default())
    .execute(pool)
    .await?;
    Ok(())
}

/// Saves the approved messages as a flow and enrolls the customer
/// (`switch`: replacing the running flow). `Ok(true)` when the first
/// message is due now.
pub async fn start(
    state: &AppState,
    user: &SessionUser,
    target: &CadenceTarget,
    preview: &[AiCadencePreviewMessage],
    switch: bool,
    force: bool,
    now: Instant,
) -> Result<Result<bool, &'static str>, sqlx::Error> {
    let resolved = match resolve(state, user, target).await? {
        Ok(resolved) => resolved,
        Err(token) => return Ok(Err(token)),
    };
    let request = request_for(user, &resolved, resolved.channel, force);
    let gathered = gather_followup_context(&state.pool, &request, now).await?;
    if matches!(
        gathered.generation_decision,
        FollowupGenerationDecision::DoNotGenerate { .. }
    ) && !force
    {
        return Ok(Err("nothing_to_send"));
    }
    let (planned, _) = plan(&gathered, &resolved, force, now);
    let approved: Vec<AiCadenceFlowPreviewStep> = preview
        .iter()
        .map(|message| AiCadenceFlowPreviewStep {
            skip: message.skip,
            body: message.body.clone(),
            subject: message.subject.clone(),
            delay_minutes: Some(message.delay_minutes),
            channel: Some(message.channel),
        })
        .collect();
    let steps: Vec<StepInput> =
        build_ai_cadence_flow_steps(&planned, &approved, Some(resolved.channel))
            .into_iter()
            .map(|step| StepInput {
                channel: if step.channel == FollowupChannel::Email {
                    StepChannel::Email
                } else {
                    StepChannel::Sms
                },
                template_id: step.template_id.and_then(|id| i32::try_from(id).ok()),
                custom_text: step.custom_text,
                subject: step.subject,
                #[allow(clippy::cast_possible_truncation)]
                delay_minutes: step.delay_minutes.round() as i32,
            })
            .collect();
    if steps.is_empty() {
        return Ok(Err("nothing_to_send"));
    }
    let flow_id = insert_personal_flow(&state.pool, user, AI_CADENCE_FLOW_NAME, &steps).await?;
    // Learning never blocks starting a follow-up.
    if let Err(err) = record_edits(state, user, &gathered, flow_id, preview).await {
        tracing::warn!(?err, flow_id, "ai follow-up edits not recorded");
    }
    // The rep's own line, as their sends use it (not the company's first).
    let provider = super::processor::rep_sms_provider(state, user.company_id, user).await?;
    super::enrollments::start(
        &state.pool,
        user,
        provider,
        flow_id,
        resolved.phone_digits.as_deref(),
        resolved.customer_id,
        switch,
    )
    .await
}

/// One preview message to rewrite.
#[derive(Debug, Clone)]
pub struct RegenerateMessage {
    pub delay_minutes: f64,
    pub delay_label: String,
    pub goal: String,
    pub channel: FollowupChannel,
    pub angle: Option<String>,
    pub phase: Option<String>,
    pub avoid_body: String,
    pub avoid_subject: Option<String>,
    pub avoid_previous: Vec<String>,
    pub force_generate: bool,
}

/// Rewrites one preview message so it differs from the drafts the rep
/// turned down; `(body, subject)` or a token.
pub async fn regenerate(
    state: &AppState,
    user: &SessionUser,
    target: &CadenceTarget,
    message: &RegenerateMessage,
    now: Instant,
) -> Result<Result<(String, Option<String>), &'static str>, GenerationError> {
    let resolved = match resolve(state, user, target).await? {
        Ok(resolved) => resolved,
        Err(token) => return Ok(Err(token)),
    };
    let request = request_for(user, &resolved, message.channel, message.force_generate);
    let gathered = gather_followup_context(&state.pool, &request, now).await?;
    let angle = message
        .angle
        .as_deref()
        .and_then(FollowupAngle::parse)
        .unwrap_or(FollowupAngle::HelpfulInformation);
    let step = PlannedAiCadenceStep {
        #[allow(clippy::cast_possible_truncation)]
        offset_days: (message.delay_minutes / MINUTES_PER_DAY).round().max(0.0),
        delay_label: message.delay_label.clone(),
        situation: FollowupSituation::StalledConversation,
        goal: message.goal.clone(),
        purpose: message.goal.clone(),
        angle,
        subangle: None,
        phase: message.phase.as_deref().and_then(FollowupPhase::parse),
        semantic_objective: None,
        delay_minutes: message.delay_minutes,
        channel: message.channel,
        manual_action: None,
        original_step_index: None,
        preflight_status: None,
        preflight_reason: None,
        expected_send_on: None,
    };
    let writer = Writer {
        pool: &state.pool,
        ai: &state.ai,
        company_id: user.company_id,
        user_id: user.id,
    };
    Ok(regenerate_cadence_message(
        writer,
        &gathered,
        &step,
        &message.avoid_body,
        message.avoid_subject.as_deref(),
        &message.avoid_previous,
        message.force_generate,
        now,
    )
    .await?
    .map_err(RegenerateFailure::as_str))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delay_labels_read_like_the_crm() {
        assert_eq!(delay_label_for_model_delay(0.0), "Immediately");
        assert_eq!(delay_label_for_model_delay(20.0), "Immediately");
        assert_eq!(delay_label_for_model_delay(90.0), "+2h");
        assert_eq!(delay_label_for_model_delay(1440.0), "+1 day");
        assert_eq!(
            delay_label_for_model_delay(3.0 * 1440.0 + 180.0),
            "+3 days +3h"
        );
    }

    #[test]
    fn block_messages_explain_and_quote_the_evidence() {
        assert_eq!(
            friendly_generation_block_message("STOPPED_OPT_OUT", None),
            "This customer opted out of messaging. Automated follow-ups cannot be sent."
        );
        assert_eq!(
            friendly_generation_block_message("CUSTOMER_INTENT_PAUSE", Some(" call me in May ")),
            "Customer intent indicates automated follow-ups should stop. Note: “call me in May”"
        );
        assert_eq!(
            friendly_generation_block_message("", None),
            "There is nothing left to send in this follow-up sequence."
        );
        let long = "x".repeat(200);
        assert!(friendly_generation_block_message("HARD_HOLD_ACTIVE", Some(&long)).ends_with("…”"));
        assert!(!can_force_generate_despite_block("STOPPED_OPT_OUT"));
        assert!(!can_force_generate_despite_block("VALIDATION_FAILED:x"));
        assert!(can_force_generate_despite_block("HARD_HOLD_ACTIVE"));
    }
}
