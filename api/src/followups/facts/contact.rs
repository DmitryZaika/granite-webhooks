//! How the customer wants to be contacted, where they stand, and the open
//! questions about budget, sink and backsplash (part of
//! `smsFlowAiFollowupFacts.ts`).

use super::super::dates::{Instant, utc_date};
use super::super::js::trim;
use super::quotes::{
    appointment_companion_re, budget_constraint_re, budget_known_re, budget_obstacle_re,
    comparing_quotes_re, is_short_non_superseding_reply, price_factor_re, price_rejection_re,
    quality_factor_re, specials_next_step_re, spouse_decision_re, terminal_price_rejection_re,
    text_looks_like_competitor_quote_reference, text_looks_like_quote_request,
};
use super::scope::unique_evidence_chunks;
use super::types::{
    AppointmentCompanion, BacksplashFinish, BacksplashType, ContactMethod, DecisionCriterion,
    DecisionFactor, EvidenceConfidence, EvidenceSource, ExplicitCustomerRequestKind,
    LatestCustomerState, PreferredNextStep, PreferredNextStepSource, SinkSource,
    UnfinishedNextStepKind,
};
use crate::re;

fn push_unique<T: PartialEq>(items: &mut Vec<T>, item: T) {
    if !items.contains(&item) {
        items.push(item);
    }
}

/// Every channel the customer asked for ("text or email is best").
pub fn extract_requested_contact_methods(text: &str) -> Vec<ContactMethod> {
    let mut methods = Vec::new();
    if re!(
        r"text or email|sms or email|email or text|preferred method of contact is text or email|text or email is best",
        "i"
    )
    .is_match(text)
    {
        return vec![ContactMethod::Sms, ContactMethod::Email];
    }
    if re!(r"don't call.{0,40}(just )?text|text me (instead|next)|please text|asked to text|prefer.{0,20}text", "i")
        .is_match(text)
    {
        push_unique(&mut methods, ContactMethod::Sms);
    }
    if re!(
        r"please email me instead|email me instead|asked to email|please email me\b|email me\b|prefer.{0,20}email",
        "i"
    )
    .is_match(text)
    {
        push_unique(&mut methods, ContactMethod::Email);
    }
    if re!(
        r"asked to call|call me\b|please call|phone call|give .{0,16}a call",
        "i"
    )
    .is_match(text)
    {
        push_unique(&mut methods, ContactMethod::Phone);
    }
    methods
}

/// The one channel to use, and the next step it implies.
pub fn extract_requested_contact_method(text: &str) -> (Option<ContactMethod>, PreferredNextStep) {
    let methods = extract_requested_contact_methods(text);
    if methods.contains(&ContactMethod::Sms) {
        return (Some(ContactMethod::Sms), PreferredNextStep::None);
    }
    if methods.contains(&ContactMethod::Email) {
        return (Some(ContactMethod::Email), PreferredNextStep::None);
    }
    if methods.contains(&ContactMethod::Phone) {
        return (Some(ContactMethod::Phone), PreferredNextStep::PhoneCall);
    }
    if specials_next_step_re().is_match(text) {
        return (None, PreferredNextStep::SendCurrentSpecials);
    }
    (None, PreferredNextStep::None)
}

/// "Send you our current specials": who agreed to it.
pub fn extract_agreed_next_step(
    customer_corpus: &str,
    profile: &str,
    later_outbound: &str,
) -> (
    PreferredNextStep,
    PreferredNextStepSource,
    EvidenceConfidence,
) {
    let combined = format!("{customer_corpus}\n{profile}\n{later_outbound}");
    if !specials_next_step_re().is_match(&combined) {
        return (
            PreferredNextStep::None,
            PreferredNextStepSource::None,
            EvidenceConfidence::Low,
        );
    }
    let customer_agreed = re!(
        r"yes that would be (awesome|great)|that would be awesome|yes please|please do",
        "i"
    )
    .is_match(customer_corpus);
    if customer_agreed || specials_next_step_re().is_match(customer_corpus) {
        return (
            PreferredNextStep::SendCurrentSpecials,
            PreferredNextStepSource::CustomerSms,
            EvidenceConfidence::High,
        );
    }
    (
        PreferredNextStep::SendCurrentSpecials,
        PreferredNextStepSource::SalespersonNote,
        EvidenceConfidence::Medium,
    )
}

