//! Which copy objectives the known facts support
//! (`customerIntelligence/objectiveEligibility.ts`).

use super::super::cadence::types::CopySemanticObjective;
use super::super::facts::quotes::quote_evidence_is_verified;
use super::super::facts::{
    DealProductScope, DecisionFactor, FollowupFacts, MeasurementsStatus, UnfinishedNextStepKind,
    VisualStyle,
};
use crate::re;

/// The facts that decide which objectives a step may pursue.
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)]
pub struct ObjectiveEligibilityFacts {
    pub deal_product_scope: DealProductScope,
    pub named_materials: Vec<String>,
    pub color_preferences: Vec<String>,
    pub visual_style: Option<VisualStyle>,
    pub known_package_interest: Option<String>,
    pub has_quote_state: bool,
    pub measurements_need_resolution: bool,
    pub has_budget_evidence: bool,
    pub has_dependency: bool,
    pub known_scope: bool,
    pub explicit_customer_request: Option<String>,
    pub active_unresolved_facts: Vec<String>,
    pub material_selection_history: bool,
    pub information_density_low: bool,
}

/// The step being checked (angle, subangle, goal).
#[derive(Debug, Clone, Copy, Default)]
pub struct ObjectivePlanProbe<'a> {
    pub angle: Option<&'a str>,
    pub subangle: Option<&'a str>,
    pub goal: Option<&'a str>,
}

fn unique_names<'a>(values: impl IntoIterator<Item = Option<&'a String>>) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for value in values.into_iter().flatten() {
        let trimmed = super::super::js::trim(value);
        if trimmed.is_empty() || !seen.insert(trimmed.to_lowercase()) {
            continue;
        }
        out.push(trimmed.to_string());
    }
    out
}

pub fn named_materials_from_facts(f: &FollowupFacts) -> Vec<String> {
    unique_names(
        f.material_preferences
            .iter()
            .chain(&f.material_type_preferences)
            .chain(&f.countertop_preferences)
            .map(Some)
            .chain([
                f.favorite_material.as_ref(),
                f.current_confirmed_material.as_ref(),
                f.known_material.as_ref(),
                f.quoted_material_context.as_ref(),
                f.preferred_material.as_ref(),
            ]),
    )
}

fn measurements_from_facts(f: &FollowupFacts) -> bool {
    f.countertop_measurements_status == Some(MeasurementsStatus::Needed)
        || f.measurement_conflict
        || matches!(
            f.unfinished_next_step_kind,
            Some(
                UnfinishedNextStepKind::CustomerMeasurements
                    | UnfinishedNextStepKind::ScheduleMeasurement
            )
        )
        || f.active_unresolved_facts
            .iter()
            .any(|item| re!(r"measurement|dimension|square footage", "i").is_match(item))
}

pub fn eligibility_from_facts(f: &FollowupFacts) -> ObjectiveEligibilityFacts {
    let named_materials = named_materials_from_facts(f);
    let unknown = f.deal_product_scope == DealProductScope::Unknown;
    let has_quote_state = quote_evidence_is_verified(
        f.quote_evidence,
        f.quote_evidence_source,
        f.quote_evidence_confidence,
    );
    let measurements_need_resolution = measurements_from_facts(f);
    let has_budget_evidence = f.budget_obstacle_evidence.is_some()
        || f.budget_target_approx.is_some()
        || f.active_decision_factors.contains(&DecisionFactor::Budget);
    let has_dependency =
        f.construction_dependency_evidence.is_some() || !f.project_dependencies.is_empty();
    ObjectiveEligibilityFacts {
        deal_product_scope: f.deal_product_scope,
        information_density_low: unknown
            && named_materials.is_empty()
            && !has_quote_state
            && f.known_package_interest.is_none()
            && !measurements_need_resolution
            && !has_budget_evidence
            && !has_dependency,
        named_materials,
        color_preferences: f.countertop_color_preferences.clone(),
        visual_style: f.visual_style,
        known_package_interest: f.known_package_interest.clone(),
        has_quote_state,
        measurements_need_resolution,
        has_budget_evidence,
        has_dependency,
        known_scope: f.known_scope.as_deref().is_some_and(|s| !s.is_empty()) || !unknown,
        explicit_customer_request: f.explicit_customer_request.clone(),
        active_unresolved_facts: f.active_unresolved_facts.clone(),
        material_selection_history: f.material_selection_history,
    }
}

