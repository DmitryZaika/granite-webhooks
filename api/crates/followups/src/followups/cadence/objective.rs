//! What a generated message actually asks, and whether that matches what the
//! planner wanted (`classifyGeneratedCopyObjective`, `copyObjectiveMatchesPlanned`
//! and the copy approaches of `smsFlowAiCadence.ts`).

use super::super::facts::timing::body_mentions_planned_event_date;
use super::super::pressure::FollowupAngle as A;
use super::types::{CopySemanticObjective, FollowupSemanticObjective};
use crate::{re, str_enum};
use regex::Regex;

fn named_stone_re() -> &'static Regex {
    re!(
        r"\b(dekton|solitaj|ivoritaj|calacatta|taj mahal|bianco|quartzite|silestone|cambria|caesarstone|neolith|msi quartz|vincent black|black mist|black pearl|porcelain slab|soapstone)\b",
        "i"
    )
}

/// Explicit budget language: it keeps a budget ask BUDGET even next to timing words.
fn explicit_budget_re() -> &'static Regex {
    re!(
        r"\bbudget\b|too expensive|price range|cost options?|cost range|comfortable spending",
        "i"
    )
}

/// Naming the next step outright (scheduling, a visit, sending plans).
fn explicit_next_step_re() -> &'static Regex {
    re!(
        r"showroom|schedul|measure|come by|stop by|on-?site|book (a|an|the)\b|set up (a|an|the|something)\b|set something up|(find|pick|grab) a time|on the calendar|next step|send (me |over )?the (plans?|drawings?|dimensions?|measurements?)",
        "i"
    )
}

/// Low-pressure framing keeps a passing quote or material mention NURTURE.
fn low_pressure_re() -> &'static Regex {
    re!(
        r"whenever (you.re ready|that works|you get a chance)|when(ever)? it.s (convenient|helpful|useful)|comes back into focus|wanted to reconnect|circl(e|ing) back|leave the door|no pressure|no rush|take your time|in your own time|here if you|can take time",
        "i"
    )
}

pub fn body_looks_like_named_material_ask(body: &str) -> bool {
    let text = body.to_lowercase();
    named_stone_re().is_match(&text)
        || re!(r"material (direction|preference|option|choice)|still (considering|among).{0,48}material|named (stone|material)", "i").is_match(&text)
}

/// The company name is not a material or product claim.
pub fn followup_body_without_vendor_names(body: &str) -> String {
    re!(r"\bgranite depot\b", "gi")
        .replace_all(body, "")
        .into_owned()
}

/// What a generated message asks about.
#[allow(clippy::too_many_lines)]
pub fn classify_generated_copy_objective(body: &str) -> CopySemanticObjective {
    use CopySemanticObjective as C;
    let without_vendor = followup_body_without_vendor_names(body);
    let text = without_vendor.to_lowercase();
    if re!(
        r"close the loop|go another direction|still want me to (reach|follow)|should i close|close (out )?(the |your )?(file|project|inquiry|follow-?up)|i('ll| will) stop reaching|stop reaching out|no response is needed|pause (my |this )?follow-?ups?|close this out|close out",
        "i"
    )
    .is_match(&text)
    {
        return C::CloseLoop;
    }
    if re!(
        r"has the timing|timing (changed|shifted|works)|still on track|when (are you|will you be) ready|timeline (get )?pushed|still the plan|cabinets? (are |leveled|level)(in|installed|done|finished)|floors? (are |in|installed)|flooring|back on your radar|project back on|still planning|timing you mentioned|timeframe|\bon hold\b|still (active|open|part of)|getting closer|ways out",
        "i"
    )
    .is_match(&text)
    {
        return C::Timing;
    }
    let material_choice = re!(
        r"quartz or granite|granite or quartz|which material|material type|leaning toward|prefer (quartz|granite|soapstone)|still interested in (quartz|granite)",
        "i"
    )
    .is_match(&text);
    let named_material_ask = body_looks_like_named_material_ask(&without_vendor);
    if re!(
        r"revisit (the |your )?\w+ quote|revisit that quote|still an option|still something you('re| are)? (considering|looking|interested)|different material now",
        "i"
    )
    .is_match(&text)
    {
        return C::MaterialType;
    }
    let quote_review = re!(
        r"go over (the )?(quotes?|estimates?)|review (the )?(quotes?|estimates?)|the estimate together",
        "i"
    );
    if (named_material_ask
        || material_choice
        || re!(r"quartz|soapstone|\bgranite\b", "i").is_match(&text))
        && (named_material_ask || material_choice || !quote_review.is_match(&text))
    {
        return C::MaterialType;
    }
    if re!(r"\bcolor\b|white or|gray or|charcoal|dark green", "i").is_match(&text) {
        return C::Color;
    }
    if re!(
        r"go over (the )?(quotes?|estimates?)|review (the )?(quotes?|estimates?)|the estimate together|the numbers|pricing we|review the pricing|\bquotes?\b",
        "i"
    )
    .is_match(&text)
    {
        return C::Quote;
    }
    if re!(r"\bbudget\b|too expensive|price range", "i").is_match(&text) {
        return C::Budget;
    }
    if re!(
        r"kitchen, bathroom|kitchen or bathroom|what part of the project|what area|which area|project or area|what project|project type|type of project|what (kind|type) of project",
        "i"
    )
    .is_match(&text)
    {
        return C::Scope;
    }
    let without_review_look =
        re!(r"\b(tak(e|ing)|hav(e|ing)) a look\b", "gi").replace_all(&text, " ");
    if re!(r"\blook\b|style|movement|simple vs|veining", "i").is_match(&without_review_look) {
        return C::Design;
    }
    if re!(
        r"schedul|measure|showroom|come by|stop by|dimensions available|have the (measurements|dimensions)|quick (call|visit)|on-site|set up (a|an|the|something)\b|set something up|(find|pick|grab) a time|on the calendar",
        "i"
    )
    .is_match(&text)
    {
        return C::NextStep;
    }
    C::Nurture
}