/// The channel the salesperson's own CRM task names.
pub fn extract_salesperson_planned_contact_method(profile: &str) -> Option<ContactMethod> {
    for line in profile.split('\n') {
        if !line.starts_with("Activity:") || re!(r"asked to ", "i").is_match(line) {
            continue;
        }
        if re!(r"\b(call|phone)\b", "i").is_match(line) {
            return Some(ContactMethod::Phone);
        }
        if re!(r"\bemail\b", "i").is_match(line) {
            return Some(ContactMethod::Email);
        }
        if re!(r"\b(text|sms)\b", "i").is_match(line) {
            return Some(ContactMethod::Sms);
        }
    }
    None
}

/// A spouse coming to the appointment (not deciding for the customer).
pub fn extract_appointment_companion(text: &str) -> Option<AppointmentCompanion> {
    if !appointment_companion_re().is_match(text) || spouse_decision_re().is_match(text) {
        return None;
    }
    re!(r"come with|bring|attend|appointment|want to come", "i")
        .is_match(text)
        .then_some(AppointmentCompanion::Spouse)
}

/// A CRM activity says the in-home estimate is done.
pub fn profile_has_completed_estimate(profile: &str) -> bool {
    profile.split('\n').any(|line| {
        line.starts_with("Activity:")
            && re!(r"\bcompleted\b").is_match(line)
            && re!(
                r"in-?home estimate|on-?site estimate|be on-site for estimate|onsite estimate",
                "i"
            )
            .is_match(line)
    })
}

/// What the decision depends on (budget, competing quotes, a spouse...).
pub fn extract_decision_factors(text: &str) -> Vec<DecisionFactor> {
    let mut factors = Vec::new();
    if budget_obstacle_re().is_match(text)
        || budget_constraint_re().is_match(text)
        || re!(
            r"\bbudget around\b|\bbudget of \$|\baround \$\s*\d{4,}",
            "i"
        )
        .is_match(text)
    {
        push_unique(&mut factors, DecisionFactor::Budget);
    }
    if comparing_quotes_re().is_match(text)
        || re!(r"highest of all my bids|compare prices", "i").is_match(text)
        || re!(r"\bprice and quality\b", "i").is_match(text)
    {
        push_unique(&mut factors, DecisionFactor::PriceComparison);
        push_unique(&mut factors, DecisionFactor::CompetitiveComparison);
        push_unique(&mut factors, DecisionFactor::Price);
    }
    if price_factor_re().is_match(text)
        || re!(r"price list|pricing by level|exact price|\/\s*sqft", "i").is_match(text)
    {
        push_unique(&mut factors, DecisionFactor::Price);
    }
    if quality_factor_re().is_match(text) {
        push_unique(&mut factors, DecisionFactor::Quality);
    }
    if spouse_decision_re().is_match(text) {
        push_unique(&mut factors, DecisionFactor::SpouseOrPartnerDecision);
    }
    if re!(r"timing|not yet|later|next (month|year)|after ", "i").is_match(text)
        && re!(r"wait|not yet|later", "i").is_match(text)
    {
        push_unique(&mut factors, DecisionFactor::Timing);
    }
    if re!(r"backsplash", "i").is_match(text)
        && re!(r"\bprice\b", "i").is_match(text)
        && re!(r"depend|might|not sure|unsure|thinking about", "i").is_match(text)
    {
        push_unique(&mut factors, DecisionFactor::BacksplashCost);
    }
    factors
}