fn is_measurement_angle(probe: Option<&ObjectivePlanProbe<'_>>) -> bool {
    let sub = format!(
        "{} {}",
        probe.and_then(|p| p.subangle).unwrap_or(""),
        probe.and_then(|p| p.goal).unwrap_or("")
    )
    .to_lowercase();
    re!(r"measurement|dimension|sqft|square").is_match(&sub)
}

fn filled(value: Option<&String>) -> bool {
    value.is_some_and(|v| !v.is_empty())
}

/// Whether the facts let a step pursue this copy objective.
pub fn is_objective_supported(
    objective: CopySemanticObjective,
    f: &ObjectiveEligibilityFacts,
    probe: Option<&ObjectivePlanProbe<'_>>,
) -> bool {
    // A step about stones they already looked at needs only that history.
    if probe.and_then(|p| p.angle) == Some("previous_stone_selection") {
        return f.material_selection_history;
    }
    objective_requirement_failure(objective, f, probe).is_none()
}

/// Why the objective is not supported (`OBJECTIVE_NOT_SUPPORTED_BY_FACTS` or
/// `INVALID_PLAN`, and the detail), or `None` when it is.
#[allow(clippy::too_many_lines)]
pub fn objective_requirement_failure(
    objective: CopySemanticObjective,
    f: &ObjectiveEligibilityFacts,
    probe: Option<&ObjectivePlanProbe<'_>>,
) -> Option<(&'static str, &'static str)> {
    use CopySemanticObjective as O;
    const NOT_SUPPORTED: &str = "OBJECTIVE_NOT_SUPPORTED_BY_FACTS";
    const INVALID: &str = "INVALID_PLAN";
    let angle = probe.and_then(|p| p.angle).unwrap_or("");
    let unknown = f.deal_product_scope == DealProductScope::Unknown;
    let no_materials = f.named_materials.is_empty();
    if angle == "previous_stone_selection" && !f.material_selection_history {
        return Some((
            NOT_SUPPORTED,
            "previous_stone_selection requires material selection history",
        ));
    }
    if angle == "package_interest" && !filled(f.known_package_interest.as_ref()) {
        return Some((
            NOT_SUPPORTED,
            "PACKAGE_INTEREST requires knownPackageInterest",
        ));
    }
    if objective == O::MaterialType {
        let generic_education_allowed = f.known_scope && angle == "helpful_information";
        if no_materials && !generic_education_allowed {
            return Some((
                NOT_SUPPORTED,
                "MATERIAL_TYPE requires a supported material fact or known-scope product education",
            ));
        }
        if unknown && no_materials {
            return Some((
                NOT_SUPPORTED,
                "MATERIAL_TYPE is prohibited when dealProductScope is UNKNOWN and no material facts exist",
            ));
        }
    }
    if objective == O::Color {
        if no_materials && f.color_preferences.is_empty() && f.visual_style.is_none() {
            return Some((
                NOT_SUPPORTED,
                "COLOR requires supported color, design, or material context",
            ));
        }
        if unknown && no_materials && f.color_preferences.is_empty() {
            return Some((
                NOT_SUPPORTED,
                "COLOR is prohibited when dealProductScope is UNKNOWN without independent color facts",
            ));
        }
    }
    if objective == O::Design {
        let has_visual = f.visual_style.is_some()
            || !no_materials
            || !f.color_preferences.is_empty()
            || f.known_scope;
        if unknown && !has_visual {
            return Some((
                NOT_SUPPORTED,
                "product-specific DESIGN is prohibited when dealProductScope is UNKNOWN",
            ));
        }
    }
    if objective == O::Quote && !f.has_quote_state {
        return Some((
            NOT_SUPPORTED,
            "QUOTE requires quote_requested, quote_created, quote_sent, or quote_received_or_discussed",
        ));
    }
    if is_measurement_angle(probe) && objective == O::NextStep && !f.measurements_need_resolution {
        return Some((
            NOT_SUPPORTED,
            "RESOLVE_MEASUREMENTS requires missing, incomplete, or conflicted measurements",
        ));
    }
    if angle == "project_dependency" && !f.has_dependency && f.active_unresolved_facts.is_empty() {
        return Some((
            NOT_SUPPORTED,
            "PROJECT_DEPENDENCY requires an unresolved dependency",
        ));
    }
    if objective == O::Budget && !f.has_budget_evidence {
        return Some((
            NOT_SUPPORTED,
            "BUDGET requires budget evidence, obstacle, or explicit price concern",
        ));
    }
    if (objective == O::Scope || angle == "project_scope") && f.known_scope {
        return Some((
            INVALID,
            "CLARIFY_PROJECT_TYPE is invalid when structured project type is already known",
        ));
    }
    if unknown && angle == "helpful_information" && no_materials {
        return Some((
            INVALID,
            "product education is not achievable with UNKNOWN scope and no material facts",
        ));
    }
    None
}

