//! The facts the engine reads from a customer's history (port of the types
//! of `smsFlowAiFollowupFacts.ts`).

use super::super::dates::Instant;
use crate::str_enum;
use serde::Serialize;

str_enum! { pub enum QuoteType { Countertop = "countertop", Cabinet = "cabinet", FullRemodel = "full_remodel", Unknown = "unknown" } }

str_enum! { pub enum DealProductScope {
    Cabinets = "CABINETS", Countertops = "COUNTERTOPS", CabinetsAndCountertops = "CABINETS_AND_COUNTERTOPS",
    FullRemodel = "FULL_REMODEL", Unknown = "UNKNOWN",
} }

str_enum! { pub enum ActivityActor { Customer = "customer", Salesperson = "salesperson", Unknown = "unknown" } }

str_enum! { pub enum EvidenceSource {
    CustomerMessage = "customer_message", CustomerSms = "customer_sms",
    /// The customer confirmed it by answering a specific question we asked.
    CustomerReplyToContext = "customer_reply_to_context",
    CustomerEmail = "customer_email", SalespersonNote = "salesperson_note",
    SalespersonEmail = "salesperson_email", DealRecord = "deal_record", Conversation = "conversation",
    CrmActivity = "crm_activity", CrmNote = "crm_note", None = "none",
} }

str_enum! { pub enum ProjectScale { MultiUnit = "MULTI_UNIT", SingleResidential = "SINGLE_RESIDENTIAL", Unknown = "UNKNOWN" } }

str_enum! { pub enum ContactMethod { Phone = "PHONE", Sms = "SMS", Email = "EMAIL" } }

str_enum! { pub enum PreferredNextStep {
    PhoneCall = "PHONE_CALL", Sms = "SMS", Email = "EMAIL", SendCurrentSpecials = "SEND_CURRENT_SPECIALS", None = "NONE",
} }

str_enum! { pub enum PreferredNextStepSource {
    CustomerSms = "customer_sms", CustomerEmail = "customer_email", SalespersonNote = "salesperson_note", None = "none",
} }

str_enum! { pub enum MaterialDirectionStatus {
    None = "NONE", SingleOpenOption = "SINGLE_OPEN_OPTION", MultipleOpenOptions = "MULTIPLE_OPEN_OPTIONS", Confirmed = "CONFIRMED",
} }

str_enum! { pub enum BacksplashFinish { Tile = "TILE", Stone = "STONE", None = "NONE", Unknown = "UNKNOWN" } }

str_enum! { pub enum TimelinePosition {
    BeforeExpectedDate = "BEFORE_EXPECTED_DATE", NearExpectedDate = "NEAR_EXPECTED_DATE", AfterExpectedDate = "AFTER_EXPECTED_DATE",
} }

str_enum! { pub enum AppointmentCompanion { Spouse = "spouse" } }

str_enum! { pub enum BacksplashType { None = "NONE", Full = "FULL", FourInch = "4_INCH", Unknown = "UNKNOWN" } }

str_enum! { pub enum SinkSource { Company = "COMPANY", CustomerExternal = "CUSTOMER_EXTERNAL", Unknown = "UNKNOWN" } }

str_enum! { pub enum DecisionCriterion { Price = "PRICE", Quality = "QUALITY", Timing = "TIMING", Material = "MATERIAL" } }

str_enum! { pub enum DecisionObstacle {
    Budget = "BUDGET", Price = "PRICE", PriceComparison = "PRICE_COMPARISON", Timing = "TIMING",
    MaterialFit = "MATERIAL_FIT", ProjectDependency = "PROJECT_DEPENDENCY", None = "NONE",
} }

str_enum! { pub enum EvidenceConfidence { High = "high", Medium = "medium", Low = "low" } }

/// Whether a derived fact holds, and how sure we are.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct DerivedFactEvidence {
    pub present: bool,
    pub source: EvidenceSource,
    pub confidence: EvidenceConfidence,
}

/// A fact valid only for a while (a price change).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TimeSensitiveFact {
    /// Always `pricing_change`.
    pub kind: &'static str,
    pub valid_from: Option<Instant>,
    pub valid_until: Option<Instant>,
    pub evidence: String,
}