/// Where the customer's latest messages leave the deal.
#[allow(clippy::too_many_lines)]
pub fn infer_latest_customer_state(latest_customer: &str) -> LatestCustomerState {
    use LatestCustomerState as S;
    let stripped = latest_customer
        .split('\n')
        .map(trim)
        .filter(|line| {
            !line.is_empty()
                && !re!(r"^(thanks|thank you|ok|okay|got it)[.!]*\s*$", "i").is_match(line)
                && !re!(r"^[\u{1F300}-\u{1FAFF}\u{2600}-\u{27BF}\s.!]{1,8}$", "u").is_match(line)
        })
        .collect::<Vec<_>>()
        .join("\n");
    let trimmed = if trim(&stripped).is_empty() {
        trim(latest_customer)
    } else {
        trim(&stripped)
    };
    if trimmed.is_empty() {
        return S::Unknown;
    }
    let t = trimmed;
    if re!(r"template approval required", "i").is_match(t) && re!(r"\bapproved\b", "i").is_match(t)
    {
        return S::TemplateApproved;
    }
    if re!(
        r"template approved|approved (the )?template|customer approved (the )?template",
        "i"
    )
    .is_match(t)
    {
        return S::TemplateApproved;
    }
    if re!(
        r"\bwe are not ready yet\b|\bnot ready yet\b|\bnot ready for (any )?renovations\b",
        "i"
    )
    .is_match(t)
    {
        return S::Waiting;
    }
    if re!(
        r"^(yes|yeah|yep)[,.]?\s*(sorry[, ]*)?(we |i )?have[.!]?\s*$",
        "i"
    )
    .is_match(t)
        || (re!(
            r"not interested|please stop|do not contact|unsubscribe|no longer interested",
            "i"
        )
        .is_match(t)
            && !re!(r"still considering", "i").is_match(t))
    {
        return S::Declined;
    }
    if re!(
        r"decided to put off|decided tu put off|put off the project|postponed|pushed back|on delay",
        "i"
    )
    .is_match(t)
    {
        return S::Delayed;
    }
    if re!(
        r"building a home|building our home|future home|not finished|choosing vendors ahead|pick out vendors ahead|more than 4 weeks",
        "i"
    )
    .is_match(t)
    {
        return S::FutureProject;
    }
    if re!(
        r"signed (the |our |your )?contract|we (just )?received (your |the )?(check|payment|deposit)|received your check|scheduling manager should call",
        "i"
    )
    .is_match(t)
    {
        return S::Active;
    }
    if re!(
        r"still in the planning process|in the planning process|still planning",
        "i"
    )
    .is_match(t)
    {
        return S::Planning;
    }
    if re!(
        r"not doing the project now|we are not doing the project",
        "i"
    )
    .is_match(t)
    {
        return S::PausedIndefinite;
    }
    if re!(
        r"almost ready for counter|cabinets? (are |is )?(unlevel|not level|bot leveled|unleveled)|waiting for (the )?floors|until (the )?floors",
        "i"
    )
    .is_match(t)
    {
        return S::WaitingOnDependency;
    }
    if re!(
        r"just hold for now|will contact you when|contact you whenever|not ready for any renovations|we are not ready yet",
        "i"
    )
    .is_match(t)
    {
        return S::Waiting;
    }
    if re!(
        r"decided to hold off|holding off|hold off for now|putting it on hold|put the project on hold",
        "i"
    )
    .is_match(t)
    {
        return S::Waiting;
    }
    if terminal_price_rejection_re().is_match(t) && price_rejection_re().is_match(t) {
        return S::Declined;
    }
    if price_rejection_re().is_match(t) && comparing_quotes_re().is_match(t) {
        return S::PriceObjection;
    }
    if re!(
        r"hoping to start soon|should be able to get started|days would be best|about to have a baby|start soon",
        "i"
    )
    .is_match(t)
    {
        return S::NearTermProject;
    }
    if re!(r"\bbriefly looked at (?:it|the quote)\b", "i").is_match(t)
        || re!(
            r"(?:have not|haven't) .{0,48}(?:sit down to )?(?:fully )?review (?:the )?quote",
            "i"
        )
        .is_match(t)
        || re!(r"still thinking about it", "i").is_match(t)
        || re!(
            r"(?:we're|i'm|im) (?:still )?comparing (?:the )?options",
            "i"
        )
        .is_match(t)
    {
        return S::Evaluating;
    }
    if re!(r"(?:have not|haven't) reviewed (?:it|the quote) yet", "i").is_match(t) {
        return S::Waiting;
    }
    if is_short_non_superseding_reply(t) || re!(r"\bnot yet\b", "i").is_match(t) {
        return S::Waiting;
    }
    if comparing_quotes_re().is_match(t) || re!(r"i('ll| will) review", "i").is_match(t) {
        return S::Evaluating;
    }
    if budget_obstacle_re().is_match(t) {
        if re!(
            r"\bbe patient\b|\btry(?:ing)? to get (?:the )?money\b|\bnot right now\b",
            "i"
        )
        .is_match(t)
        {
            return S::Waiting;
        }
        return S::Evaluating;
    }
    S::Unknown
}

