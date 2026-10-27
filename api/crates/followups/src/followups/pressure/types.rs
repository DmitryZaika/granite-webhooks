//! The contact-pressure picture of a customer (types of
//! `smsFlowAiContactPressure.ts`).

use super::super::classify::{
    InboundMessageType, ReplyClassificationConfidence, SalespersonOutboundType,
};
use super::super::contact_resume::{AutomationContactStatus, ChannelPermission, ContactResumeMode};
use super::super::conversation::RepOwnedNextStep;
use super::super::dates::Instant;
use super::super::facts::{
    ActivityActor, AppointmentCompanion, BacksplashFinish, BacksplashType, ContactMethod,
    DealProductScope, DecisionCriterion, DecisionFactor, DecisionObstacle, EstimatedReadyPrecision,
    EvidenceConfidence, EvidenceSource, ExplicitCustomerRequestKind, ExtractedMeasurement,
    LatestCustomerState, ManualActionRequired, ManualActionStatus, MaterialDirectionStatus,
    MeasurementComponent, MeasurementsStatus, PlannedCustomerEventType, PreferredNextStep,
    PreferredNextStepSource, PriceContext, ProjectAreaState, ProjectDependency, ProjectScale,
    QuoteType, SinkSource, TimelinePosition, UnfinishedNextStepKind, VisualStyle,
};
use super::super::intent::MeaningfulCustomerIntent;
use super::super::lifecycle::FollowupSequenceStatus;
use crate::str_enum;
use serde::Serialize;

str_enum! { pub enum FollowupAngle {
    InitialInterest = "initial_interest", ProjectStatus = "project_status", OriginalTimeline = "original_timeline",
    MaterialPreference = "material_preference", ProjectScope = "project_scope", BudgetObstacle = "budget_obstacle",
    DecisionObstacle = "decision_obstacle", QuoteStatus = "quote_status", DesignHelp = "design_help",
    EstimateOffer = "estimate_offer", ProjectDependency = "project_dependency", HelpfulInformation = "helpful_information",
    Promotion = "promotion", Reactivation = "reactivation", HoldReactivation = "hold_reactivation",
    ProjectTiming = "project_timing", CloseLoop = "close_loop", LongTermNurture = "long_term_nurture",
    QuoteReview = "quote_review", PreferredNextStep = "preferred_next_step", ChannelPreference = "channel_preference",
    PackageInterest = "package_interest", ComparisonSupport = "comparison_support",
    PreviousStoneSelection = "previous_stone_selection",
} }

str_enum! { pub enum CustomerFollowupState {
    FreshLead = "fresh_lead", NewLeadNoResponse = "new_lead_no_response", UnreachedLead = "unreached_lead",
    RecentlyEngaged = "recently_engaged", Waiting = "waiting", QuoteGhost = "quote_ghost",
    StalledProject = "stalled_project", RepeatedNonResponder = "repeated_non_responder", Dormant = "dormant", Hold = "hold",
} }

str_enum! { pub enum ContactPressureLevel { None = "none", Low = "low", Moderate = "moderate", Elevated = "elevated" } }

str_enum! { pub enum ChannelAttemptState {
    Untested = "untested", AttemptedNoResponse = "attempted_no_response", Replied = "replied", Unsuccessful = "unsuccessful",
} }

str_enum! { pub enum CustomerIntent { HighUrgency = "high_urgency", Normal = "normal", Deferred = "deferred", Paused = "paused" } }

str_enum! { pub enum CommunicationHistoryStatus {
    TrulyNew = "truly_new", ExistingCustomerNoDigitalHistory = "existing_customer_no_digital_history",
    PreviouslyContacted = "previously_contacted", ActiveConversation = "active_conversation", Dormant = "dormant",
} }

str_enum! { pub enum HoldReasonType {
    CustomerRequestedDelay = "customer_requested_delay", HigherPriorityDelay = "higher_priority_delay",
    FinancialDelay = "financial_delay", ConstructionDependency = "construction_dependency",
    ProjectTimeline = "project_timeline", InternalFollowupSchedule = "internal_followup_schedule",
    PersonalIssue = "personal_issue", SeasonalDelay = "seasonal_delay", DecisionDelay = "decision_delay", Unknown = "unknown",
} }