str_enum! { pub enum DecisionFactor {
    Budget = "BUDGET", Price = "PRICE", PriceComparison = "PRICE_COMPARISON", CompetitiveComparison = "COMPETITIVE_COMPARISON",
    Quality = "QUALITY", MaterialSelection = "MATERIAL_SELECTION", Timing = "TIMING",
    SpouseOrPartnerDecision = "SPOUSE_OR_PARTNER_DECISION", ProjectDependency = "PROJECT_DEPENDENCY",
    BacksplashCost = "BACKSPLASH_COST", Unknown = "UNKNOWN",
} }

/// What the customer has and what they want instead.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MaterialReplacementRelationship {
    pub existing_material_type: Option<String>,
    pub target_material_type: Option<String>,
    pub target_color: Option<String>,
    pub evidence: String,
}

str_enum! { pub enum ManualActionStatus {
    Pending = "PENDING", Completed = "COMPLETED", Skipped = "SKIPPED", Overridden = "OVERRIDDEN", Superseded = "SUPERSEDED",
} }

str_enum! { pub enum ProjectAreaKind { Bathroom = "BATHROOM", Kitchen = "KITCHEN", Other = "OTHER" } }

str_enum! { pub enum AreaReadiness { Waiting = "WAITING", Active = "ACTIVE", Unknown = "UNKNOWN" } }

/// One room of the project and whether it is ready.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct ProjectAreaState {
    pub area: ProjectAreaKind,
    pub readiness: AreaReadiness,
}

str_enum! { pub enum LatestCustomerState {
    Waiting = "WAITING", Evaluating = "EVALUATING", Active = "ACTIVE", FutureProject = "FUTURE_PROJECT",
    Planning = "PLANNING", Delayed = "DELAYED", NearTermProject = "NEAR_TERM_PROJECT", PriceObjection = "PRICE_OBJECTION",
    Declined = "DECLINED", PausedIndefinite = "PAUSED_INDEFINITE", WaitingOnDependency = "WAITING_ON_DEPENDENCY",
    TemplateApproved = "TEMPLATE_APPROVED", Unknown = "UNKNOWN",
} }

str_enum! { pub enum PlannedCustomerEventType {
    ShowroomVisit = "SHOWROOM_VISIT", Measurement = "MEASUREMENT", CabinetDelivery = "CABINET_DELIVERY",
    VacationReturn = "VACATION_RETURN", FloorInstallation = "FLOOR_INSTALLATION",
} }

str_enum! { pub enum MeasurementComponent {
    Countertop = "COUNTERTOP", Backsplash = "BACKSPLASH", Island = "ISLAND", Vanity = "VANITY",
    TotalProject = "TOTAL_PROJECT", Unknown = "UNKNOWN",
} }

/// A measurement found in the history.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExtractedMeasurement {
    pub value: f64,
    pub measurement_type: MeasurementComponent,
    pub source: EvidenceSource,
    pub source_text: String,
    pub confidence: EvidenceConfidence,
}

str_enum! { pub enum ProjectDependencyType {
    CabinetLeveling = "CABINET_LEVELING", CabinetInstall = "CABINET_INSTALL", FloorInstallation = "FLOOR_INSTALLATION",
    Plumbing = "PLUMBING", Drywall = "DRYWALL", Other = "OTHER",
} }

str_enum! { pub enum DependencyStatus { Incomplete = "INCOMPLETE", Complete = "COMPLETE", Unknown = "UNKNOWN" } }

/// Work that must happen before the countertops.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProjectDependency {
    #[serde(rename = "type")]
    pub kind: ProjectDependencyType,
    pub status: DependencyStatus,
    pub evidence: String,
}

str_enum! { pub enum VisualStyle { SimpleLowMovement = "SIMPLE_LOW_MOVEMENT" } }

str_enum! { pub enum PriceContext {
    BudgetTarget = "BUDGET_TARGET", BudgetMax = "BUDGET_MAX", PackagePrice = "PACKAGE_PRICE", QuoteAmount = "QUOTE_AMOUNT",
    PromotionPrice = "PROMOTION_PRICE", MaterialPrice = "MATERIAL_PRICE", EstimateAmount = "ESTIMATE_AMOUNT",
    UnknownPrice = "UNKNOWN_PRICE", None = "NONE",
} }

str_enum! { pub enum EstimatedReadyPrecision { Approximate = "approximate", Exact = "exact", Minimum = "minimum" } }