/// The instruction line for a copy objective.
pub const fn copy_objective_instruction(objective: CopySemanticObjective) -> &'static str {
    use CopySemanticObjective as C;
    match objective {
        C::Timing => {
            "TIMING — timing, readiness, or whether the project is still active. Not material, quote, or scope."
        }
        C::MaterialType => {
            "MATERIAL_TYPE — material type or named stone direction. Revisiting a prior quote counts here."
        }
        C::Color => "COLOR — color direction within a material family.",
        C::Quote => "QUOTE — quote status, pricing review, or numbers already sent.",
        C::Budget => "BUDGET — budget obstacle or price concern.",
        C::Scope => "SCOPE — kitchen vs bathroom, cabinets vs countertops, or project type.",
        C::Design => "DESIGN — visual style, veining, or look preference.",
        C::NextStep => "NEXT_STEP — scheduling, measurement, showroom, or concrete next action.",
        C::CloseLoop => {
            "CLOSE_LOOP — permission to stop following up or close the file. Do not ask them to reply with a keyword."
        }
        C::Nurture => {
            "NURTURE — low-pressure reactivation in plain human language. Do not use “just reply …” or AI fluff (“radar”, “door open”, “back seat”, “no assumptions”)."
        }
    }
}

/// What the planner meant a step to do.
#[derive(Debug, Clone, Default)]
pub struct PlannedIntent<'a> {
    pub angle: Option<&'a str>,
    pub goal: Option<&'a str>,
    pub subangle: Option<&'a str>,
    pub must_do: Option<&'a str>,
}