str_enum! { pub enum HoldReasonSource {
    CustomerSms = "customer_sms", CustomerEmail = "customer_email", SalespersonNote = "salesperson_note",
    CrmActivity = "crm_activity", Deal = "deal", LeadForm = "lead_form", Inferred = "inferred",
} }

str_enum! { pub enum HoldSourceType {
    CustomerRequested = "customer_requested", SalespersonRequested = "salesperson_requested",
    ProjectDependency = "project_dependency", CrmManualHold = "crm_manual_hold",
    InternalFollowupTask = "internal_followup_task", Unknown = "unknown",
} }

/// Why the customer is on hold, from where, and how sure.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HoldReasonEvidence {
    pub hold_reason_type: HoldReasonType,
    pub hold_reason_source: Option<HoldReasonSource>,
    pub hold_reason_evidence: Option<String>,
    pub hold_reason_at: Option<Instant>,
    pub hold_reason_confidence: EvidenceConfidence,
}

str_enum! { pub enum SmsChannelStatus {
    Valid = "valid", Untested = "untested", AttemptedNoResponse = "attempted_no_response",
    AttemptedUnknownResult = "attempted_unknown_result", Invalid = "invalid", InvalidFormat = "invalid_format",
    WrongNumber = "wrong_number", OptedOut = "opted_out", Undeliverable = "undeliverable",
    CustomerRequestedNoText = "customer_requested_no_text", Unknown = "unknown",
} }

str_enum! { pub enum EmailChannelStatus {
    Valid = "valid", Untested = "untested", AttemptedNoResponse = "attempted_no_response",
    AttemptedUnknownResult = "attempted_unknown_result", Bounced = "bounced", OptedOut = "opted_out",
    Invalid = "invalid", Unknown = "unknown",
} }

str_enum! { pub enum OperationalContactState {
    Uncontacted = "UNCONTACTED", ContactedNoResponse = "CONTACTED_NO_RESPONSE", Responsive = "RESPONSIVE",
    NoDigitalHistory = "NO_DIGITAL_HISTORY",
} }

str_enum! { pub enum PhoneChannelStatus {
    Valid = "valid", Untested = "untested", AttemptedNoResponse = "attempted_no_response", InvalidFormat = "invalid_format",
    WrongNumber = "wrong_number", Unreachable = "unreachable", DoNotCall = "do_not_call", Unknown = "unknown",
} }

str_enum! { pub enum CustomerMessageSemanticType {
    AcknowledgmentReaction = "ACKNOWLEDGMENT_REACTION", Acknowledgment = "ACKNOWLEDGMENT",
    FollowupPermission = "FOLLOWUP_PERMISSION", ProjectStatement = "PROJECT_STATEMENT", Question = "QUESTION",
    Delay = "DELAY", Decline = "DECLINE", None = "NONE",
} }

str_enum! { pub enum RelationshipState { None = "none", ExistingOldLead = "existing_old_lead", ExistingActive = "existing_active" } }

str_enum! { pub enum RelationshipAge { New = "new", Existing = "existing", Old = "old" } }

str_enum! { pub enum EngagementState {
    Unreached = "unreached", Responsive = "responsive", RecentlyUnresponsive = "recently_unresponsive",
    Stalled = "stalled", Dormant = "dormant", DormantNoDigitalHistory = "dormant_no_digital_history", Hold = "hold",
} }

str_enum! { pub enum DealState {
    NotContacted = "not_contacted", Contacted = "contacted", Quoted = "quoted", Won = "won", Lost = "lost", OnHold = "on_hold",
} }

str_enum! { pub enum FollowupPhase {
    FirstFollowup = "FIRST_FOLLOWUP", EarlyFollowup = "EARLY_FOLLOWUP", ActivePursuit = "ACTIVE_PURSUIT",
    Decay = "DECAY", Reactivation = "REACTIVATION", LongTermNurture = "LONG_TERM_NURTURE",
} }

str_enum! { pub enum InformationDensity { High = "HIGH", Medium = "MEDIUM", Low = "LOW" } }

str_enum! { pub enum TerminalStatus { Won = "won", Lost = "lost" } }

/// A deal that ended, and when.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct FollowupTerminalDeal {
    pub status: TerminalStatus,
    pub at: Option<Instant>,
}