str_enum! { pub enum ExplicitCustomerRequestKind {
    EstimatePricing = "estimate_pricing", SquareFootage = "square_footage", PricePerSqft = "price_per_sqft",
    OnsiteQuote = "onsite_quote", QuoteRequest = "quote_request", EstimateRequest = "estimate_request",
    FabricationRequest = "fabrication_request", ProductAvailability = "product_availability", Other = "other",
} }

str_enum! { pub enum UnfinishedNextStepKind {
    CustomerRequest = "customer_request", SalespersonTask = "salesperson_task", InHomeEstimate = "IN_HOME_ESTIMATE",
    ScheduleMeasurement = "schedule_measurement", CustomerMeasurements = "CUSTOMER_MEASUREMENTS",
    ShowroomAppointment = "SHOWROOM_APPOINTMENT",
} }

str_enum! { pub enum ManualActionRequired { PhoneCall = "PHONE_CALL", SalesTask = "SALES_TASK" } }

str_enum! { pub enum MeasurementsStatus { Provided = "PROVIDED", Needed = "NEEDED" } }

/// Everything the engine knows about the customer's project.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
#[allow(clippy::struct_excessive_bools)]
pub struct FollowupFacts {
    pub quote_type: Option<QuoteType>,
    pub quote_requested: bool,
    pub quote_evidence: bool,
    pub quote_created: bool,
    pub quote_sent: bool,
    pub quote_received_or_discussed: bool,
    pub quote_evidence_source: EvidenceSource,
    pub quote_evidence_confidence: EvidenceConfidence,
    pub quote_expiration_evidence: Option<String>,
    pub quote_expiration_date: Option<Instant>,
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
    pub known_material: Option<String>,
    pub quoted_material_context: Option<String>,
    pub historical_material_preferences: Vec<String>,
    pub material_preferences: Vec<String>,
    pub favorite_material: Option<String>,
    pub countertop_preferences: Vec<String>,
    pub cabinet_preferences: Vec<String>,
    pub current_confirmed_material: Option<String>,
    pub material_selection_history: bool,
    pub material_selection_evidence: bool,
    pub material_selection_evidence_source: EvidenceSource,
    pub original_timeline_evidence: Option<String>,
    pub timeline_evidence_source: EvidenceSource,
    pub timeline_evidence_confidence: EvidenceConfidence,
    pub known_scope: Option<String>,
    pub project_scope_source: EvidenceSource,
    pub project_scope_confidence: EvidenceConfidence,
    pub unfinished_next_step: Option<String>,
    pub unfinished_next_step_kind: Option<UnfinishedNextStepKind>,
    pub unfinished_possibly_satisfied: bool,
    pub construction_dependency_evidence: Option<String>,
    pub construction_delay_source: EvidenceSource,
    pub construction_delay_confidence: EvidenceConfidence,
    pub budget_known: bool,
    pub budget_constraint: bool,
    pub budget_obstacle_evidence: Option<String>,
    pub budget_evidence: bool,
    pub budget_evidence_source: EvidenceSource,
    pub budget_evidence_confidence: EvidenceConfidence,
    pub financial_delay_source: EvidenceSource,
    pub financial_delay_confidence: EvidenceConfidence,
    pub deal_product_scope: DealProductScope,
    pub activity_actor: Option<ActivityActor>,
    pub activity_object: Option<String>,
    pub activity_confidence: Option<EvidenceConfidence>,
    pub manual_action_required: Option<ManualActionRequired>,
    pub manual_action_description: Option<String>,
    pub manual_action_tasks: Vec<String>,
    pub time_sensitive_facts: Vec<TimeSensitiveFact>,
    pub known_package_interest: Option<String>,
    pub decision_obstacle_evidence: Option<String>,
    pub decision_criteria: Vec<DecisionCriterion>,
    pub decision_obstacle: DecisionObstacle,
    pub project_details: Vec<String>,
    pub project_areas: Vec<ProjectAreaState>,
    pub budget_target_approx: Option<f64>,
    pub comparing_quotes: bool,
    pub active_decision_factors: Vec<DecisionFactor>,
    pub latest_customer_state: LatestCustomerState,
    pub active_unresolved_facts: Vec<String>,
    pub existing_countertop_material: Option<String>,
    pub existing_countertop_removal: bool,
    pub removal_required: bool,
    /// A `MeasurementComponent` value or `UNKNOWN`.
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
}