/// Whether the generated copy does what was planned (with the tolerated
/// neighbours: a design ask naming a color, a nurture with a passing quote...).
#[allow(clippy::too_many_lines, clippy::cognitive_complexity)]
pub fn copy_objective_matches_planned(
    planned: CopySemanticObjective,
    body: &str,
    intent: Option<&PlannedIntent<'_>>,
) -> bool {
    use CopySemanticObjective as C;
    let classified = classify_generated_copy_objective(body);
    if let Some(caps) = intent.and_then(|i| i.must_do).and_then(|must| {
        re!(
            r"\b(january|february|march|april|may|june|july|august|september|october|november|december)\s+(\d{1,2})(?:st|nd|rd|th)?\b",
            "i"
        )
        .captures(must)
    }) {
        if !body_mentions_planned_event_date(body, &format!("{} {}", &caps[1], &caps[2])) {
            return false;
        }
    }
    if classified == planned {
        return true;
    }
    let intent_text = format!(
        "{} {} {} {}",
        intent.and_then(|i| i.must_do).unwrap_or(""),
        intent.and_then(|i| i.angle).unwrap_or(""),
        intent.and_then(|i| i.goal).unwrap_or(""),
        intent.and_then(|i| i.subangle).unwrap_or("")
    )
    .to_lowercase();
    if planned == C::Design && matches!(classified, C::Color | C::Design | C::MaterialType) {
        if re!(r"cabinet layout|review cabinet|door layout|hinge").is_match(&intent_text) {
            return false;
        }
        let visual_plan = intent.is_none()
            || intent.and_then(|i| i.angle) == Some("design_help")
            || re!(r"design|visual|look|vein|style|character|finish|color").is_match(&intent_text);
        if !visual_plan {
            return false;
        }
        let must_do = intent.and_then(|i| i.must_do).unwrap_or("").to_lowercase();
        if !must_do.is_empty()
            && !re!(r"visual|look|vein|style|character|finish|color|design").is_match(&must_do)
        {
            return false;
        }
        return re!(
            r"\bcolor\b|vein|look|style|movement|finish|contrast|white or|gray or|subtle|bold",
            "i"
        )
        .is_match(body);
    }
    if planned == C::Scope
        && classified == C::Nurture
        && re!(
            r"project (type|still relevant)|type of project|kitchen or bathroom|what (kind|type) of project|what area|which area|what project|project or area",
            "i"
        )
        .is_match(body)
    {
        return true;
    }
    if planned == C::MaterialType
        && classified == C::Nurture
        && body_looks_like_named_material_ask(body)
    {
        return true;
    }
    if planned == C::MaterialType
        && classified == C::Quote
        && re!(
            r"revisit|still (an option|something you|considering)|different material",
            "i"
        )
        .is_match(body)
    {
        return true;
    }
    if planned == C::Timing
        && classified == C::Nurture
        && re!(
            r"still (the plan|planning|on track|working on|active|open|part of)|timing|timeframe|rader|back on|project (still|changed)|ready|\bon hold\b|check back|later time|circle back|down the road|happening right now",
            "i"
        )
        .is_match(body)
    {
        return true;
    }
    if planned == C::Nurture && matches!(classified, C::Timing | C::Scope) {
        return true;
    }
    if planned == C::Nurture
        && matches!(classified, C::Quote | C::MaterialType)
        && low_pressure_re().is_match(body)
        && !re!(r"quartz or granite|which material|leaning toward", "i").is_match(body)
    {
        return true;
    }
    if planned == C::CloseLoop
        && classified == C::Nurture
        && re!(r"close|another direction|reach(ing)? out|follow up|stop reaching|no response is needed", "i").is_match(body)
    {
        return true;
    }
    if planned == C::Timing
        && classified == C::MaterialType
        && re!(
            r"still active|timing works|on hold|timing (changed|shifted)|still (the plan|planning)",
            "i"
        )
        .is_match(body)
    {
        return true;
    }
    if planned == C::Quote
        && classified == C::Timing
        && re!(r"\bquote\b|the numbers|pricing|estimate", "i").is_match(body)
    {
        return true;
    }
    if planned == C::NextStep
        && classified == C::Quote
        && (re!(
            r"send me|locations|addresses|schedule|measure|visit|dimensions",
            "i"
        )
        .is_match(body)
            || explicit_next_step_re().is_match(body))
    {
        return true;
    }
    if planned == C::NextStep
        && matches!(classified, C::Nurture | C::Scope | C::Design)
        && explicit_next_step_re().is_match(body)
    {
        return true;
    }
    planned == C::Budget
        && matches!(classified, C::Timing | C::Quote)
        && explicit_budget_re().is_match(body)
}

str_enum! { pub enum CopyApproach {
    Reactivation = "reactivation", ProjectStatus = "project_status", HelpfulInformation = "helpful_information",
    DecisionObstacle = "decision_obstacle", MaterialHelp = "material_help", TimingCheck = "timing_check",
    SoftClose = "soft_close", UnfinishedRequest = "unfinished_request", EasyRestart = "easy_restart",
    ScopeDiscovery = "scope_discovery", QuoteReview = "quote_review", DesignHelp = "design_help",
} }

/// The writing approach for a planned step.
pub fn planner_copy_approach(
    angle: A,
    subangle: Option<&str>,
    semantic_objective: Option<FollowupSemanticObjective>,
) -> CopyApproach {
    use CopyApproach as C;
    let sub = subangle.unwrap_or("").to_lowercase();
    match angle {
        A::CloseLoop => return C::SoftClose,
        A::LongTermNurture if re!(r"easy_restart").is_match(&sub) => return C::EasyRestart,
        A::LongTermNurture => return C::ProjectStatus,
        A::Reactivation | A::HoldReactivation => return C::Reactivation,
        A::HelpfulInformation => return C::HelpfulInformation,
        A::BudgetObstacle | A::DecisionObstacle => return C::DecisionObstacle,
        A::MaterialPreference | A::PreviousStoneSelection => return C::MaterialHelp,
        _ => {}
    }
    if matches!(angle, A::ProjectTiming | A::OriginalTimeline)
        || semantic_objective == Some(FollowupSemanticObjective::ConfirmProjectActive)
    {
        return C::TimingCheck;
    }
    match angle {
        A::ProjectStatus => C::ProjectStatus,
        A::ProjectScope => C::ScopeDiscovery,
        A::QuoteStatus | A::QuoteReview => C::QuoteReview,
        A::DesignHelp => C::DesignHelp,
        A::InitialInterest | A::EstimateOffer | A::PreferredNextStep => C::UnfinishedRequest,
        _ if semantic_objective == Some(FollowupSemanticObjective::ScheduleNextStep) => {
            C::UnfinishedRequest
        }
        _ => C::ProjectStatus,
    }
}

/// An objective-compatibility change that lowered retries but also lowered
/// semantic accuracy failed.
pub fn objective_compatibility_optimization_failed(
    before_retry_rate: f64,
    after_retry_rate: f64,
    before_semantic_accuracy: f64,
    after_semantic_accuracy: f64,
) -> bool {
    after_retry_rate < before_retry_rate && after_semantic_accuracy < before_semantic_accuracy
}