const ALL_COPY_OBJECTIVES: [CopySemanticObjective; 10] = [
    CopySemanticObjective::NextStep,
    CopySemanticObjective::Budget,
    CopySemanticObjective::Quote,
    CopySemanticObjective::MaterialType,
    CopySemanticObjective::Color,
    CopySemanticObjective::Design,
    CopySemanticObjective::Timing,
    CopySemanticObjective::Scope,
    CopySemanticObjective::CloseLoop,
    CopySemanticObjective::Nurture,
];

pub fn get_eligible_objectives(f: &ObjectiveEligibilityFacts) -> Vec<CopySemanticObjective> {
    use CopySemanticObjective as O;
    let with_angle = |objective, angle: &str| {
        is_objective_supported(
            objective,
            f,
            Some(&ObjectivePlanProbe {
                angle: Some(angle),
                ..Default::default()
            }),
        )
    };
    ALL_COPY_OBJECTIVES
        .into_iter()
        .filter(|objective| match objective {
            O::MaterialType => with_angle(*objective, "material_preference"),
            O::Color | O::Design => with_angle(*objective, "design_help"),
            O::Quote => with_angle(*objective, "quote_review"),
            O::Budget => with_angle(*objective, "budget_obstacle"),
            O::NextStep => {
                f.measurements_need_resolution || filled(f.explicit_customer_request.as_ref())
            }
            _ => is_objective_supported(*objective, f, None),
        })
        .collect()
}

/// JS `[...].sort()` on strings: by UTF-16 code units.
fn js_sorted(mut items: Vec<String>) -> Vec<String> {
    items.sort_by(|a, b| a.encode_utf16().cmp(b.encode_utf16()));
    items
}

/// What makes two steps ask the same thing.
pub fn objective_fingerprint(
    objective: CopySemanticObjective,
    f: &ObjectiveEligibilityFacts,
    probe: Option<&ObjectivePlanProbe<'_>>,
) -> String {
    use CopySemanticObjective as O;
    let material_key =
        js_sorted(f.named_materials.iter().map(|m| m.to_lowercase()).collect()).join(",");
    let color_key = js_sorted(
        f.color_preferences
            .iter()
            .map(|m| m.to_lowercase())
            .collect(),
    )
    .join(",");
    let angle = probe.and_then(|p| p.angle).unwrap_or("");
    let subangle = probe.and_then(|p| p.subangle).unwrap_or("");
    match objective {
        O::MaterialType | O::Color | O::Design => format!("{objective}|{material_key}|{color_key}"),
        O::Quote => format!("{objective}|quote"),
        O::Budget => format!("{objective}|budget"),
        O::NextStep if is_measurement_angle(probe) => format!("{objective}|measurements"),
        _ => format!("{objective}|{angle}|{subangle}"),
    }
}

pub fn max_grounded_step_count(f: &ObjectiveEligibilityFacts, requested: usize) -> usize {
    if f.information_density_low {
        return requested.min(2);
    }
    let grounded = get_eligible_objectives(f)
        .into_iter()
        .filter(|item| {
            !matches!(
                item,
                CopySemanticObjective::Nurture | CopySemanticObjective::CloseLoop
            )
        })
        .count();
    if grounded <= 1 {
        return requested.min(2.max(grounded + 1));
    }
    requested
}