/// The deal the follow-up is for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TargetDealSummary {
    pub id: Option<i64>,
    pub status_label: String,
    pub is_terminal: bool,
    pub terminal_status: Option<TerminalStatus>,
}

/// Another deal of the same customer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OtherDealSummary {
    pub status_label: String,
    pub is_terminal: bool,
}

/// Everything the cadence, prompt and validators know about a customer:
/// contact history, pressure per channel, holds, and the project facts.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
#[allow(clippy::struct_excessive_bools)]
pub struct FollowupContactPressure {
    pub days_since_last_customer_reply: Option<f64>,
    pub days_since_last_outbound: Option<f64>,
    pub unanswered_sms_since_last_reply: f64,
    pub unanswered_email_since_last_reply: f64,
    pub unanswered_total_since_last_reply: f64,
    pub outbound_last7_days: f64,
    pub outbound_last30_days: f64,
    pub outbound_last90_days: f64,
    pub average_gap_last5_outbounds: Option<f64>,
    pub customer_requested_followup_at: Option<Instant>,
    pub crm_hold_until: Option<Instant>,
    pub last_customer_message_at: Option<Instant>,
    pub last_customer_message_summary: Option<String>,
    pub latest_inbound_event_at: Option<Instant>,
    pub latest_meaningful_customer_message_at: Option<Instant>,
    pub latest_terminal_status_at: Option<Instant>,
    pub latest_terminal_status: Option<TerminalStatus>,
    pub customer_state: CustomerFollowupState,
    pub customer_intent: CustomerIntent,
    pub global_contact_pressure: ContactPressureLevel,
    pub sms_contact_pressure: ContactPressureLevel,
    pub email_contact_pressure: ContactPressureLevel,
    pub phone_contact_pressure: ContactPressureLevel,
    pub sms_channel_state: ChannelAttemptState,
    pub email_channel_state: ChannelAttemptState,
    pub phone_channel_state: ChannelAttemptState,
    pub used_followup_angles: Vec<FollowupAngle>,
    pub quote_requested: bool,
    pub quote_evidence: bool,
    pub quote_created: bool,
    pub quote_sent: bool,
    pub quote_received_or_discussed: bool,
    pub quote_evidence_source: EvidenceSource,
    pub quote_evidence_confidence: EvidenceConfidence,
    pub quote_expiration_evidence: Option<String>,
    pub quote_expiration_date: Option<Instant>,
    pub quote_type: Option<QuoteType>,
    pub explicit_customer_request: Option<String>,
    pub explicit_customer_request_kind: Option<ExplicitCustomerRequestKind>,
    pub needs_sink: bool,
    pub sink_required: bool,
    pub sink_source: Option<SinkSource>,
    pub needs_sink_from_company: bool,
    pub backsplash_known: bool,
    pub backsplash_type: BacksplashType,
    pub stone_backsplash_required: Option<bool>,
    pub backsplash_finish: BacksplashFinish,
    pub backsplash_material: Option<String>,
    pub backsplash_description: Option<String>,
    pub uncertain_material_direction: bool,
    pub material_direction_status: MaterialDirectionStatus,
    pub preferred_material: Option<String>,
    pub did_not_find_suitable_option: bool,
    pub rejected_previous_options: bool,
    pub does_not_like_current_selection: bool,
    pub crm_deal_stage: Option<String>,
    pub operational_contact_state: OperationalContactState,
    pub countertop_preferences: Vec<String>,
    pub cabinet_preferences: Vec<String>,
    pub unfinished_next_step_kind: Option<UnfinishedNextStepKind>,
    pub timeline_evidence_source: EvidenceSource,
    pub timeline_evidence_confidence: EvidenceConfidence,
    pub project_scope_source: EvidenceSource,
    pub project_scope_confidence: EvidenceConfidence,
    pub construction_delay_source: EvidenceSource,
    pub construction_delay_confidence: EvidenceConfidence,
    pub financial_delay_source: EvidenceSource,
    pub financial_delay_confidence: EvidenceConfidence,
    pub followup_starting_phase: FollowupPhase,
    pub customer_replied_after_last_outbound: bool,
    pub latest_inbound_type: Option<InboundMessageType>,
    pub latest_inbound_sender: Option<String>,
    pub matched_customer_identity: bool,
    pub reply_classification_confidence: Option<ReplyClassificationConfidence>,
    pub reply_classification_reason: Option<String>,
    pub original_timeline_evidence: Option<String>,
    pub known_material: Option<String>,
    pub quoted_material_context: Option<String>,
    pub favorite_material: Option<String>,
    pub known_scope: Option<String>,
    pub unfinished_next_step: Option<String>,
    pub unfinished_possibly_satisfied: bool,
    pub construction_dependency_evidence: Option<String>,
    pub communication_history_status: CommunicationHistoryStatus,
    pub relationship_state: RelationshipState,
    pub relationship_age: RelationshipAge,
    pub engagement_state: EngagementState,
    pub deal_state: DealState,
    pub followup_sequence_status: FollowupSequenceStatus,
    pub latest_meaningful_customer_intent: Option<MeaningfulCustomerIntent>,
    pub additional_customer_intents: Vec<MeaningfulCustomerIntent>,
    pub last_conversational_rep_reply_at: Option<Instant>,
    pub customer_reply_handled: bool,
    pub customer_waiting_for_human_reply: bool,
    /// The next step belongs to a person.
    pub rep_owned_next_step: Option<RepOwnedNextStep>,
    pub sms_available: bool,
    pub email_available: bool,
    pub phone_available: bool,
    pub known_package_interest: Option<String>,
    pub decision_obstacle_evidence: Option<String>,
    pub previous_sequence_status: Option<FollowupSequenceStatus>,
    pub sequence_generated_at: Option<Instant>,
    pub latest_outbound_type: Option<SalespersonOutboundType>,
    pub historical_material_preferences: Vec<String>,
    pub material_preferences: Vec<String>,
    pub current_confirmed_material: Option<String>,
    pub material_selection_history: bool,
    pub material_selection_evidence: bool,
    pub material_selection_evidence_source: EvidenceSource,
    pub deal_product_scope: DealProductScope,
    pub target_deal_id: Option<i64>,
    pub target_deal_status: Option<String>,
    pub other_deals: Vec<OtherDealSummary>,
    pub multi_project_customer: bool,
    pub other_project_markers: Vec<String>,
    pub target_project_address: Option<String>,
    pub other_deal_ids: Vec<i64>,
    pub deal_stage_hold: bool,
    pub hard_contact_hold_until: Option<Instant>,
    pub activity_actor: Option<ActivityActor>,
    pub activity_object: Option<String>,
    pub pricing_change_valid_until: Option<Instant>,
    pub hold_reason_type: Option<HoldReasonType>,
    pub hold_reason_source: Option<HoldReasonSource>,
    pub hold_reason_evidence: Option<String>,
    pub hold_reason_at: Option<Instant>,
    pub hold_reason_confidence: Option<EvidenceConfidence>,
    pub hold_source_type: Option<HoldSourceType>,
    pub last_conversation_reply: Option<String>,
    pub last_customer_originated_information: Option<String>,
    pub sms_channel_status: SmsChannelStatus,
    pub email_channel_status: EmailChannelStatus,
    pub phone_channel_status: PhoneChannelStatus,
    pub has_relationship_history: bool,
    pub contact_resume_mode: ContactResumeMode,
    pub automation_contact_status: AutomationContactStatus,
    pub sms_permission: ChannelPermission,
    pub email_permission: ChannelPermission,
    pub phone_permission: ChannelPermission,
    pub project_details: Vec<String>,
    pub project_areas: Vec<ProjectAreaState>,
    pub timeline_contamination_detected: bool,
    pub excluded_event_count: f64,
    pub identity_conflict_count: f64,
    pub system_event_count: f64,
    pub unmatched_event_count: f64,
    pub budget_target_approx: Option<f64>,
    pub budget_known: bool,
    pub budget_constraint: bool,
    pub budget_obstacle_evidence: Option<String>,
    pub budget_evidence: bool,
    pub budget_evidence_source: EvidenceSource,
    pub budget_evidence_confidence: EvidenceConfidence,
    pub comparing_quotes: bool,
    pub active_decision_factors: Vec<DecisionFactor>,
    pub decision_criteria: Vec<DecisionCriterion>,
    pub decision_obstacle: DecisionObstacle,
    pub latest_customer_state: LatestCustomerState,
    pub active_unresolved_facts: Vec<String>,
    pub never_tried_sms: bool,
    pub existing_countertop_material: Option<String>,
    pub existing_countertop_removal: bool,
    pub removal_required: bool,
    pub removal_component: Option<MeasurementComponent>,
    pub customer_project_components: Vec<String>,
    pub unsupported_requested_components: Vec<String>,
    pub historical_quote_expiration_warning: bool,
    /// The CRM task points at an event that has already passed.
    pub historical_task: bool,
    pub alternative_material_offered: Option<String>,
    pub installation_requested: bool,
    pub price_context: PriceContext,
    pub material_type_preferences: Vec<String>,
    pub countertop_color_preferences: Vec<String>,
    pub estimated_ready_at: Option<Instant>,
    pub estimated_ready_at_source: EvidenceSource,
    pub estimated_ready_at_confidence: EvidenceConfidence,
    pub estimated_ready_at_precision: Option<EstimatedReadyPrecision>,
    pub estimated_ready_window_start: Option<Instant>,
    pub estimated_ready_window_end: Option<Instant>,
    pub timeline_position: Option<TimelinePosition>,
    pub project_scale: ProjectScale,
    pub approx_unit_count: Option<f64>,
    pub preferred_next_step: PreferredNextStep,
    pub preferred_next_step_source: PreferredNextStepSource,
    pub preferred_next_step_confidence: EvidenceConfidence,
    pub customer_requested_contact_method: Option<ContactMethod>,
    pub customer_requested_contact_methods: Vec<ContactMethod>,
    pub customer_resume_condition: Option<String>,
    pub edge_preferences: Vec<String>,
    pub salesperson_planned_contact_method: Option<ContactMethod>,
    pub sender_identity_conflict: bool,
    pub manual_action_required: Option<ManualActionRequired>,
    pub manual_action_description: Option<String>,
    pub manual_action_tasks: Vec<String>,
    pub manual_action_due_at: Option<Instant>,
    /// PENDING blocks the automated sequence; COMPLETED forces a recompute.
    pub manual_action_status: ManualActionStatus,
    pub manual_action_completed_at: Option<Instant>,
    pub appointment_companion: Option<AppointmentCompanion>,
    pub completed_estimate_satisfied: bool,
    pub customer_requested_contact_window_start: Option<Instant>,
    pub customer_requested_contact_window_end: Option<Instant>,
    pub planned_customer_event_type: Option<PlannedCustomerEventType>,
    pub planned_customer_event_window_start: Option<Instant>,
    pub planned_customer_event_window_end: Option<Instant>,
    pub planned_customer_event_confidence: Option<EvidenceConfidence>,
    pub planned_customer_event_date_label: Option<String>,
    pub visual_style: Option<VisualStyle>,
    pub showroom_interest: bool,
    pub countertop_measurements_status: Option<MeasurementsStatus>,
    pub project_size_raw: Option<f64>,
    pub countertop_sqft: Option<ExtractedMeasurement>,
    pub backsplash_sqft: Option<ExtractedMeasurement>,
    pub island_sqft: Option<ExtractedMeasurement>,
    pub vanity_sqft: Option<ExtractedMeasurement>,
    pub total_project_sqft: Option<ExtractedMeasurement>,
    pub unknown_sqft: Option<ExtractedMeasurement>,
    pub measurement_conflict: bool,
    pub measurement_dimensions_length_in: Option<f64>,
    pub measurement_dimensions_width_in: Option<f64>,
    pub calculated_area_from_dimensions_sqft: Option<f64>,
    pub customer_reported_area_sqft: Option<f64>,
    pub customer_reported_island_sqft: Option<f64>,
    pub measurement_raw_text: Option<String>,
    pub future_volume_sqft: Option<f64>,
    pub project_dependencies: Vec<ProjectDependency>,
    pub information_density: InformationDensity,
    pub customer_message_semantic_type: CustomerMessageSemanticType,
    pub latest_genuine_inbound_at: Option<Instant>,
    pub last_outbound_at: Option<Instant>,
    pub sequence_stop_reason: Option<FollowupSequenceStatus>,
    pub previous_sequence_stop_reason: Option<FollowupSequenceStatus>,
    pub current_sequence_stop_reason: Option<FollowupSequenceStatus>,
    pub sequence_stopped_at: Option<Instant>,
}
