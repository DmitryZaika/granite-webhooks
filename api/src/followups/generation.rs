//! Writing follow-ups with the model.
//!
//! One flow message, a whole cadence (the `LEGACY` prompt or the
//! `NEW_CONTEXT` model context, with one repair pass for copy that fails the
//! checks), and rewriting one cadence message (port of the generation parts
//! of `smsFlowAiFollowup.server.ts` and `aiFollowupRefactoredGeneration.server.ts`).

use super::cadence::ramp::{
    PreviewDelayInputs, is_established_conversation_customer, resolve_cadence_preview_delay_minutes,
};
use super::cadence::types::PlannedAiCadenceStep;
use super::dates::Instant;
use super::gather::{FollowupRequest, GatheredContext};
use super::instruction::admin_instruction_gap_minutes;
use super::js::{js_slice, trim};
use super::lifecycle::FollowupGenerationDecision;
use super::model_context::{ModelContextSource, build_cadence_model_context};
use super::pipeline::{
    CopyFailure, LlmCallMetrics, PipelineTimer, PromptParts, prompt_component_metrics,
};
use super::pressure::copy::{
    followup_tone_errors, validate_generated_followup_copy,
    validate_generated_followup_copy_for_send_at,
};
use super::pressure::format::format_contact_pressure_block;
use super::prompt::{
    CadencePromptContext, FollowupPromptContext, GeneratedCadenceFollowup, GeneratedFollowup,
    build_cadence_followup_system_prompt, build_followup_system_prompt, build_followup_user_prompt,
    cadence_has_writable_copy, format_followup_template_block, format_followup_timeline_block,
    parse_cadence_followup_generation, parse_followup_generation, strip_followup_email_signature,
};
use super::prompt_texts::{FORCE_GENERATE_CADENCE_INSTRUCTION, FORCE_GENERATE_RETRY_INSTRUCTION};
use super::refactored_prompt::{
    AI_FOLLOWUP_PROMPT, build_refactored_cadence_prompt,
    build_refactored_cadence_repair_user_prompt,
};
use super::send_window::{FollowupChannel, StepTiming, expected_followup_send_ats};
use super::style::format_style_preference_prompt_block;
use crate::ai::{Ai, AiError, Caller, Chat, ChatReply};
use crate::re;
use sqlx::MySqlPool;
use std::time::Duration;

/// The follow-up model (`AI_FOLLOWUP_MODEL`).
pub const AI_FOLLOWUP_MODEL: &str = "gpt-6.1-sol";
/// Its reasoning effort (it rejects `none`).
pub const AI_FOLLOWUP_REASONING_EFFORT: &str = "low";
/// Longest text the SMS providers take.
pub const SMS_MAX_TEXT: usize = 1600;
const PHONE_CALL: &str = "PHONE_CALL";

/// Which prompt writes a cadence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CopyGenerationMode {
    Legacy,
    NewContext,
    /// Legacy copy; the CRM also ran the new prompt for comparison (not here).
    Shadow,
}

impl CopyGenerationMode {
    pub fn parse(value: &str) -> Option<Self> {
        match trim(value).to_uppercase().replace('-', "_").as_str() {
            "NEW" | "NEW_CONTEXT" => Some(Self::NewContext),
            "LEGACY" => Some(Self::Legacy),
            "SHADOW" => Some(Self::Shadow),
            _ => None,
        }
    }