/// The facts [`collect_unresolved_facts`] reads.
#[allow(clippy::struct_excessive_bools)]
pub struct UnresolvedFactInputs<'a> {
    pub latest_customer: &'a str,
    pub all_customer_text: &'a str,
    pub unfinished_next_step_kind: Option<UnfinishedNextStepKind>,
    pub unfinished_next_step: Option<&'a str>,
    pub budget_obstacle_evidence: Option<&'a str>,
    pub budget_target_approx: Option<f64>,
    pub comparing_quotes: bool,
    pub active_decision_factors: &'a [DecisionFactor],
    pub decision_obstacle_evidence: Option<&'a str>,
    pub did_not_find_suitable_option: bool,
    pub countertop_color_preferences: &'a [String],
    pub material_type_preferences: &'a [String],
}

/// What is still open with the customer, in plain words for the prompt.
pub fn collect_unresolved_facts(params: &UnresolvedFactInputs<'_>) -> Vec<String> {
    let mut facts: Vec<String> = Vec::new();
    let all = params.all_customer_text;
    if let Some(target) = params.budget_target_approx {
        facts.push(format!(
            "budget target around ${}",
            super::super::js::js_number(target)
        ));
    } else if let Some(evidence) = params.budget_obstacle_evidence.filter(|e| !e.is_empty()) {
        facts.push(evidence.to_string());
    }
    if params.did_not_find_suitable_option
        && params
            .countertop_color_preferences
            .iter()
            .any(|item| re!(r"white", "i").is_match(item))
        && params
            .material_type_preferences
            .iter()
            .any(|item| re!(r"granite", "i").is_match(item))
    {
        facts.push("customer wants white granite".into());
        facts.push("customer did not find a suitable option".into());
    } else if params.did_not_find_suitable_option {
        facts.push("customer did not find a suitable option".into());
    }
    if params.comparing_quotes {
        facts.push("comparing competitor quotes".into());
    }
    if re!(
        r"does not have measurements|will provide approximate square footage|approximate sqft or a drawing|still needs approximate sqft|measurements? (are )?not available yet|will provide (?:approximate )?(?:square footage|a drawing)",
        "i"
    )
    .is_match(all)
    {
        facts.push("approximate square footage or drawing still needed".into());
    }
    if re!(
        r"countertop measurements? (are )?not available|countertop measur(e)?ments not available",
        "i"
    )
    .is_match(all)
    {
        facts.push("countertop measurements needed".into());
    }
    if re!(
        r"cabinet.{0,40}(wall width|layout).{0,40}(needed|not available)|cabinet wall.{0,20}measurements? (are )?still needed|cabinet.{0,20}layout measurements? (are )?still needed",
        "i"
    )
    .is_match(all)
    {
        facts.push("cabinet wall/layout measurements needed".into());
    }
    if re!(
        r"measurements? (are )?not available|not available yet|layout still needed|wall widths|measurements not finalized",
        "i"
    )
    .is_match(all)
    {
        if re!(r"countertop|kitchen", "i").is_match(all)
            && !facts.iter().any(|item| re!(r"countertop measurements", "i").is_match(item))
        {
            facts.push("countertop measurements not finalized".into());
        }
        if re!(r"cabinet|wall width|layout", "i").is_match(all)
            && !facts.iter().any(|item| re!(r"cabinet wall", "i").is_match(item))
        {
            facts.push("cabinet wall measurements/layout needed".into());
        }
    }
    if params
        .active_decision_factors
        .contains(&DecisionFactor::Quality)
    {
        facts.push("price and quality are decision criteria".into());
    }
    if params.unfinished_next_step_kind == Some(UnfinishedNextStepKind::CustomerMeasurements)
        || re!(r"should send .{0,30}measur|measurments|measurements", "i")
            .is_match(params.unfinished_next_step.unwrap_or(""))
    {
        facts.push("customer/project measurements still needed".into());
    }
    if re!(r"how many square feet|how much (?:square )?footage|sq\.?\s*ft|sqft", "i").is_match(all)
        && re!(r"\bcost\b|\bprice\b|\bpricing\b|\bestimate\b", "i").is_match(all)
        && !re!(r"\d+\s*(?:sq\.?\s*ft|sqft)\b", "i").is_match(all)
        && !re!(r"length.{0,24}depth|width.{0,24}(?:depth|length)|\d+\s*(?:ft|foot|feet|in|inch|inches|'|-)", "i").is_match(all)
        && !facts.iter().any(|item| re!(r"dimension|measurement|square footage", "i").is_match(item))
    {
        facts.push("dimensions needed to determine remaining square footage".into());
        facts.push("project measurements needed for pricing".into());
    }
    if let Some(obstacle) = params.decision_obstacle_evidence.filter(|e| !e.is_empty()) {
        let needle = obstacle.to_lowercase();
        if !facts
            .iter()
            .any(|item| item.to_lowercase().contains(&needle))
        {
            facts.push(obstacle.to_string());
        }
    }
    facts
}

