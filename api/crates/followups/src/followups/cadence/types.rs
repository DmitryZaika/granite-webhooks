//! Types of the AI follow-up cadence (`smsFlowAiCadence.ts`).

use super::super::dates::Instant;
use super::super::pressure::{
    CustomerFollowupState, FollowupAngle, FollowupContactPressure, FollowupPhase,
};
use super::super::send_window::FollowupChannel;
use super::ramp::ConversationPace;
use crate::str_enum;
use serde::Serialize;

/// Personal flows created by Generate are hidden from the picker and manager.
pub const AI_CADENCE_FLOW_NAME: &str = "Generated with AI";

str_enum! { pub enum FollowupSituation {
    FirstContact = "first_contact", SecondFollowup = "second_followup", StalledConversation = "stalled_conversation",
} }

str_enum! { pub enum FollowupSemanticObjective {
    ChooseVisualStyle = "choose_visual_style", ConfirmProjectActive = "confirm_project_active",
    IdentifyBudgetObstacle = "identify_budget_obstacle", ReviewQuote = "review_quote", ConfirmMaterial = "confirm_material",
    ScheduleNextStep = "schedule_next_step", CloseLoop = "close_loop", EasyRestart = "easy_restart",
    ProjectStillRelevant = "project_still_relevant", RestartProject = "restart_project", EducateMaterial = "educate_material",
    MultiUnitReadiness = "multi_unit_readiness", Other = "other",
} }

str_enum! { pub enum CopySemanticObjective {
    Timing = "TIMING", MaterialType = "MATERIAL_TYPE", Color = "COLOR", Quote = "QUOTE", Budget = "BUDGET",
    Scope = "SCOPE", Design = "DESIGN", NextStep = "NEXT_STEP", CloseLoop = "CLOSE_LOOP", Nurture = "NURTURE",
} }

str_enum! { pub enum AiCadenceMode { New = "new", Unanswered = "unanswered", Monthly = "monthly" } }

str_enum! { pub enum PreflightStatus { Writable = "WRITABLE", Repaired = "REPAIRED", Drop = "DROP" } }

/// One step's intent, before it is pinned to a date or channel.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CadenceShapeEntry {
    pub situation: FollowupSituation,
    pub goal: String,
    pub purpose: String,
    pub angle: FollowupAngle,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subangle: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub phase: Option<FollowupPhase>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub semantic_objective: Option<FollowupSemanticObjective>,
}

impl CadenceShapeEntry {
    pub fn new(
        situation: FollowupSituation,
        goal: impl Into<String>,
        angle: FollowupAngle,
        subangle: Option<&str>,
        purpose: impl Into<String>,
    ) -> Self {
        Self {
            situation,
            goal: goal.into(),
            purpose: purpose.into(),
            angle,
            subangle: subangle.map(str::to_string),
            phase: None,
            semantic_objective: None,
        }
    }

    pub fn stalled(
        goal: impl Into<String>,
        angle: FollowupAngle,
        subangle: &str,
        purpose: impl Into<String>,
    ) -> Self {
        Self::new(
            FollowupSituation::StalledConversation,
            goal,
            angle,
            Some(subangle),
            purpose,
        )
    }
}

impl From<&super::shapes::ShapeDef> for CadenceShapeEntry {
    fn from(def: &super::shapes::ShapeDef) -> Self {
        Self::new(
            def.situation,
            def.goal,
            def.angle,
            def.subangle,
            def.purpose,
        )
    }
}

/// A step of the planned sequence: its intent, when and over which channel.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PlannedAiCadenceStep {
    pub offset_days: f64,
    pub delay_label: String,
    pub situation: FollowupSituation,
    pub goal: String,
    pub purpose: String,
    pub angle: FollowupAngle,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subangle: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub phase: Option<FollowupPhase>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub semantic_objective: Option<FollowupSemanticObjective>,
    pub delay_minutes: f64,
    pub channel: FollowupChannel,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub manual_action: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub original_step_index: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preflight_status: Option<PreflightStatus>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preflight_reason: Option<String>,
    /// Day the message will go out under the final schedule (YYYY-MM-DD).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expected_send_on: Option<String>,
}

/// Options of [`plan_ai_followup_cadence`](super::plan::plan_ai_followup_cadence).
#[derive(Debug, Clone, Default)]
pub struct PlanAiFollowupCadenceOptions {
    pub mix_sms_and_email: bool,
    pub channel: Option<FollowupChannel>,
    pub outbound_count: Option<f64>,
    pub first_outbound_at: Option<Instant>,
    /// Do not send the first step before this instant.
    pub hold_until_at: Option<Instant>,
    pub customer_requested_followup_at: Option<Instant>,
    pub customer_state: Option<CustomerFollowupState>,
    pub unanswered_total: Option<f64>,
    pub contact_pressure: Option<FollowupContactPressure>,
    /// How fast we have been writing; no wait gets shorter than it.
    pub pace: Option<ConversationPace>,
}

/// One message of a cadence preview (generated, maybe edited).
#[derive(Debug, Clone, PartialEq, Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AiCadencePreviewMessage {
    pub delay_minutes: f64,
    pub delay_label: String,
    pub goal: String,
    pub channel: FollowupChannel,
    pub skip: bool,
    pub skip_reason: Option<String>,
    pub body: String,
    pub subject: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generated_body: Option<String>,
    #[serde(
        default,
        deserialize_with = "crate::serde_helpers::double_option",
        skip_serializing_if = "Option::is_none"
    )]
    pub generated_subject: Option<Option<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub angle: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<String>,
}
