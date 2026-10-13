//! Timing and counts of one AI follow-up generation, sent to `PostHog` as
//! `ai_followup_pipeline` (port of `smsFlowAiPipelineInstrumentation.ts`).

use super::js::js_len;
use super::refactored_prompt::estimate_tokens_from_chars;
use serde::Serialize;
use std::collections::HashMap;
use std::time::Instant;

/// One model call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LlmCallMetrics {
    pub model: String,
    pub duration_ms: u32,
    pub input_tokens: Option<u32>,
    pub output_tokens: Option<u32>,
    pub finish_reason: Option<String>,
    /// `primary` or `retry`.
    pub attempt: &'static str,
}

impl LlmCallMetrics {
    pub fn of(reply: &crate::ai::ChatReply, attempt: &'static str) -> Self {
        Self {
            model: reply.model.clone(),
            duration_ms: reply.duration_ms,
            input_tokens: reply.input_tokens,
            output_tokens: reply.output_tokens,
            finish_reason: reply.finish_reason.clone(),
            attempt,
        }
    }
}

/// Characters of each part of a legacy prompt.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptComponentChars {
    pub system_rules_chars: usize,
    pub contact_pressure_chars: usize,
    pub profile_chars: usize,
    pub conversation_chars: usize,
    pub cadence_specs_chars: usize,
    pub style_chars: usize,
    pub templates_chars: usize,
    pub other_user_prompt_chars: usize,
    pub total_user_prompt_chars: usize,
    pub total_system_prompt_chars: usize,
    pub estimated_input_tokens: usize,
}

/// The parts of a legacy prompt, to measure.
pub struct PromptParts<'a> {
    pub system_prompt: &'a str,
    pub user_prompt: &'a str,
    pub contact_pressure_block: &'a str,
    pub profile: &'a str,
    pub conversation_block: &'a str,
    pub cadence_specs_block: &'a str,
    pub style_block: &'a str,
    pub templates_block: &'a str,
}

pub fn prompt_component_metrics(parts: &PromptParts<'_>) -> PromptComponentChars {
    let total_user = js_len(parts.user_prompt);
    let pressure = js_len(parts.contact_pressure_block);
    let profile = js_len(parts.profile);
    let conversation = js_len(parts.conversation_block);
    let cadence = js_len(parts.cadence_specs_block);
    let style = js_len(parts.style_block);
    let templates = js_len(parts.templates_block);
    let attributed = pressure + profile + conversation + cadence + style + templates;
    let system = js_len(parts.system_prompt);
    PromptComponentChars {
        system_rules_chars: system,
        contact_pressure_chars: pressure,
        profile_chars: profile,
        conversation_chars: conversation,
        cadence_specs_chars: cadence,
        style_chars: style,
        templates_chars: templates,
        other_user_prompt_chars: total_user.saturating_sub(attributed),
        total_user_prompt_chars: total_user,
        total_system_prompt_chars: system,
        estimated_input_tokens: estimate_tokens_from_chars(system + total_user),
    }
}

/// A copy check that failed on the first pass.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CopyFailure {
    pub step_index: usize,
    pub validator_rule: String,
    pub rule_category: &'static str,
    pub retry_reason: &'static str,
    pub offending_claim: String,
    pub message_text: String,
}

/// Times the phases of one generation and collects what it did.
#[derive(Debug)]
pub struct PipelineTimer {
    started_at: Instant,
    last_mark_at: Instant,
    marks: HashMap<&'static str, f64>,
    pub llm_primary: Option<LlmCallMetrics>,
    pub llm_retry: Option<LlmCallMetrics>,
    pub retry_used: bool,
    pub retry_reasons: Vec<&'static str>,
    pub prompt_components: Option<PromptComponentChars>,
    pub followups_generated: usize,
    pub followups_requested: usize,
    pub first_pass_validator_pass: Option<bool>,
    pub first_pass_copy_failures: Option<Vec<CopyFailure>>,
    /// `full_batch` or `failed_step_only`.
    pub retry_mode: Option<&'static str>,
    pub failed_step_count: Option<usize>,
    pub total_writable_steps: Option<usize>,
}

impl Default for PipelineTimer {
    fn default() -> Self {
        Self::new()
    }
}

impl PipelineTimer {
    pub fn new() -> Self {
        let now = Instant::now();
        Self {
            started_at: now,
            last_mark_at: now,
            marks: HashMap::new(),
            llm_primary: None,
            llm_retry: None,
            retry_used: false,
            retry_reasons: Vec::new(),
            prompt_components: None,
            followups_generated: 0,
            followups_requested: 0,
            first_pass_validator_pass: None,
            first_pass_copy_failures: None,
            retry_mode: None,
            failed_step_count: None,
            total_writable_steps: None,
        }
    }

    /// The time since the last mark belongs to `phase`.
    pub fn mark(&mut self, phase: &'static str) {
        let now = Instant::now();
        *self.marks.entry(phase).or_default() += (now - self.last_mark_at).as_secs_f64() * 1000.0;
        self.last_mark_at = now;
    }