fn estimate_request(chunk: &str) -> bool {
    re!(r"\b(get|give me|need|want|like to).{0,24}\bestimate\b", "i").is_match(chunk)
}

fn has_stated_sqft(chunk: &str) -> bool {
    re!(r"\d+\s*(?:sq\.?\s*ft|sqft)", "i").is_match(chunk)
        && !re!(
            r"how many|can you|give me|what('s| is) the|need .{0,20}square",
            "i"
        )
        .is_match(chunk)
}

fn mentions_sqft(chunk: &str) -> bool {
    re!(r"square feet|sq\.?\s*ft|sqft|how many square", "i").is_match(chunk)
}

fn mentions_cost(chunk: &str) -> bool {
    re!(r"\bcost\b|\bprice\b|\bpricing\b|\bestimate\b", "i").is_match(chunk)
}

/// The customer's own ask ("can you give me a quote", "price per sqft").
pub fn extract_explicit_customer_request(
    text: &str,
) -> (Option<String>, Option<ExplicitCustomerRequestKind>) {
    use ExplicitCustomerRequestKind as K;
    let chunks = unique_evidence_chunks(&[text]);
    let found = |chunk: &String, kind| (Some(chunk.clone()), Some(kind));
    for chunk in &chunks {
        if text_looks_like_competitor_quote_reference(chunk) {
            continue;
        }
        if estimate_request(chunk) && !re!(r"\bquote\b", "i").is_match(chunk) {
            return found(chunk, K::EstimateRequest);
        }
    }
    for chunk in &chunks {
        if has_stated_sqft(chunk) {
            continue;
        }
        if mentions_sqft(chunk)
            && mentions_cost(chunk)
            && !re!(r"price per (?:sq\.?\s*ft|sqft|square foot)", "i").is_match(chunk)
        {
            return found(chunk, K::EstimatePricing);
        }
    }
    for chunk in &chunks {
        if text_looks_like_quote_request(chunk) {
            return found(chunk, K::QuoteRequest);
        }
    }
    for chunk in &chunks {
        if re!(r"price per (sq\.?\s*ft|sqft|square foot)", "i").is_match(chunk) {
            return found(chunk, K::PricePerSqft);
        }
    }
    for chunk in &chunks {
        if re!(r"looking for a fabricated|fabricated .{0,40}countertop|fabricated with a single faucet hole", "i")
            .is_match(chunk)
        {
            return found(chunk, K::FabricationRequest);
        }
        if re!(r"\b(remnant|bianco absoluto|frost white)\b", "i").is_match(chunk)
            && re!(r"looking for|need|want", "i").is_match(chunk)
        {
            return found(chunk, K::ProductAvailability);
        }
    }
    for chunk in &chunks {
        if text_looks_like_competitor_quote_reference(chunk) {
            continue;
        }
        if estimate_request(chunk) {
            return found(chunk, K::EstimateRequest);
        }
    }
    for chunk in &chunks {
        if text_looks_like_competitor_quote_reference(chunk) || has_stated_sqft(chunk) {
            continue;
        }
        let sqft = mentions_sqft(chunk);
        if sqft && mentions_cost(chunk) {
            return found(chunk, K::EstimatePricing);
        }
        if sqft {
            return found(chunk, K::SquareFootage);
        }
        if re!(r"business").is_match(chunk) && text_looks_like_quote_request(chunk) {
            return found(chunk, K::OnsiteQuote);
        }
        if text_looks_like_quote_request(chunk) {
            return found(chunk, K::QuoteRequest);
        }
    }
    (None, None)
}