    /// The explicit mode, else `AI_FOLLOWUP_COPY_MODE`, else `LEGACY`.
    pub fn resolve(explicit: Option<Self>) -> Self {
        explicit
            .or_else(|| {
                std::env::var("AI_FOLLOWUP_COPY_MODE")
                    .ok()
                    .as_deref()
                    .and_then(Self::parse)
            })
            .unwrap_or(Self::Legacy)
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Legacy => "LEGACY",
            Self::NewContext => "NEW_CONTEXT",
            Self::Shadow => "SHADOW",
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum GenerationError {
    #[error(transparent)]
    Ai(#[from] AiError),
    /// The model's reply was not the JSON asked for.
    #[error("ai_followup_parse_failed")]
    Parse,
    /// Reading the customer's context failed.
    #[error(transparent)]
    Database(#[from] sqlx::Error),
}

/// Where calls are billed and the clock sends are scheduled from.
#[derive(Clone, Copy)]
pub struct Writer<'a> {
    pub pool: &'a MySqlPool,
    pub ai: &'a Ai,
    pub company_id: i32,
    pub user_id: i32,
}

impl Writer<'_> {
    async fn complete(&self, system: &str, user: &str) -> Result<ChatReply, AiError> {
        let mut chat = Chat::new(AI_FOLLOWUP_MODEL)
            .message("system", system)
            .message("user", user);
        chat.json = true;
        chat.reasoning_effort = Some(AI_FOLLOWUP_REASONING_EFFORT);
        chat.timeout = Duration::from_secs(120);
        self.ai
            .chat_reply(
                self.pool,
                Caller {
                    feature: "ai_followup",
                    user_id: Some(self.user_id),
                    company_id: Some(self.company_id),
                },
                &chat,
            )
            .await
    }
}

fn sms_text(body: &str) -> String {
    js_slice(body, 0, SMS_MAX_TEXT)
}

/// One flow message, or why not.
pub async fn complete_followup(
    writer: Writer<'_>,
    request: &FollowupRequest,
    gathered: &GatheredContext,
) -> Result<GeneratedFollowup, GenerationError> {
    if let FollowupGenerationDecision::DoNotGenerate { reason } = &gathered.generation_decision {
        return Ok(GeneratedFollowup::Skip {
            skip: true,
            reason: reason.clone(),
        });
    }
    let user_prompt = build_followup_user_prompt(&FollowupPromptContext {
        channel: request.channel,
        step_position: request.step_position,
        situation: gathered.situation,
        customer_profile: gathered.profile.clone(),
        company_instructions: gathered.company_instructions.clone(),
        admin_instruction: Some(gathered.admin_instruction.clone()),
        templates: gathered.templates.clone(),
        timeline: gathered.timeline.clone(),
        now: None,
        crm_hold: gathered.crm_hold_for_prompt(),
        contact_pressure: Some(gathered.contact_pressure.clone()),
        style_profile: gathered.style_profile.clone(),
        rep_voice: gathered.rep_voice.examples.clone(),
        force_generate: false,
    });
    let reply = writer
        .complete(&build_followup_system_prompt(), &user_prompt)
        .await?;
    match parse_followup_generation(&reply.text, request.channel).ok_or(GenerationError::Parse)? {
        GeneratedFollowup::Message { body, subject, .. } => Ok(GeneratedFollowup::Message {
            skip: false,
            body: if request.channel == FollowupChannel::Sms {
                sms_text(&body)
            } else {
                strip_followup_email_signature(&body)
            },
            subject: (request.channel == FollowupChannel::Email)
                .then(|| js_slice(subject.as_deref().unwrap_or("Following up"), 0, 300)),
        }),
        skip @ GeneratedFollowup::Skip { .. } => Ok(skip),
    }
}

/// A generated message as it is stored: SMS cut to the provider limit,
/// email without a sign-off, a placeholder `SKIP` dropped.
fn stored_message(
    generated: Option<&GeneratedCadenceFollowup>,
    channel: FollowupChannel,
    drop_placeholder: bool,
) -> GeneratedCadenceFollowup {
    let Some(generated) = generated else {
        return GeneratedCadenceFollowup::skipped("skip");
    };
    if generated.skip {
        return generated.clone();
    }
    if drop_placeholder && re!(r"\bSKIP\b", "i").is_match(&generated.body) {
        return GeneratedCadenceFollowup::skipped("placeholder");
    }
    GeneratedCadenceFollowup {
        skip: false,
        reason: None,
        body: if channel == FollowupChannel::Sms {
            sms_text(&generated.body)
        } else {
            strip_followup_email_signature(&generated.body)
        },
        subject: (channel == FollowupChannel::Email).then(|| {
            js_slice(
                generated.subject.as_deref().unwrap_or("Following up"),
                0,
                300,
            )
        }),
        delay_minutes: generated.delay_minutes,
    }
}

fn writable(steps: &[PlannedAiCadenceStep]) -> Vec<&PlannedAiCadenceStep> {
    steps
        .iter()
        .filter(|s| s.manual_action != Some(PHONE_CALL))
        .collect()
}

/// Generated messages back onto all planned steps (phone-call steps skip).
fn onto_planned_steps(
    steps: &[PlannedAiCadenceStep],
    parsed: &[GeneratedCadenceFollowup],
    drop_placeholder: bool,
) -> Vec<GeneratedCadenceFollowup> {
    let mut index = 0;
    steps
        .iter()
        .map(|step| {
            if step.manual_action == Some(PHONE_CALL) {
                return GeneratedCadenceFollowup::skipped("manual_phone_call");
            }
            let generated = parsed.get(index);
            index += 1;
            stored_message(generated, step.channel, drop_placeholder)
        })
        .collect()
}

/// When each writable message would go out, with the model's waits.
fn send_ats(
    writable: &[&PlannedAiCadenceStep],
    parsed: &[GeneratedCadenceFollowup],
    gathered: &GatheredContext,
    now: Instant,
) -> Vec<Instant> {
    let floor = is_established_conversation_customer(Some(&gathered.contact_pressure));
    #[allow(clippy::cast_precision_loss)]
    let gap = admin_instruction_gap_minutes(&gathered.admin_instruction).map(|g| g as f64);
    let steps: Vec<StepTiming> = writable
        .iter()
        .enumerate()
        .map(|(index, step)| {
            let generated = parsed.get(index).filter(|g| !g.skip);
            StepTiming {
                delay_minutes: resolve_cadence_preview_delay_minutes(&PreviewDelayInputs {
                    generated_delay_minutes: generated.and_then(|g| g.delay_minutes),
                    planned_delay_minutes: step.delay_minutes,
                    floor_to_planned_delay: floor,
                    instruction_gap_minutes: gap,
                    ..PreviewDelayInputs::default()
                }),
                channel: step.channel,
            }
        })
        .collect();
    expected_followup_send_ats(&steps, now)
}

/// Copy checks of the first pass: the failing message indexes and reasons.
fn copy_failures(
    parsed: &[GeneratedCadenceFollowup],
    count: usize,
    gathered: &GatheredContext,
    sends: &[Instant],
) -> (Vec<usize>, Vec<String>) {
    let mut rejected = Vec::new();
    let mut reasons = Vec::new();
    for index in 0..count {
        let Some(generated) = parsed
            .get(index)
            .filter(|g| !g.skip && !trim(&g.body).is_empty())
        else {
            continue;
        };
        let errors = sends.get(index).map_or_else(
            || validate_generated_followup_copy(&generated.body, &gathered.contact_pressure),
            |send_at| {
                validate_generated_followup_copy_for_send_at(
                    &generated.body,
                    &gathered.contact_pressure,
                    *send_at,
                )
            },
        );
        if !errors.is_empty() {
            rejected.push(index);
            reasons.extend(
                errors
                    .into_iter()
                    .map(|error| format!("Message {}: {error}", index + 1)),
            );
        }
    }
    (rejected, reasons)
}

fn rule_category(error: &str) -> &'static str {
    if re!(r"send date|contact-window|timeline copy", "i").is_match(error) {
        "send_time"
    } else if re!(
        r"unsupported|inconsistency|invent|without known|product education",
        "i"
    )
    .is_match(error)
    {
        "factuality"
    } else {
        "copy"
    }
}

/// A whole cadence (empty when not to write). `steps` are the planned steps;
/// phone-call steps come back as skips.
#[allow(clippy::too_many_lines)]
pub async fn complete_cadence(
    writer: Writer<'_>,
    request: &FollowupRequest,
    gathered: &GatheredContext,
    steps: &[PlannedAiCadenceStep],
    mode: CopyGenerationMode,
    now: Instant,
    timer: &mut PipelineTimer,
) -> Result<Vec<GeneratedCadenceFollowup>, GenerationError> {
    if steps.is_empty() {
        return Ok(Vec::new());
    }
    if matches!(
        gathered.generation_decision,
        FollowupGenerationDecision::DoNotGenerate { .. }
    ) && !request.force_generate
    {
        return Ok(Vec::new());
    }
    let gathered = if request.force_generate {
        gathered.for_forced_generate()
    } else {
        gathered.clone()
    };
    if mode == CopyGenerationMode::NewContext {
        let messages =
            complete_cadence_new_context(writer, request, &gathered, steps, now, timer).await?;
        timer.followups_generated = messages
            .iter()
            .filter(|m| !m.skip && !trim(&m.body).is_empty())
            .count();
        timer.mark("post_processing");
        return Ok(messages);
    }
    let writable_steps = writable(steps);
    timer.followups_requested = writable_steps.len();
    if writable_steps.is_empty() {
        return Ok(steps
            .iter()
            .map(|_| GeneratedCadenceFollowup::skipped("manual_phone_call"))
            .collect());
    }
    let channels: Vec<FollowupChannel> = writable_steps.iter().map(|s| s.channel).collect();
    let system_prompt = build_cadence_followup_system_prompt(request.force_generate);
    let user_prompt = super::prompt::build_cadence_followup_user_prompt_at(
        &CadencePromptContext {
            channel: request.channel,
            customer_profile: gathered.profile.clone(),
            company_instructions: gathered.company_instructions.clone(),
            admin_instruction: Some(gathered.admin_instruction.clone()),
            templates: gathered.templates.clone(),
            timeline: gathered.timeline.clone(),
            now: None,
            crm_hold: if request.force_generate {
                None
            } else {
                gathered.crm_hold_for_prompt()
            },
            contact_pressure: Some(gathered.contact_pressure.clone()),
            style_profile: gathered.style_profile.clone(),
            rep_voice: gathered.rep_voice.examples.clone(),
            force_generate: request.force_generate,
            step_channels: channels.clone(),
        },
        now,
    );
    timer.prompt_components = Some(prompt_component_metrics(&PromptParts {
        system_prompt: &system_prompt,
        user_prompt: &user_prompt,
        contact_pressure_block: &format_contact_pressure_block(&gathered.contact_pressure),
        profile: &gathered.profile,
        conversation_block: &format_followup_timeline_block(&gathered.timeline),
        cadence_specs_block: &user_prompt,
        style_block: &format_style_preference_prompt_block(gathered.style_profile.as_ref()),
        templates_block: &format_followup_template_block(&gathered.templates, Some(4), Some(400)),
    }));
    timer.mark("prompt_construction");
    let reply = writer.complete(&system_prompt, &user_prompt).await?;
    timer.llm_primary = Some(LlmCallMetrics::of(&reply, "primary"));
    let Some(mut parsed) = parse_cadence_followup_generation(&reply.text, &channels) else {
        timer.add_retry_reason("json_parse_failure");
        return Err(GenerationError::Parse);
    };
    if request.force_generate && !cadence_has_writable_copy(Some(&parsed)) {
        timer.retry_reasons.push("missing_message");
        timer.retry_used = true;
        let retry = writer
            .complete(
                &system_prompt,
                &format!("{user_prompt}\n\n{FORCE_GENERATE_RETRY_INSTRUCTION}"),
            )
            .await?;
        timer.llm_retry = Some(LlmCallMetrics::of(&retry, "retry"));
        match parse_cadence_followup_generation(&retry.text, &channels) {
            Some(retried) if cadence_has_writable_copy(Some(&retried)) => parsed = retried,
            Some(_) => {}
            None => timer.retry_reasons.push("json_parse_failure"),
        }
    }
    let sends = send_ats(&writable_steps, &parsed, &gathered, now);
    let (rejected, errors) = copy_failures(&parsed, writable_steps.len(), &gathered, &sends);
    if !errors.is_empty() {
        timer.retry_reasons.push("copy_validation");
    }
    timer.first_pass_validator_pass = Some(errors.is_empty());
    timer.first_pass_copy_failures = Some(
        errors
            .iter()
            .enumerate()
            .map(|(index, error)| CopyFailure {
                step_index: index,
                validator_rule: error.clone(),
                rule_category: rule_category(error),
                retry_reason: "copy_validation",
                offending_claim: error.clone(),
                message_text: String::new(),
            })
            .collect(),
    );
    timer.mark("validation_after_llm");
    if !errors.is_empty() {
        timer.retry_used = true;
        let correction = errors
            .iter()
            .map(|error| format!("Copy validation: {error}."))
            .collect::<Vec<_>>()
            .join(" ");
        let retry = writer
            .complete(
                &system_prompt,
                &format!("{user_prompt}\n\nREGENERATE: fix copy validation issues. {correction}"),
            )
            .await?;
        timer.llm_retry = Some(LlmCallMetrics::of(&retry, "retry"));
        match parse_cadence_followup_generation(&retry.text, &channels) {
            Some(retried) => {
                let accepted = parsed;
                parsed = retried
                    .into_iter()
                    .enumerate()
                    .map(|(index, message)| {
                        if rejected.contains(&index) {
                            message
                        } else {
                            accepted.get(index).cloned().unwrap_or(message)
                        }
                    })
                    .collect();
            }
            None => timer.retry_reasons.push("json_parse_failure"),
        }
    }
    let messages = onto_planned_steps(steps, &parsed, true);
    timer.followups_generated = messages
        .iter()
        .filter(|m| !m.skip && !trim(&m.body).is_empty())
        .count();
    timer.mark("post_processing");
    Ok(messages)
}

/// The `NEW_CONTEXT` cadence: the model context and Markdown instructions,
/// then one repair of only the messages that failed the copy checks.
#[allow(clippy::too_many_lines)]
async fn complete_cadence_new_context(
    writer: Writer<'_>,
    request: &FollowupRequest,
    gathered: &GatheredContext,
    steps: &[PlannedAiCadenceStep],
    now: Instant,
    timer: &mut PipelineTimer,
) -> Result<Vec<GeneratedCadenceFollowup>, GenerationError> {
    let writable_steps: Vec<PlannedAiCadenceStep> = writable(steps).into_iter().cloned().collect();
    let source = ModelContextSource {
        profile: &gathered.profile,
        timeline: &gathered.timeline,
        templates: &gathered.templates,
        admin_instruction: Some(&gathered.admin_instruction),
        contact_pressure: &gathered.contact_pressure,
        style_profile: gathered.style_profile.as_ref(),
    };
    let context = build_cadence_model_context(&source, &writable_steps, now);
    let mut prompt = build_refactored_cadence_prompt(AI_FOLLOWUP_PROMPT, &context);
    let voice = super::rep_voice::format_voice_block(&gathered.rep_voice.examples);
    if !voice.is_empty() {
        prompt.user_prompt = format!("{}\n\n{voice}", prompt.user_prompt);
    }
    let system_prompt = if request.force_generate {
        format!(
            "{}\n\n{FORCE_GENERATE_CADENCE_INSTRUCTION}",
            prompt.system_prompt
        )
    } else {
        prompt.system_prompt.clone()
    };
    if writable_steps.is_empty() {
        return Ok(steps
            .iter()
            .map(|_| GeneratedCadenceFollowup::skipped("manual_phone_call"))
            .collect());
    }
    let channels: Vec<FollowupChannel> = writable_steps.iter().map(|s| s.channel).collect();
    let reply = writer.complete(&system_prompt, &prompt.user_prompt).await?;
    timer.llm_primary = Some(LlmCallMetrics::of(&reply, "primary"));
    let Some(mut parsed) = parse_cadence_followup_generation(&reply.text, &channels) else {
        return Err(GenerationError::Parse);
    };
    let mut retry_used = false;
    if request.force_generate && !cadence_has_writable_copy(Some(&parsed)) {
        retry_used = true;
        timer.add_retry_reason("missing_message");
        let retry = writer
            .complete(
                &system_prompt,
                &format!(
                    "{}\n\n{FORCE_GENERATE_RETRY_INSTRUCTION}",
                    prompt.user_prompt
                ),
            )
            .await?;
        timer.llm_retry = Some(LlmCallMetrics::of(&retry, "retry"));
        timer.retry_used = true;
        match parse_cadence_followup_generation(&retry.text, &channels) {
            Some(retried) if cadence_has_writable_copy(Some(&retried)) => parsed = retried,
            Some(_) => {}
            None => timer.add_retry_reason("json_parse_failure"),
        }
    }
    let writable_refs: Vec<&PlannedAiCadenceStep> = writable_steps.iter().collect();
    let sends = send_ats(&writable_refs, &parsed, gathered, now);
    let (rejected, reasons) = copy_failures(&parsed, writable_steps.len(), gathered, &sends);
    if reasons.is_empty() {
        timer.retry_mode = None;
        timer.failed_step_count = Some(0);
        timer.total_writable_steps = Some(writable_steps.len());
    } else {
        timer.add_retry_reason("copy_validation");
        retry_used = true;
        // Repair only the rejected steps: re-emitting the passing ones costs
        // output tokens and risks changing accepted copy.
        let repair_channels: Vec<FollowupChannel> = rejected
            .iter()
            .filter_map(|i| channels.get(*i).copied())
            .collect();
        let scoped = !rejected.is_empty() && repair_channels.len() == rejected.len();
        let numbers: Vec<usize> = if scoped {
            rejected.iter().map(|i| i + 1).collect()
        } else {
            Vec::new()
        };
        let repair = writer
            .complete(
                &system_prompt,
                &build_refactored_cadence_repair_user_prompt(
                    &prompt.user_prompt,
                    &reasons,
                    &numbers,
                ),
            )
            .await?;
        timer.llm_retry = Some(LlmCallMetrics::of(&repair, "retry"));
        // A model that re-emits the whole cadence must not have its first
        // messages written into the failed slots.
        let full = parse_cadence_followup_generation(&repair.text, &channels);
        let merged_scoped = full.is_none() && scoped;
        let retried = full.or_else(|| {
            if scoped {
                parse_cadence_followup_generation(&repair.text, &repair_channels)
            } else {
                None
            }
        });
        timer.retry_used = true;
        timer.retry_mode = Some(if merged_scoped && retried.is_some() {
            "failed_step_only"
        } else {
            "full_batch"
        });
        timer.failed_step_count = Some(rejected.len());
        timer.total_writable_steps = Some(writable_steps.len());
        match retried {
            Some(retried) if merged_scoped => {
                for (repair_index, step_index) in rejected.iter().enumerate() {
                    if let (Some(message), Some(slot)) =
                        (retried.get(repair_index), parsed.get_mut(*step_index))
                    {
                        *slot = message.clone();
                    }
                }
            }
            Some(retried) => {
                let accepted = parsed;
                parsed = retried
                    .into_iter()
                    .enumerate()
                    .map(|(index, message)| {
                        if rejected.contains(&index) {
                            message
                        } else {
                            accepted.get(index).cloned().unwrap_or(message)
                        }
                    })
                    .collect();
            }
            None => timer.add_retry_reason("json_parse_failure"),
        }
    }
    timer.retry_used = timer.retry_used || retry_used;
    timer.followups_requested = writable_steps.len();
    Ok(onto_planned_steps(steps, &parsed, false))
}

fn normalize_compare_text(text: &str) -> String {
    trim(&re!(r"\s+", "g").replace_all(&text.to_lowercase(), " ")).to_string()
}

fn significant_words(text: &str) -> std::collections::HashSet<String> {
    normalize_compare_text(text)
        .split(' ')
        .filter(|w| w.chars().count() > 2)
        .map(str::to_string)
        .collect()
}

/// The new draft is the same or nearly the same as the rejected one.
pub fn followup_drafts_too_similar(a: &str, b: &str) -> bool {
    let (na, nb) = (normalize_compare_text(a), normalize_compare_text(b));
    if na.is_empty() || nb.is_empty() {
        return false;
    }
    if na == nb {
        return true;
    }
    let (wa, wb) = (significant_words(a), significant_words(b));
    if wa.is_empty() || wb.is_empty() {
        return false;
    }
    let overlap = wa.intersection(&wb).count();
    let union = wa.len() + wb.len() - overlap;
    #[allow(clippy::cast_precision_loss)]
    let ratio = overlap as f64 / union as f64;
    union > 0 && ratio >= 0.82
}

/// Stricter: a light paraphrase of a rejected draft still repeats it.
pub fn regenerated_draft_repeats(draft: &str, rejected: &[String]) -> bool {
    let wd = significant_words(draft);
    rejected.iter().any(|previous| {
        if followup_drafts_too_similar(draft, previous) {
            return true;
        }
        let wp = significant_words(previous);
        if wd.is_empty() || wp.is_empty() {
            return false;
        }
        let overlap = wd.intersection(&wp).count();
        #[allow(clippy::cast_precision_loss)]
        let ratio = overlap as f64 / (wd.len() + wp.len() - overlap) as f64;
        ratio >= 0.6
    })
}

/// Why a rewrite failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegenerateFailure {
    NothingToSend,
    TooSimilar,
    GenerateFailed,
}