    pub fn elapsed_ms(&self) -> f64 {
        self.started_at.elapsed().as_secs_f64() * 1000.0
    }

    pub fn phase_ms(&self, phase: &str) -> f64 {
        self.marks.get(phase).copied().unwrap_or_default()
    }

    /// Adds a retry reason once.
    pub fn add_retry_reason(&mut self, reason: &'static str) {
        if !self.retry_reasons.contains(&reason) {
            self.retry_reasons.push(reason);
        }
    }
}

/// The `ai_followup_pipeline` event.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PipelineMetrics {
    /// `preview`, `single_message` or `cadence_batch`.
    pub operation: &'static str,
    pub total_ms: f64,
    pub db_context_load_ms: f64,
    pub conversation_normalization_ms: f64,
    pub fact_extraction_ms: f64,
    pub contact_pressure_build_ms: f64,
    pub validation_ms: f64,
    pub cadence_planning_ms: f64,
    pub prompt_construction_ms: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub llm_primary: Option<LlmCallMetrics>,
    pub validation_after_llm_ms: f64,
    pub retry_used: bool,
    pub retry_reasons: Vec<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub llm_retry: Option<LlmCallMetrics>,
    pub post_processing_ms: f64,
    pub sms_loaded: usize,
    pub emails_loaded: usize,
    pub timeline_message_count: usize,
    pub timeline_chars: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_components: Option<PromptComponentChars>,
    pub followups_requested: usize,
    pub followups_generated: usize,
    pub generation_decision: String,
    pub copy_generation_mode: &'static str,
    pub customer_id: Option<i32>,
    pub company_id: i32,
    pub user_id: i32,
    pub first_pass_validator_pass: Option<bool>,
    pub first_pass_copy_failures: Option<Vec<CopyFailure>>,
    pub retry_mode: Option<&'static str>,
    pub failed_step_count: Option<usize>,
    pub total_writable_steps: Option<usize>,
}

/// What [`assemble_metrics`] reads from the gathered context.
pub struct GatheredCounts<'a> {
    pub customer_id: Option<i32>,
    pub timeline_texts: Vec<&'a str>,
    pub generation_decision: &'a super::lifecycle::FollowupGenerationDecision,
    pub sms_loaded: usize,
    pub emails_loaded: usize,
}

pub fn assemble_metrics(
    timer: &PipelineTimer,
    operation: &'static str,
    gathered: &GatheredCounts<'_>,
    company_id: i32,
    user_id: i32,
    cadence_planning_ms: f64,
    copy_generation_mode: &'static str,
) -> PipelineMetrics {
    use super::lifecycle::FollowupGenerationDecision as D;
    PipelineMetrics {
        operation,
        total_ms: timer.elapsed_ms(),
        db_context_load_ms: timer.phase_ms("db_load"),
        conversation_normalization_ms: timer.phase_ms("conversation_normalization"),
        fact_extraction_ms: timer.phase_ms("fact_extraction"),
        contact_pressure_build_ms: timer.phase_ms("fact_extraction"),
        validation_ms: timer.phase_ms("validation"),
        cadence_planning_ms,
        prompt_construction_ms: timer.phase_ms("prompt_construction"),
        llm_primary: timer.llm_primary.clone(),
        validation_after_llm_ms: timer.phase_ms("validation_after_llm"),
        retry_used: timer.retry_used,
        retry_reasons: timer.retry_reasons.clone(),
        llm_retry: timer.llm_retry.clone(),
        post_processing_ms: timer.phase_ms("post_processing"),
        sms_loaded: gathered.sms_loaded,
        emails_loaded: gathered.emails_loaded,
        timeline_message_count: gathered.timeline_texts.len(),
        timeline_chars: gathered.timeline_texts.iter().map(|t| js_len(t)).sum(),
        prompt_components: timer.prompt_components,
        followups_requested: timer.followups_requested,
        followups_generated: timer.followups_generated,
        generation_decision: match gathered.generation_decision {
            D::Generate => "GENERATE".into(),
            D::DoNotGenerate { reason } => reason.clone(),
        },
        copy_generation_mode,
        customer_id: gathered.customer_id,
        company_id,
        user_id,
        first_pass_validator_pass: timer.first_pass_validator_pass,
        first_pass_copy_failures: timer.first_pass_copy_failures.clone(),
        retry_mode: timer.retry_mode,
        failed_step_count: timer.failed_step_count,
        total_writable_steps: timer.total_writable_steps,
    }
}

/// Sends the event to `PostHog` (when server reporting is on).
pub async fn log_metrics(http: &reqwest::Client, metrics: &PipelineMetrics) {
    let properties = serde_json::to_value(metrics).unwrap_or_default();
    crate::posthog::capture(
        http,
        "ai_followup_pipeline",
        &metrics.user_id.to_string(),
        properties,
    )
    .await;
}