/// What the customer said about the backsplash.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BacksplashState {
    pub known: bool,
    #[serde(rename = "type")]
    pub kind: BacksplashType,
    pub stone_backsplash_required: Option<bool>,
    pub backsplash_finish: BacksplashFinish,
    pub backsplash_material: Option<String>,
    pub backsplash_description: Option<String>,
}

fn stone_backsplash(kind: BacksplashType, description: &str) -> BacksplashState {
    BacksplashState {
        known: true,
        kind,
        stone_backsplash_required: Some(true),
        backsplash_finish: BacksplashFinish::Stone,
        backsplash_material: None,
        backsplash_description: Some(description.into()),
    }
}

/// Tile, stone (full or 4"), none, or undecided.
pub fn extract_backsplash_state(text: &str) -> BacksplashState {
    let tile_description = re!(
        r"backsplash will be ([^.]+)|((?:white )?subway tile(?:[^.]{0,80})?)",
        "i"
    )
    .captures(text)
    .and_then(|caps| caps.get(1).or_else(|| caps.get(2)))
    .map(|m| m.as_str().to_string());
    if re!(r"full[- ]?height backsplash|full height backsplash", "i").is_match(text) {
        return stone_backsplash(BacksplashType::Full, "full-height backsplash");
    }
    if re!(
        r"subway tile|tile backsplash|backsplash will be .{0,40}tile",
        "i"
    )
    .is_match(text)
    {
        return BacksplashState {
            known: true,
            kind: BacksplashType::Unknown,
            stone_backsplash_required: Some(false),
            backsplash_finish: BacksplashFinish::Tile,
            backsplash_material: Some(
                if re!(r"subway", "i").is_match(text) {
                    "white subway tile"
                } else {
                    "tile"
                }
                .into(),
            ),
            backsplash_description: Some(
                tile_description
                    .filter(|d| !d.is_empty())
                    .map_or_else(|| "tile backsplash".to_string(), |d| trim(&d).to_string()),
            ),
        };
    }
    if re!(r"no backsplash|\bno bs\b", "i").is_match(text) {
        return BacksplashState {
            known: true,
            kind: BacksplashType::None,
            stone_backsplash_required: Some(false),
            backsplash_finish: BacksplashFinish::None,
            backsplash_material: None,
            backsplash_description: None,
        };
    }
    if re!(r"full[- ]?height backsplash|full backsplash", "i").is_match(text) {
        return stone_backsplash(BacksplashType::Full, "full stone backsplash");
    }
    if re!(
        r#"4\s*["”]\s*backsplash|4.?(-| )?inch backsplash|4.? backsplash"#,
        "i"
    )
    .is_match(text)
    {
        return stone_backsplash(BacksplashType::FourInch, "4-inch stone backsplash");
    }
    if re!(r"backsplash", "i").is_match(text)
        && re!(
            r"not sure|unsure|undecided|might depend|thinking about it|we are thinking",
            "i"
        )
        .is_match(text)
    {
        return BacksplashState {
            known: true,
            kind: BacksplashType::Unknown,
            stone_backsplash_required: None,
            backsplash_finish: BacksplashFinish::Unknown,
            backsplash_material: Some("UNDECIDED".into()),
            backsplash_description: Some("backsplash decision open".into()),
        };
    }
    BacksplashState {
        known: false,
        kind: BacksplashType::Unknown,
        stone_backsplash_required: None,
        backsplash_finish: BacksplashFinish::Unknown,
        backsplash_material: None,
        backsplash_description: None,
    }
}