impl RegenerateFailure {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NothingToSend => "nothing_to_send",
            Self::TooSimilar => "regenerate_too_similar",
            Self::GenerateFailed => "generate_failed",
        }
    }
}

/// Rewrites one cadence message so it differs from the drafts the rep
/// rejected (up to three tries). Always the `LEGACY` prompt.
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
pub async fn regenerate_cadence_message(
    writer: Writer<'_>,
    gathered: &GatheredContext,
    step: &PlannedAiCadenceStep,
    avoid_body: &str,
    avoid_subject: Option<&str>,
    avoid_previous: &[String],
    force_generate: bool,
    now: Instant,
) -> Result<Result<(String, Option<String>), RegenerateFailure>, GenerationError> {
    let mut rejected: Vec<String> = Vec::new();
    for body in avoid_previous
        .iter()
        .map(String::as_str)
        .chain([avoid_body])
    {
        let body = trim(body);
        if !body.is_empty() && !rejected.iter().any(|r| r == body) {
            rejected.push(body.to_string());
        }
    }
    if matches!(
        gathered.generation_decision,
        FollowupGenerationDecision::DoNotGenerate { .. }
    ) && !force_generate
    {
        return Ok(Err(RegenerateFailure::NothingToSend));
    }
    let gathered = if force_generate {
        gathered.for_forced_generate()
    } else {
        gathered.clone()
    };
    let system_prompt = build_cadence_followup_system_prompt(force_generate);
    let user_prompt = super::prompt::build_cadence_followup_user_prompt_at(
        &CadencePromptContext {
            channel: step.channel,
            customer_profile: gathered.profile.clone(),
            company_instructions: gathered.company_instructions.clone(),
            admin_instruction: Some(gathered.admin_instruction.clone()),
            templates: gathered.templates.clone(),
            timeline: gathered.timeline.clone(),
            now: None,
            crm_hold: if force_generate {
                None
            } else {
                gathered.crm_hold_for_prompt()
            },
            contact_pressure: Some(gathered.contact_pressure.clone()),
            style_profile: gathered.style_profile.clone(),
            rep_voice: gathered.rep_voice.examples.clone(),
            force_generate,
            step_channels: vec![step.channel],
        },
        now,
    );
    let subject_line = avoid_subject
        .map(trim)
        .filter(|s| !s.is_empty())
        .map_or_else(String::new, |s| {
            format!("\nRejected subject:\n\"\"\"{s}\"\"\"")
        });
    let body_lines = rejected
        .iter()
        .enumerate()
        .map(|(i, body)| format!("Rejected body {}:\n\"\"\"{body}\"\"\"", i + 1))
        .collect::<Vec<_>>()
        .join("\n");
    let instruction = format!(
        "REGENERATE THIS SINGLE MESSAGE.\nWrite a substantially different draft from every rejected text below.\nDo not repeat or paraphrase any of them. Change the opening, the concrete ask, the angle, and the wording.\nKeep the same Channel and Goal.\n{body_lines}{subject_line}"
    );
    let mut last: Option<(String, Option<String>)> = None;
    for attempt in 0..3 {
        let note = if attempt == 0 {
            instruction.clone()
        } else {
            format!(
                "{instruction}\nPrevious attempt was still too similar. Rewrite again with a different angle and different sentences."
            )
        };
        let reply = writer
            .complete(&system_prompt, &format!("{user_prompt}\n\n{note}"))
            .await?;
        let Some(generated) = parse_cadence_followup_generation(&reply.text, &[step.channel])
            .and_then(|parsed| parsed.into_iter().next())
            .filter(|g| !g.skip && !trim(&g.body).is_empty())
        else {
            continue;
        };
        let stored = stored_message(Some(&generated), step.channel, true);
        if stored.skip
            || trim(&stored.body).is_empty()
            || !followup_tone_errors(&stored.body, false).is_empty()
        {
            continue;
        }
        let body_similar = regenerated_draft_repeats(&stored.body, &rejected);
        let subject_similar = step.channel == FollowupChannel::Email
            && avoid_subject.is_some_and(|s| !trim(s).is_empty())
            && stored
                .subject
                .as_deref()
                .is_some_and(|s| followup_drafts_too_similar(s, avoid_subject.unwrap_or("")));
        last = Some((stored.body.clone(), stored.subject.clone()));
        if !body_similar && !subject_similar {
            return Ok(Ok((stored.body, stored.subject)));
        }
    }
    Ok(match last {
        Some((body, subject)) if !trim(&body).is_empty() => {
            if rejected
                .iter()
                .any(|r| followup_drafts_too_similar(&body, r))
            {
                Err(RegenerateFailure::TooSimilar)
            } else {
                Ok((body, subject))
            }
        }
        _ => Err(RegenerateFailure::GenerateFailed),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn similarity_matches_the_crm() {
        assert!(followup_drafts_too_similar(
            "Hi Ann, any news on the quote?",
            "hi ann,  any news on the quote?"
        ));
        assert!(!followup_drafts_too_similar("", "x"));
        assert!(regenerated_draft_repeats(
            "Just checking in about your kitchen quote today",
            &["Checking in about your kitchen quote".to_string()]
        ));
        assert!(!regenerated_draft_repeats(
            "Would Tuesday work for a measure?",
            &["Any news on the quote?".to_string()]
        ));
        assert_eq!(
            CopyGenerationMode::parse(" new-context "),
            Some(CopyGenerationMode::NewContext)
        );
        assert_eq!(
            CopyGenerationMode::parse("new"),
            Some(CopyGenerationMode::NewContext)
        );
        assert_eq!(CopyGenerationMode::parse("x"), None);
    }
}
