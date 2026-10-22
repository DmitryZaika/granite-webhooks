//! The project: details, size, measurements, edges, dependencies (part of
//! `smsFlowAiFollowupFacts.ts`).

use super::super::js::{
    js_len, js_number, js_parse_number, js_regex, js_round, js_window, squash, trim,
};
use super::materials::title_case_material;
use super::types::{
    DependencyStatus, EvidenceConfidence, EvidenceSource, ExtractedMeasurement,
    MeasurementComponent, ProjectDependency, ProjectDependencyType, ProjectScale,
};
use crate::re;

/// Project details worth mentioning (island, edges, backsplash, sink...).
#[allow(clippy::too_many_lines)]
pub fn extract_project_details(text: &str, customer_originated: &str) -> Vec<String> {
    let mut details: Vec<String> = Vec::new();
    let mut add = |value: &str| {
        if !details
            .iter()
            .any(|item| item.to_lowercase() == value.to_lowercase())
        {
            details.push(value.to_string());
        }
    };
    let customer_scope = trim(customer_originated);
    let scope_source = if customer_scope.is_empty() {
        text
    } else {
        customer_scope
    };
    if re!(r"\bisland\b", "i").is_match(scope_source) {
        add("island");
    }
    if re!(r#"3[\s"'-]?(inch|in)?\s*miter"#, "i").is_match(text)
        || re!(r"mitered (island )?edges?", "i").is_match(text)
    {
        add("3-inch mitered edge");
    }
    if re!(r"\bkitchen\b", "i").is_match(customer_scope) {
        add("kitchen");
    }
    if re!(r"\bcountertops?\b|\bcounterops\b", "i").is_match(customer_scope)
        && !re!(
            r"countertops? removal|existing countertop removal|granite removal|remove .{0,24}countertops?",
            "i"
        )
        .is_match(customer_scope)
    {
        add("countertops");
    }
    if re!(r"leathered", "i").is_match(text) {
        add("leathered finish");
    }
    if re!(r"pencil edge", "i").is_match(text) {
        add("pencil edge");
    }
    if re!(r"full[- ]?height backsplash|full backsplash", "i").is_match(text) {
        add("full-height backsplash");
    }
    if re!(
        r"subway tile|tile backsplash|backsplash will be .{0,40}tile",
        "i"
    )
    .is_match(text)
    {
        add("tile backsplash");
    } else if re!(r"no backsplash|\bno bs\b", "i").is_match(text) {
        add("no backsplash");
    }
    if re!(
        r#"4\s*["”]\s*backsplash|4.?(-| )?inch backsplash|4.? backsplash"#,
        "i"
    )
    .is_match(text)
    {
        add("4-inch backsplash");
    }
    if re!(r"backsplash under bar|to meet bar surface", "i").is_match(text) {
        add("backsplash under bar to meet bar surface");
    }
    let cabinet_only_dispose = re!(
        r"replace cabinet fronts|cabinet fronts, flooring|project type:\s*kitchen cabinetry",
        "i"
    )
    .is_match(text)
        && !re!(r"\bcountertops?\b", "i").is_match(text);
    if re!(
        r"remove (her |his |their |the )?(existing )?(granite|countertops?|counters)|remove her granite",
        "i"
    )
    .is_match(text)
        || re!(r"remove and dispose existing (granite|countertops?)", "i").is_match(text)
    {
        add("existing granite removal");
        add("existing countertop removal");
    } else if re!(r"remove and dispose:\s*yes", "i").is_match(text) && !cabinet_only_dispose {
        add("removal required");
    }
    if re!(r"simple|not busy|low movement", "i").is_match(text) {
        add("simple low-movement look");
    }
    if re!(r"showroom", "i").is_match(text) {
        add("showroom visit interest");
    }
    if let Some(volume) = extract_future_volume_sqft(text) {
        add(&format!("future volume ~{} sqft", js_number(volume)));
    }
    if let Some(per_level) = re!(r"(\d+)\s+(units? )?per level", "i")
        .captures(text)
        .and_then(|caps| caps.get(1))
    {
        add(&format!("{} units per level", per_level.as_str()));
    }
    if re!(r"phased|staged|one level at a time|level by level", "i").is_match(text) {
        add("staged/phased installation");
    }
    if re!(
        r"sink (from|form) someone else|own sink|provide .{0,20}own sink|sourcing sink elsewhere|buy a sink (from|form) someone else",
        "i"
    )
    .is_match(text)
    {
        add("customer sourcing sink elsewhere");
    }
    if re!(
        r"buy a sink from you|would like to buy a sink from you|sink from (you|us|the company|company)|i would like to buy a sink",
        "i"
    )
    .is_match(text)
    {
        add("sink from company");
    }
    if re!(
        r"provided (the )?measur(e)?ments|measur(e)?ments (for the countertops )?provided|has (the )?measur(e)?ments",
        "i"
    )
    .is_match(text)
    {
        add("countertop measurements provided");
    } else if re!(r"measurements? (still )?(needed|needed still|not done)", "i").is_match(text)
        || re!(r"need to get the measur", "i").is_match(text)
        || re!(r"get the measur(e)?ments", "i").is_match(text)
    {
        add("measurements needed");
    }
    if re!(
        r"project pictures|has (project )?pictures|sent (the )?pictures|pictures of (the )?project",
        "i"
    )
    .is_match(text)
    {
        add("project pictures");
    }
    if let Some(size) = re!(
        r#"(\d+\s*(?:'|ft|feet)\s*(?:x|by|×)\s*\d+\s*(?:'|ft|feet|"|in|inch)?)"#,
        "i"
    )
    .captures(text)
    .and_then(|caps| caps.get(1))
    {
        add(&squash(size.as_str()));
    }
    if let Some(caps) = re!(
        r"(?:around|about|approximately|approx\.?)\s+(\d+)\s+app?artments?|(?:around|about|approximately|approx\.?)\s+(\d+)\s+units?|(\d+)\s+app?artments?",
        "i"
    )
    .captures(text)
    {
        let count = caps
            .get(1)
            .or_else(|| caps.get(2))
            .or_else(|| caps.get(3))
            .and_then(|m| js_parse_number(m.as_str()));
        if let Some(count) = count.filter(|count| *count >= 2.0) {
            add(&format!("approximately {} apartments/units", js_number(count)));
        }
    }
    details
}

fn strip_non_project_numbers(text: &str) -> String {
    let mut out = re!(r"https?:\/\/\S+", "gi")
        .replace_all(text, " ")
        .into_owned();
    out = re!(r"\b\d{3}[-.)\s]?\d{3}[-.\s]?\d{4}\b", "g")
        .replace_all(&out, " ")
        .into_owned();
    out = re!(r"\$\s*[\d,]+(?:\.\d+)?", "g")
        .replace_all(&out, " ")
        .into_owned();
    out = re!(r"\b(?:target|#)\s*#?\d+\b", "gi")
        .replace_all(&out, " ")
        .into_owned();
    out = re!(r"\b\d{5}(?:-\d{4})?\b", "g")
        .replace_all(&out, " ")
        .into_owned();
    out = re!(r"\b\d{4}-\d{2}-\d{2}\b", "g")
        .replace_all(&out, " ")
        .into_owned();
    re!(r"\b\d{1,2}:\d{2}(?::\d{2})?\b", "g")
        .replace_all(&out, " ")
        .into_owned()
}

/// Whether the project is several units (apartments, condos), and how many.
pub fn extract_project_scale(text: &str) -> (ProjectScale, Option<f64>) {
    let cleaned = strip_non_project_numbers(text);
    if let Some(count) = re!(
        r"(?:one|1|this|current)\s+building.{0,40}?(\d+)\s+units?",
        "i"
    )
    .captures(&cleaned)
    .and_then(|caps| caps.get(1))
    .and_then(|m| js_parse_number(m.as_str()))
    .filter(|count| *count >= 2.0)
    {
        return (ProjectScale::MultiUnit, Some(count));
    }
    if let Some(caps) = re!(
        r"(\d+)\s+total units?|(?:around|about|approximately|approx\.?)\s+(\d+)\s+app?artments?|(?:around|about|approximately|approx\.?)\s+(\d+)\s+units?|(\d+)\s+app?artments?|total (?:of )?(\d+)\s+units?|(\d+)\s+(?:units?|kitchens?|condos?|homes?|bathrooms?)\b",
        "i"
    )
    .captures(&cleaned)
    {
        let count = (1..=6)
            .find_map(|group| caps.get(group))
            .and_then(|m| js_parse_number(m.as_str()));
        if let Some(count) = count.filter(|count| *count >= 2.0) {
            return (ProjectScale::MultiUnit, Some(count));
        }
    }
    if re!(r"\b(app?artments?|multi[- ]unit)\b", "i").is_match(&cleaned) {
        return (ProjectScale::MultiUnit, None);
    }
    (ProjectScale::Unknown, None)
}

fn source_from_measurement_context(text: &str, byte_index: usize) -> EvidenceSource {
    let prefix = js_window(text, byte_index, 80, 0);
    if re!(r"customer message:|lead details:", "i").is_match(&prefix) {
        return EvidenceSource::CustomerMessage;
    }
    if re!(r"deal note ", "i").is_match(&prefix) {
        return EvidenceSource::SalespersonNote;
    }
    if re!(r"^activity:", "im").is_match(&prefix) {
        return EvidenceSource::CrmActivity;
    }
    EvidenceSource::CustomerMessage
}

fn looks_like_future_volume_context(around: &str) -> bool {
    re!(
        r"within\s+\d+\s+months|in the (next|coming) \d+|over the next|will be ordering|i will be ordering|future (?:orders?|volume|phases?)",
        "i"
    )
    .is_match(around)
}

fn component_from_local_phrase(phrase: &str) -> MeasurementComponent {
    if re!(r"backsplash", "i").is_match(phrase) {
        return MeasurementComponent::Backsplash;
    }
    if re!(r"\bisland\b", "i").is_match(phrase) {
        return MeasurementComponent::Island;
    }
    if re!(r"vanity", "i").is_match(phrase) {
        return MeasurementComponent::Vanity;
    }
    if re!(
        r"kitchen(?:\s+tops?)?|kitchen(?:\s+countertops?)?|countertops?\b|countertop",
        "i"
    )
    .is_match(phrase)
    {
        return MeasurementComponent::Countertop;
    }
    if re!(r"\btotal\b|entire project|whole project", "i").is_match(phrase) {
        return MeasurementComponent::TotalProject;
    }
    MeasurementComponent::Unknown
}

/// Square feet the customer will order over the coming months.
pub fn extract_future_volume_sqft(text: &str) -> Option<f64> {
    re!(
        r"(?:will be |i will be |we'll be |we will be )?order(?:ing)?\s+(?:about |around |approximately |approx\.?\s*)?(\d{3,5})\s*(?:sf|sq\.?\s*ft|sqft|square feet)\b.{0,40}(?:within|in the next|over the next|in coming)",
        "i"
    )
    .captures(text)
    .and_then(|caps| caps.get(1))
    .and_then(|m| js_parse_number(m.as_str()))
    .filter(|value| *value > 0.0)
}

/// `Project size: N` (unless N is a dollar amount) and the square feet the
/// text gives, by part of the project.
#[allow(clippy::too_many_lines)]
pub fn extract_typed_measurements(text: &str) -> (Option<f64>, Vec<ExtractedMeasurement>) {
    let mut project_size_raw = re!(r"project size:\s*(\d+)", "i")
        .captures(text)
        .and_then(|caps| caps.get(1))
        .and_then(|m| js_parse_number(m.as_str()));
    if let Some(size) = project_size_raw {
        let n = js_number(size);
        let money_same_number = js_regex(
            &format!(r"\$\s*{n}\b|\b{n}\s*(?:dollars?|discount|off|credit)\b"),
            "i",
        );
        if money_same_number.is_match(text) {
            project_size_raw = None;
        }
    }
    let mut measurements: Vec<ExtractedMeasurement> = Vec::new();
    if let Some(caps) = re!(
        r"(\d+(?:\.\d+)?)\s*(?:sq\.?\s*ft|sqft|sf|square feet)\s+of\s+kitchen[^\d]{0,40}?(\d+(?:\.\d+)?)\s*(?:sq\.?\s*ft|sqft|sf|square feet)\s+of\s+vanity[^\d]{0,40}?(\d+(?:\.\d+)?)(?:\s+total)?",
        "i"
    )
    .captures(text)
    {
        let values: Vec<Option<f64>> = (1..=3)
            .map(|group| caps.get(group).and_then(|m| js_parse_number(m.as_str())))
            .collect();
        if let [Some(kitchen), Some(vanity), Some(total)] = values[..] {
            let whole = caps.get(0).expect("match");
            let source = source_from_measurement_context(text, whole.start());
            let source_text = squash(whole.as_str());
            for (value, kind) in [
                (kitchen, MeasurementComponent::Countertop),
                (vanity, MeasurementComponent::Vanity),
                (total, MeasurementComponent::TotalProject),
            ] {
                measurements.push(ExtractedMeasurement {
                    value,
                    measurement_type: kind,
                    source,
                    source_text: source_text.clone(),
                    confidence: EvidenceConfidence::High,
                });
            }
        }
    }
    for caps in re!(
        r"(\d+(?:\.\d+)?)\s*(?:sq\.?\s*ft|sqft|sf|square feet)\b(?:\s+of\s+([a-z][a-z\s]{0,24}))?",
        "gi"
    )
    .captures_iter(text)
    {
        let Some(value) = caps
            .get(1)
            .and_then(|m| js_parse_number(m.as_str()))
            .filter(|v| *v > 0.0)
        else {
            continue;
        };
        let whole = caps.get(0).expect("match");
        let whole_len = js_len(whole.as_str());
        let local = js_window(text, whole.start(), 0, whole_len + 28);
        let around = js_window(text, whole.start(), 24, whole_len + 28);
        if looks_like_future_volume_context(&around) {
            continue;
        }
        let labeled = caps.get(2).map_or("", |m| m.as_str());
        let phrase = re!(r"\s+", "g")
            .replace_all(&format!("{labeled} {local}"), " ")
            .into_owned();
        let kind = component_from_local_phrase(&phrase);
        if measurements
            .iter()
            .any(|item| item.measurement_type == kind && (item.value - value).abs() < 0.01)
        {
            continue;
        }
        measurements.push(ExtractedMeasurement {
            value,
            measurement_type: kind,
            source: source_from_measurement_context(text, whole.start()),
            source_text: squash(&around),
            confidence: if kind == MeasurementComponent::Unknown {
                EvidenceConfidence::Low
            } else {
                EvidenceConfidence::High
            },
        });
    }
    for caps in re!(r"\b(\d+(?:\.\d+)?)\s+total\b", "gi").captures_iter(text) {
        let Some(value) = caps
            .get(1)
            .and_then(|m| js_parse_number(m.as_str()))
            .filter(|v| *v > 0.0)
        else {
            continue;
        };
        let whole = caps.get(0).expect("match");
        let around = js_window(text, whole.start(), 24, js_len(whole.as_str()) + 12);
        if looks_like_future_volume_context(&around) {
            continue;
        }
        if measurements
            .iter()
            .any(|item| item.measurement_type == MeasurementComponent::TotalProject)
        {
            continue;
        }
        measurements.push(ExtractedMeasurement {
            value,
            measurement_type: MeasurementComponent::TotalProject,
            source: source_from_measurement_context(text, whole.start()),
            source_text: squash(&around),
            confidence: EvidenceConfidence::High,
        });
    }
    (
        project_size_raw,
        reconcile_component_measurements(measurements),
    )
}

fn reconcile_component_measurements(
    measurements: Vec<ExtractedMeasurement>,
) -> Vec<ExtractedMeasurement> {
    let first = |kind: MeasurementComponent| {
        measurements
            .iter()
            .find(|item| item.measurement_type == kind)
            .map(|item| item.value)
    };
    let (Some(kitchen), Some(vanity), Some(total)) = (
        first(MeasurementComponent::Countertop),
        first(MeasurementComponent::Vanity),
        first(MeasurementComponent::TotalProject),
    ) else {
        return measurements;
    };
    if (kitchen + vanity - total).abs() <= 1.0 {
        return measurements;
    }
    measurements
        .into_iter()
        .map(|mut item| {
            if matches!(
                item.measurement_type,
                MeasurementComponent::Vanity | MeasurementComponent::Countertop
            ) {
                item.confidence = EvidenceConfidence::Low;
            }
            item
        })
        .collect()
}

fn parse_linear_inches(raw: &str) -> Option<(f64, EvidenceConfidence)> {
    let text = squash(raw);
    if text.is_empty() {
        return None;
    }
    if let Some(caps) = re!(
        r#"^(\d+(?:\.\d+)?)\s*(?:'|ft|feet)\s*(?:and\s+)?(\d+(?:\.\d+)?)\s*(?:"|''|in|inch|inches)\.?$"#,
        "i"
    )
    .captures(&text)
        && let (Some(feet), Some(extra)) = (
            caps.get(1).and_then(|m| js_parse_number(m.as_str())),
            caps.get(2).and_then(|m| js_parse_number(m.as_str())),
        )
    {
        return Some((feet * 12.0 + extra, EvidenceConfidence::High));
    }
    if let Some(feet) = re!(r"^(\d+(?:\.\d+)?)\s*(?:'|ft|feet)\.?$", "i")
        .captures(&text)
        .and_then(|caps| caps.get(1))
        .and_then(|m| js_parse_number(m.as_str()))
    {
        return Some((feet * 12.0, EvidenceConfidence::High));
    }
    re!(r#"^(\d+(?:\.\d+)?)\s*(?:"|''|in|inch|inches)\.?$"#, "i")
        .captures(&text)
        .and_then(|caps| caps.get(1))
        .and_then(|m| js_parse_number(m.as_str()))
        .map(|inches| (inches, EvidenceConfidence::Medium))
}

fn split_dimension_pair(text: &str) -> Option<(String, String, String)> {
    if let Some(caps) = re!(
        r#"(\d+(?:\.\d+)?)\s*(?:'|ft|feet)\s*(?:x|by|×)\s*(\d+(?:\.\d+)?)\s*(?:'|ft|feet)\s+and\s+(\d+(?:\.\d+)?)\s*(?:"|''|in|inch|inches)\b"#,
        "i"
    )
    .captures(text)
        && let (Some(a), Some(b), Some(c)) = (caps.get(1), caps.get(2), caps.get(3))
    {
        return Some((
            format!("{}ft", a.as_str()),
            format!("{}ft and {} inches", b.as_str(), c.as_str()),
            caps[0].to_string(),
        ));
    }
    if let Some(caps) = re!(
        r#"(\d+(?:\.\d+)?)\s*(?:'|ft|feet)\s*(?:x|by|×)\s*(\d+(?:\.\d+)?)\s*(?:"|''|in|inch|inches)"#,
        "i"
    )
    .captures(text)
        && let (Some(a), Some(b)) = (caps.get(1), caps.get(2))
    {
        return Some((format!("{}ft", a.as_str()), format!("{} inches", b.as_str()), caps[0].to_string()));
    }
    let caps = re!(
        r#"(\d+(?:\.\d+)?\s*(?:'|ft|feet)(?:\s*(?:and\s+)?\d+(?:\.\d+)?\s*(?:"|''|in|inch|inches)?)?)\s*(?:x|by|×)\s*(\d+(?:\.\d+)?\s*(?:'|ft|feet)(?:\s*(?:and\s+)?\d+(?:\.\d+)?\s*(?:"|''|in|inch|inches)?)?)"#,
        "i"
    )
    .captures(text)?;
    Some((
        caps.get(1)?.as_str().to_string(),
        caps.get(2)?.as_str().to_string(),
        caps[0].to_string(),
    ))
}

/// Dimensions given against an area reported for them.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MeasurementConflict {
    pub measurement_conflict: bool,
    pub measurement_dimensions_length_in: Option<f64>,
    pub measurement_dimensions_width_in: Option<f64>,
    pub calculated_area_from_dimensions_sqft: Option<f64>,
    pub customer_reported_area_sqft: Option<f64>,
    pub customer_reported_island_sqft: Option<f64>,
    pub measurement_raw_text: Option<String>,
}

fn reported_area(text: &str) -> Option<f64> {
    re!(r"(?:~|≈|about|=)\s*(\d+(?:\.\d+)?)\s*sq\.?\s*ft", "i")
        .captures(text)
        .and_then(|caps| caps.get(1))
        .and_then(|m| js_parse_number(m.as_str()))
}

/// Dimensions in the text, their area, and whether a reported area disagrees.
pub fn extract_measurement_conflict(text: &str) -> MeasurementConflict {
    let island = re!(r"\bisland\b", "i").is_match(text);
    if let Some((left, right, raw)) = split_dimension_pair(text)
        && let (Some((length, length_confidence)), Some((width, width_confidence))) =
            (parse_linear_inches(&left), parse_linear_inches(&right))
        && length > 0.0
        && width > 0.0
    {
        let calculated = js_round(length * width / 144.0 * 100.0) / 100.0;
        let reported = reported_area(text);
        let uncertain = length_confidence == EvidenceConfidence::Low
            || width_confidence == EvidenceConfidence::Low
            || re!(r"\bthick(?:ness)?\b|\bdepth\b", "i").is_match(&raw);
        let raw_text = Some(squash(&raw));
        let Some(reported) = reported else {
            return MeasurementConflict {
                measurement_conflict: uncertain,
                measurement_dimensions_length_in: Some(length),
                measurement_dimensions_width_in: Some(width),
                calculated_area_from_dimensions_sqft: (!uncertain).then_some(calculated),
                customer_reported_island_sqft: (island && !uncertain).then_some(calculated),
                measurement_raw_text: raw_text,
                ..MeasurementConflict::default()
            };
        };
        let conflict = (reported - calculated).abs() > 2.0;
        return MeasurementConflict {
            measurement_conflict: conflict || uncertain,
            measurement_dimensions_length_in: Some(length),
            measurement_dimensions_width_in: Some(width),
            calculated_area_from_dimensions_sqft: (!uncertain).then_some(calculated),
            customer_reported_area_sqft: Some(reported),
            customer_reported_island_sqft: island.then_some(reported),
            measurement_raw_text: raw_text,
        };
    }
    let dim = re!(
        r#"(\d+(?:\.\d+)?)\s*(?:in|inch|inches|["”])?\s*(?:l|long|length)?\s*[×xX*]\s*(\d+(?:\.\d+)?)\s*(?:w|wide|width|in|inch|inches|["”])?(?:\s*[×xX*]\s*(\d+(?:\.\d+)?)\s*(?:in|inch|inches|["”])?)?"#,
        "i"
    )
    .captures(text);
    let length_only = re!(
        r#"(\d+(?:\.\d+)?)\s*(?:in|inch|inches|["”])\s*(?:l|long|length)\b(?:.{0,40}instead of (\d+(?:\.\d+)?))?"#,
        "i"
    )
    .captures(text);
    let reported = reported_area(text);
    let length = dim
        .as_ref()
        .and_then(|caps| caps.get(1))
        .or_else(|| length_only.as_ref().and_then(|caps| caps.get(1)))
        .and_then(|m| js_parse_number(m.as_str()));
    let width = dim
        .as_ref()
        .and_then(|caps| caps.get(2))
        .and_then(|m| js_parse_number(m.as_str()));
    let (Some(length), Some(width)) = (length, width) else {
        return MeasurementConflict::default();
    };
    if re!(r"\bft\b|\bfeet\b|'", "i").is_match(text) && length < 40.0 && width < 40.0 {
        return MeasurementConflict::default();
    }
    let calculated = js_round(length * width / 144.0);
    let Some(reported) = reported else {
        return MeasurementConflict {
            measurement_dimensions_length_in: Some(length),
            measurement_dimensions_width_in: Some(width),
            calculated_area_from_dimensions_sqft: Some(calculated),
            ..MeasurementConflict::default()
        };
    };
    MeasurementConflict {
        measurement_conflict: (reported - calculated).abs() > 2.0,
        measurement_dimensions_length_in: Some(length),
        measurement_dimensions_width_in: Some(width),
        calculated_area_from_dimensions_sqft: Some(calculated),
        customer_reported_area_sqft: Some(reported),
        customer_reported_island_sqft: island.then_some(reported),
        measurement_raw_text: None,
    }
}

/// Edge profiles named (bullnose, ogee...).
pub fn extract_edge_preferences(text: &str) -> Vec<String> {
    let mut found: Vec<String> = Vec::new();
    for caps in re!(
        r"\b(bullnose|ogee|bevel(?:ed)?|eased|pencil edge|square edge|mitered)\b",
        "gi"
    )
    .captures_iter(text)
    {
        let Some(edge) = caps.get(1).map(|m| m.as_str()) else {
            continue;
        };
        let normalized = if edge.to_lowercase() == "beveled" {
            "Beveled".to_string()
        } else {
            title_case_material(edge)
        };
        if !found
            .iter()
            .any(|item| item.to_lowercase() == normalized.to_lowercase())
        {
            found.push(normalized);
        }
    }
    found
}

/// Stones picked in the conversation.
pub fn extract_conversation_material_selections(text: &str) -> Vec<String> {
    let mut found: Vec<String> = Vec::new();
    for caps in re!(
        r"\b(viscon white|atlas(?: white)?|cristallo|turquoise|viraldi|idillio|vincent black(?:\s+honed)?|black pearl(?:\s+leathered)?|calacatta(?:\s+(?:laza|oro|gold|viola|borghini))+|devinity|mirag(?:io|g)(?: cove)?)\b",
        "gi"
    )
    .captures_iter(text)
    {
        let name = title_case_material(caps.get(1).map_or("", |m| m.as_str()));
        if !name.is_empty() && !found.iter().any(|item| item.to_lowercase() == name.to_lowercase()) {
            found.push(name);
        }
    }
    found
}

/// The first measurement of a part of the project.
pub fn measurement_of_type(
    measurements: &[ExtractedMeasurement],
    kind: MeasurementComponent,
) -> Option<ExtractedMeasurement> {
    measurements
        .iter()
        .find(|item| item.measurement_type == kind)
        .cloned()
}

/// Work still to be done before the countertops.
pub fn extract_project_dependencies(text: &str) -> Vec<ProjectDependency> {
    let mut found: Vec<ProjectDependency> = Vec::new();
    let mut add = |kind: ProjectDependencyType, evidence: &str| {
        if !found.iter().any(|item| item.kind == kind) {
            found.push(ProjectDependency {
                kind,
                status: DependencyStatus::Incomplete,
                evidence: evidence.to_string(),
            });
        }
    };
    if re!(
        r"cabinets? (are |is )?(almost ready.{0,48})?(unlevel|not level|bot leveled|unleveled)|cabinets are almost ready, but bot leveled",
        "i"
    )
    .is_match(text)
    {
        add(ProjectDependencyType::CabinetLeveling, "cabinets are almost ready but not leveled");
    }
    if re!(
        r"waiting for (the )?floors|flooring (is )?on order|floors to arrive|until (the )?floors are in place|no decisions will be made until (the )?floors",
        "i"
    )
    .is_match(text)
    {
        add(ProjectDependencyType::FloorInstallation, "flooring must be installed before countertop decisions");
    }
    if re!(r"waiting for (the )?cabinets|cabinets not installed", "i").is_match(text) {
        add(
            ProjectDependencyType::CabinetInstall,
            "waiting for cabinets to be installed",
        );
    }
    if re!(r"waiting (on|for) plumbing|plumbing not (ready|done)", "i").is_match(text) {
        add(ProjectDependencyType::Plumbing, "waiting on plumbing");
    }
    if re!(r"waiting (on|for) drywall|drywall not (ready|done)", "i").is_match(text) {
        add(ProjectDependencyType::Drywall, "waiting on drywall");
    }
    found
}

/// Cabinets mentioned as a prerequisite, not as what is being bought.
pub(crate) fn is_cabinet_prerequisite_language(text: &str) -> bool {
    re!(
        r"cabinets? (are |is )?(almost ready|unlevel|not level|bot leveled|unleveled|need(s)? (to be )?level)|almost ready for counter|countertops? waiting on cabinets|waiting (on|for) (the )?cabinets|already have (the )?cabinets (installed|in)|cabinets already installed|already installed.{0,40}cabinets",
        "i"
    )
    .is_match(text)
}

/// The lines of the profile and corpus that carry the customer's own request.
pub(crate) fn customer_request_evidence_text(profile: &str, customer_corpus: &str) -> String {
    format!("{profile}\n{customer_corpus}")
        .split('\n')
        .filter(|line| {
            if re!(r"^Activity:", "i").is_match(line) && !re!(r"asked to ", "i").is_match(line) {
                return false;
            }
            if re!(r"^EMAIL (Company|Customer):", "i").is_match(line) {
                return false;
            }
            if re!(r"save \d+%|browse our stone|promo|this august", "i").is_match(line) {
                return false;
            }
            if re!(
                r"^Deal note|^Customer message:|^Lead details:|^Project type:|^Name:",
                "i"
            )
            .is_match(line)
            {
                return true;
            }
            if re!(
                r"^(Asked to call|Have a quote|They want|Still in the planning)",
                "i"
            )
            .is_match(trim(line))
            {
                return true;
            }
            re!(
                r"asked to call|preferred method of contact|text or email",
                "i"
            )
            .is_match(line)
        })
        .collect::<Vec<_>>()
        .join("\n")
}