/// Whether a sink is needed and who supplies it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SinkState {
    pub sink_required: bool,
    pub sink_source: Option<SinkSource>,
    pub needs_sink_from_company: bool,
}

pub fn extract_sink_state(text: &str) -> SinkState {
    let external = re!(
        r"sink (from|form) someone else|provide .{0,24}own sink|own sink|buy a sink (from|form) someone else|sourcing sink elsewhere|\(customer supplied\)|customer supplied",
        "i"
    )
    .is_match(text);
    let from_company = re!(
        r"buy a sink from (you|us|the company|company)|need(s)? (a )?sink from you|sink from (you|us|the company|company)|buy sink from company|sink:\s*from company",
        "i"
    )
    .is_match(text);
    if external {
        return SinkState {
            sink_required: true,
            sink_source: Some(SinkSource::CustomerExternal),
            needs_sink_from_company: false,
        };
    }
    if from_company {
        return SinkState {
            sink_required: true,
            sink_source: Some(SinkSource::Company),
            needs_sink_from_company: true,
        };
    }
    if re!(r"need(s)? (a )?sink", "i").is_match(text) {
        return SinkState {
            sink_required: true,
            sink_source: Some(SinkSource::Unknown),
            needs_sink_from_company: true,
        };
    }
    SinkState {
        sink_required: false,
        sink_source: None,
        needs_sink_from_company: false,
    }
}

/// What the customer will decide on: price, quality.
pub fn extract_decision_criteria(text: &str) -> Vec<DecisionCriterion> {
    let mut criteria = Vec::new();
    if re!(r"\bprice and quality\b", "i").is_match(text)
        || (re!(r"\bmain criteria\b", "i").is_match(text) && re!(r"\bprice\b", "i").is_match(text))
    {
        push_unique(&mut criteria, DecisionCriterion::Price);
        push_unique(&mut criteria, DecisionCriterion::Quality);
    } else {
        if re!(r"\bquality\b", "i").is_match(text) && !budget_obstacle_re().is_match(text) {
            push_unique(&mut criteria, DecisionCriterion::Quality);
        }
        if (re!(r"\bprice\b", "i").is_match(text)
            || comparing_quotes_re().is_match(text)
            || re!(
                r"price list|pricing by level|exact price|what can i get at",
                "i"
            )
            .is_match(text))
            && !budget_obstacle_re().is_match(text)
        {
            push_unique(&mut criteria, DecisionCriterion::Price);
        }
    }
    if re!(r"backsplash", "i").is_match(text)
        && re!(r"\bprice\b", "i").is_match(text)
        && re!(r"depend|might|not sure|unsure|thinking about", "i").is_match(text)
    {
        push_unique(&mut criteria, DecisionCriterion::Price);
    }
    criteria
}

fn budget_lines(text: &str, keep: &regex::Regex) -> Option<String> {
    let lines: Vec<String> = text
        .split('\n')
        .map(|line| {
            trim(&re!(r"^(Customer message|Lead details):\s*", "iu").replace(line, "")).to_string()
        })
        .filter(|line| !line.is_empty() && keep.is_match(line))
        .collect();
    if lines.is_empty() {
        return None;
    }
    let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
    Some(unique_evidence_chunks(&refs).join(" "))
}

/// What the history says about money: a known budget, an obstacle, a cap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BudgetSemantics {
    pub known_evidence: Option<String>,
    pub obstacle_evidence: Option<String>,
    pub constraint: bool,
    pub source: EvidenceSource,
    pub confidence: EvidenceConfidence,
}

pub fn budget_semantics(customer_corpus: &str, profile: &str) -> BudgetSemantics {
    let customer_known = budget_lines(customer_corpus, budget_known_re());
    let customer_obstacle = budget_lines(customer_corpus, budget_obstacle_re());
    let profile_known = budget_lines(profile, budget_known_re());
    let profile_obstacle = budget_lines(profile, budget_obstacle_re());
    let constraint = budget_constraint_re().is_match(&format!("{customer_corpus}\n{profile}"));
    let (known_evidence, obstacle_evidence, source, confidence) = if customer_obstacle.is_some() {
        (
            customer_known,
            customer_obstacle,
            EvidenceSource::CustomerSms,
            EvidenceConfidence::High,
        )
    } else if customer_known.is_some() {
        (
            customer_known,
            None,
            EvidenceSource::CustomerSms,
            EvidenceConfidence::High,
        )
    } else if profile_obstacle.is_some() {
        (
            profile_known,
            profile_obstacle,
            EvidenceSource::SalespersonNote,
            EvidenceConfidence::Medium,
        )
    } else if profile_known.is_some() {
        (
            profile_known,
            None,
            EvidenceSource::SalespersonNote,
            EvidenceConfidence::Medium,
        )
    } else {
        (None, None, EvidenceSource::None, EvidenceConfidence::Low)
    };
    BudgetSemantics {
        known_evidence,
        obstacle_evidence,
        constraint,
        source,
        confidence,
    }
}

pub(crate) fn material_selection_history_re() -> &'static regex::Regex {
    re!(
        r"stones you picked out|picked out (our |the )?(stones|materials)|the stones you (looked at|picked)",
        "i"
    )
}

fn historical_expiration_re() -> &'static regex::Regex {
    re!(
        r"quote will expire soon|let (him|her|them) know that (his|her|their) quote will expire",
        "i"
    )
}

/// The quote's expiration date, from an "expiration quote" CRM task.
pub fn extract_quote_expiration(profile: &str) -> (Option<String>, Option<Instant>) {
    if historical_expiration_re().is_match(profile)
        || !re!(
            r"expiration quote|quote expir|expires?\s+(on|by)|valid until",
            "i"
        )
        .is_match(profile)
    {
        return (None, None);
    }
    let number = |caps: &regex::Captures<'_>, i: usize| {
        caps.get(i).and_then(|m| m.as_str().parse::<i32>().ok())
    };
    let date = re!(r"\b(\d{1,2})\/(\d{1,2})\/(\d{4})\b")
        .captures(profile)
        .map_or_else(
            || {
                let iso = re!(r"\b(\d{4})-(\d{2})-(\d{2})\b").captures(profile)?;
                Some(utc_date(
                    number(&iso, 1)?,
                    number(&iso, 2)? - 1,
                    number(&iso, 3)?,
                ))
            },
            |mdy| {
                Some(utc_date(
                    number(&mdy, 3)?,
                    number(&mdy, 1)? - 1,
                    number(&mdy, 2)?,
                ))
            },
        );
    let evidence = profile
        .split('\n')
        .find(|line| re!(r"expiration quote|quote expir", "i").is_match(line))
        .map_or_else(
            || "expiration quote".to_string(),
            |line| trim(&re!(r"^Activity:\s*", "u").replace(line, "")).to_string(),
        );
    (Some(evidence), date)
}

/// An old "let her know the quote will expire" task.
pub fn extract_historical_quote_expiration_warning(profile: &str) -> bool {
    historical_expiration_re().is_match(profile)
}
