//! Quotes, prices and budgets in a customer's history (part of
//! `smsFlowAiFollowupFacts.ts`).

use super::super::js::{js_round, trim};
use super::materials::material_name_re;
use super::types::{EvidenceConfidence, EvidenceSource, PriceContext, QuoteType};
use crate::{fre, re};
use regex::Regex;

pub fn construction_evidence_fancy() -> &'static fancy_regex::Regex {
    fre!(
        r"waiting for cabinets|cabinets not installed|cabinets? (are |is )?(almost ready.{0,40})?(unlevel|not level|bot leveled|unleveled)|almost ready for counter|waiting for (the )?floors|flooring (is )?on order|floors to arrive|until (the )?floors are in place|no decisions will be made until|new build not ready|contractor delayed|construction delayed|\bnew build\b(?!er)|house (is )?being built",
        "i"
    )
}

pub fn budget_known_re() -> &'static Regex {
    re!(
        r"\bbudget is set\b|\bour budget is\b|\bthe budget is\b|\bwe have a budget\b|\bbudget of \$|\bbudget:\s*\$|\bbudget around\b|\baround \$\s*\d{4,}",
        "i"
    )
}

pub fn budget_obstacle_re() -> &'static Regex {
    re!(
        r"\bfinanc|\btrying to get (the )?money\b|\bcan'?t afford\b|\bover (my |our |the )?budget\b|\bneed (a )?payment plan\b|\bcheaper options?\b|\bdon'?t have enough money\b|\bnot enough money for\b|\bsomething for \$?\d",
        "i"
    )
}

pub fn budget_constraint_re() -> &'static Regex {
    re!(
        r"\bstay under\b|\bunder \$\d|\bmax\b.{0,20}\$|\bonly want to spend\b",
        "i"
    )
}

pub fn timeline_evidence_re() -> &'static Regex {
    re!(
        r"\basap\b|\bthis month\b|\bnext (week|month)\b|\bby (january|february|march|april|may|june|july|august|september|october|november|december)\b|\bready (in|on|by)\b|\burgent\b|\bimmediately\b|\bbeginning of (the |next )?year\b|\bstart of (the |next )?year\b|\blate (january|february|march|april|may|june|july|august|september|october|november|december)\b|\bearly (january|february|march|april|may|june|july|august|september|october|november|december)\b",
        "i"
    )
}

pub fn negative_option_re() -> &'static Regex {
    re!(
        r"didn'?t find anything (she |he |they )?likes|did not find .{0,40}(she |he |they )?likes|nothing .{0,24}(she |he |they )?likes|couldn'?t find .{0,24}(she |he |they )?likes",
        "i"
    )
}

pub fn specials_next_step_re() -> &'static Regex {
    re!(
        r"send (you |her |him |them )?(our )?(current )?speci[ae]ls|speci[ae]ls closer",
        "i"
    )
}

pub fn alternative_interest_re() -> &'static Regex {
    re!(r"interested in[\s\S]{0,160}\bor\b", "i")
}

pub fn salesperson_task_only_re() -> &'static Regex {
    re!(
        r"^(call\b|be persistent|follow up|check again|call again|appointment at)\b",
        "i"
    )
}

fn quote_request_re() -> &'static Regex {
    re!(
        r"give me (a |an )?quote|need (a |an )?quote|can you .{0,60}quote|come .{0,40}(and )?(give|do) (me )?(a )?quote|request for (an? )?(quote|estimate)|interested in (getting )?(an? )?(quote|estimate)|want (a |an )?(quote|estimate)|(?:i'?d like to |like to |want to |could )get (a |an )?(quote|estimate)|(?:i'?d appreciate it if )?i could get a quote|get a quote for (each of )?those|quote for those|good to know the price|checking on the quotes|know the price of|start working on your quote|looking to get (a |an )?quote|looking for (a |an )?quote|how many square feet.{0,40}(?:cost|price|\?)|what'?s the cost|what is the cost|what would this cost|how much would (?:my |the |this )?project|can you price this|price this for me|can you give me an estimate|how much will this cost",
        "i"
    )
}

pub fn quote_sent_fancy() -> &'static fancy_regex::Regex {
    fre!(
        r"just sent (?:you )?(?:(?:\d+|three|two|four|five|several)\s+)?(all )?(the |your )?(updated )?quotes?|sent (?:you )?(?:(?:\d+|three|two|four|five|several)\s+)?(all )?(the |your )?(updated )?quotes?( to your email)?|quotes? (was |were )?(sent|emailed|attached)|quote i sent|attached your quote|emailed (the |your )?quotes?|^Activity:.*\bsent quote\b|\bgot (?:the |your )(?:\w+\s+)?quote\b|\bgot (?!a quote\b)\w{3,}\s+quote\b",
        "im"
    )
}

pub fn quote_created_fancy() -> &'static fancy_regex::Regex {
    fre!(
        r"\bhave a quote\b|make (a |the )?(cabinet )?quote|get the (cabinet )?quote|quote (was )?created|prepared (a |the )?quote|inhome estimate.{0,40}quote|got (?:the |your )(?:\w+\s+)?quote\b|got (?!a quote\b)\w{3,}\s+quote\b",
        "i"
    )
}

pub fn quote_received_fancy() -> &'static fancy_regex::Regex {
    fre!(
        r"\bi got it\b|got (?:the |your )(?:\w+\s+)?quote\b|got (?!a quote\b)\w{3,}\s+quote\b|received (the |your )?quote|i('ll| will) review (it|the quote)|looked at (the |your )?quote|look(ed)? (it )?over|did look it over|we did look it over|\bbriefly looked at it\b|\blooked at it\b|(?:have not|haven't) .{0,40}review (?:the )?quote|signed (the |our |your )?contract",
        "i"
    )
}

pub fn quote_price_eval_re() -> &'static Regex {
    re!(
        r"your (numbers|quote|pricing|price).{0,80}(highest|high|expensive|too much|higher)|the numbers (are|came in)|highest of all my bids|i like the quote|can you change this price|quote is too expensive|numbers are (higher|too high)",
        "i"
    )
}

pub fn quote_discussed_re() -> &'static Regex {
    re!(
        r"review(ed|ing)? (the |your )?quote|chance to review (the |your )?quote|look(ed|ing)? at (the |your )?quote|the quote we",
        "i"
    )
}

fn quote_existence_re() -> &'static Regex {
    re!(
        r"quote (was )?(sent|created|emailed|attached)|quote i sent|quote is ready|attached your quote|existing quote|have a quote|check the quote|expiration quote|cabinet quote|countertop quote|dekton quote|got (a |the |your )quote|received (the |your )?quote|looked at (the |your )?quote|just sent (you )?(the |your )?quote",
        "i"
    )
}

fn short_non_superseding_re() -> &'static Regex {
    re!(
        r"^(not yet|not right now|ok|okay|thanks|thank you|got it|yes|no)[.!]?\s*$",
        "i"
    )
}

pub fn comparing_quotes_re() -> &'static Regex {
    re!(
        r"compare with the other|comparing quotes|compare prices|wants to compare|other ones i am getting|other quotes|collecting prices|more quotes|shop(ping)? around|highest of all my bids|highest bid|much higher than the others|better price elsewhere|went with someone cheaper|going with another company|numbers came in at the highest|your numbers are too high|quote is much higher",
        "i"
    )
}

pub fn price_rejection_re() -> &'static Regex {
    re!(
        r"highest of all my bids|highest bid|your numbers are too high|quote is (too )?(expensive|high)|went with someone cheaper|better price elsewhere|going with another company|thank you for your time|to be honest.{0,24}price|it'?s the price",
        "i"
    )
}

pub fn terminal_price_rejection_re() -> &'static Regex {
    re!(
        r"thank you for your time|went with someone cheaper|going with another company|already (went|went ahead) with",
        "i"
    )
}

pub fn quality_factor_re() -> &'static Regex {
    re!(r"\bquality\b", "i")
}

pub fn price_factor_re() -> &'static Regex {
    re!(r"\bprice\b|\bpricing\b|\bcheaper\b|\bbudget\b", "i")
}

pub fn spouse_decision_re() -> &'static Regex {
    re!(
        r"need to ask (my |our )?(wife|husband|spouse|partner)|we need to decide together|(wife|husband|spouse|partner) hasn'?t decided|waiting for (my |our )?(wife|husband|spouse|partner)|has to (talk|discuss|check) with (his |her |their )?(wife|husband|spouse|partner)",
        "i"
    )
}

pub fn appointment_companion_re() -> &'static Regex {
    re!(r"\b(wife|husband|spouse|partner)\b", "i")
}

fn competitor_quote_reference_re() -> &'static Regex {
    re!(
        r"\b(?:estimate|quote|bid|pricing)\b.{0,48}\b(?:from|with|at)\b.{0,48}\b(?:them|home depot|lowe'?s|lowes|menards|another (?:company|contractor|vendor|supplier|fabricator)|other (?:companies|quotes|bids|estimates)|competitor)\b|\b(?:getting|waiting for|once i get|after i get|when i get).{0,48}\b(?:estimate|quote|bid)\b.{0,48}\b(?:from|with)\b|\b(?:two|2|other|another|few|more)\s+(?:other\s+)?(?:quotes|estimates|bids)\b|\breschedule.{0,48}(?:home depot|lowe'?s|lowes)\b|\bestimate from them\b|\bquote from them\b",
        "i"
    )
}

fn fancy(re: &fancy_regex::Regex, text: &str) -> bool {
    super::super::js::fancy_test(re, text)
}

pub fn text_looks_like_competitor_quote_reference(text: &str) -> bool {
    competitor_quote_reference_re().is_match(text)
}

pub fn text_looks_like_quote_request(text: &str) -> bool {
    !text_looks_like_competitor_quote_reference(text) && quote_request_re().is_match(text)
}

pub fn text_looks_like_quote_existence(text: &str) -> bool {
    !text_looks_like_quote_request(text) && quote_existence_re().is_match(text)
}

pub fn text_looks_like_quote_created(text: &str) -> bool {
    if text_looks_like_quote_request(text) && !fancy(quote_created_fancy(), text) {
        return false;
    }
    fancy(quote_created_fancy(), text)
}

fn strip_deal_stage_got_a_quote(text: &str) -> String {
    let without = re!(r"\bDeal:.*· Got a Quote\b", "gi").replace_all(text, "");
    re!(r"· stage Got a Quote\b", "gi")
        .replace_all(&without, "")
        .into_owned()
}

pub fn text_looks_like_quote_sent(text: &str) -> bool {
    fancy(quote_sent_fancy(), &strip_deal_stage_got_a_quote(text))
}

pub fn text_looks_like_quote_received(text: &str) -> bool {
    fancy(quote_received_fancy(), &strip_deal_stage_got_a_quote(text))
}

pub fn text_looks_like_quote_discussed(text: &str) -> bool {
    quote_discussed_re().is_match(text) || quote_price_eval_re().is_match(text)
}

pub fn text_looks_like_signed_contract(text: &str) -> bool {
    re!(
        r"signed (the |our |your )?contract|contract (is |was )?(signed|executed)|we signed",
        "i"
    )
    .is_match(text)
}

pub fn text_looks_like_payment_received(text: &str) -> bool {
    re!(
        r"we (just )?received (your |the )?(check|payment|deposit)|received your check|check (was |has been )?received|payment (was |has been )?received|deposit (was |has been )?received",
        "i"
    )
    .is_match(text)
}

pub fn text_looks_like_company_scheduling_obligation(text: &str) -> bool {
    re!(
        r"scheduling manager should call|our scheduling (manager|team) (should|will) call|scheduler (should|will) call you|scheduling will (call|contact)",
        "i"
    )
    .is_match(text)
}

pub fn is_short_non_superseding_reply(text: &str) -> bool {
    short_non_superseding_re().is_match(trim(text))
}

/// The quote evidence is real enough to mention.
pub fn quote_evidence_is_verified(
    quote_evidence: bool,
    source: EvidenceSource,
    confidence: EvidenceConfidence,
) -> bool {
    quote_evidence
        && source != EvidenceSource::None
        && matches!(
            confidence,
            EvidenceConfidence::Medium | EvidenceConfidence::High
        )
}

/// `normalizeProductScopeTypos`.
pub fn normalize_product_scope_typos(text: &str) -> String {
    re!(r"counterops", "gi")
        .replace_all(text, "countertops")
        .into_owned()
}

/// What kind of quote the history talks about, when there is one.
pub fn infer_quote_type(profile: &str, quote_evidence: bool) -> Option<QuoteType> {
    if !quote_evidence {
        return None;
    }
    let mut cleaned = normalize_product_scope_typos(profile);
    for (pattern, flags) in [
        (r"https?:\/\/\S+", "gi"),
        (r"granite depot", "gi"),
        (r"granite-manager\.com\S*", "gi"),
        (r"save \d+%[^\n]*", "gi"),
        (
            r"natural stone countertops have been on your wishlist[^\n]*",
            "gi",
        ),
    ] {
        cleaned = super::super::js::js_regex(pattern, flags)
            .replace_all(&cleaned, " ")
            .into_owned();
    }
    let cabinet = re!(r"cabinet quote|quote for cabinets|cabinets? quote", "i").is_match(&cleaned);
    let countertop = re!(
        r"countertop quote|stone quote|slab quote|granite quote|quartz quote|dekton quote",
        "i"
    )
    .is_match(&cleaned);
    let remodel = re!(r"full remodel|whole (house|home) remodel", "i").is_match(&cleaned);
    let material = material_name_re().is_match(&cleaned)
        || (re!(r"\b(quartz|granite|marble|quartzite)\b", "i").is_match(&cleaned)
            && re!(r"customer message:|lead details:|deal note ", "i").is_match(&cleaned));
    if remodel || (cabinet && countertop) {
        return Some(QuoteType::FullRemodel);
    }
    if cabinet && !countertop {
        return Some(QuoteType::Cabinet);
    }
    if countertop || (material && !cabinet) {
        return Some(QuoteType::Countertop);
    }
    if re!(r"existing quote", "i").is_match(&cleaned)
        && re!(r"measurements|price breakdown", "i").is_match(&cleaned)
        && !cabinet
    {
        return Some(QuoteType::Countertop);
    }
    Some(QuoteType::Unknown)
}

fn has_price(line: &str) -> bool {
    re!(r"(\$|\d+\s*k\b)", "i").is_match(line)
}

/// What a dollar amount on a line is (budget, quote, package...).
pub fn classify_price_context(text: &str) -> PriceContext {
    let line = trim(text);
    if line.is_empty() {
        return PriceContext::None;
    }
    if re!(r"package|special\b|remodel package", "i").is_match(line) && has_price(line) {
        return PriceContext::PackagePrice;
    }
    if re!(r"\b\d+%\s*off\b|\bsale\b|\bpromo", "i").is_match(line) && has_price(line) {
        return PriceContext::PromotionPrice;
    }
    if re!(r"\bquote\b", "i").is_match(line) && has_price(line) {
        return PriceContext::QuoteAmount;
    }
    if re!(r"\bestimate\b", "i").is_match(line) && has_price(line) {
        return PriceContext::EstimateAmount;
    }
    if re!(r"\bbudget is set\b", "i").is_match(line) {
        return PriceContext::None;
    }
    if re!(
        r"\bbudget around\b|\baround \$\s*\d|\bour budget is\b|\bthe budget is \$",
        "i"
    )
    .is_match(line)
        || (re!(r"\bbudget is\b", "i").is_match(line)
            && re!(r"\$\s*\d|\b\d+\s*k\b", "i").is_match(line))
    {
        if re!(r"\bunder\b|\bstay under\b|\bmax\b", "i").is_match(line) {
            return PriceContext::BudgetMax;
        }
        return PriceContext::BudgetTarget;
    }
    if re!(
        r"\$\s*\d+(?:\.\d+)?\s*\/\s*(?:sq\.?\s*ft|sqft|sf)\b|\b\d+\s*\/\s*sqft",
        "i"
    )
    .is_match(line)
    {
        return PriceContext::MaterialPrice;
    }
    if budget_obstacle_re().is_match(line) || budget_constraint_re().is_match(line) {
        if re!(r"\bunder\b|\bstay under\b|\bmax\b", "i").is_match(line) {
            return PriceContext::BudgetMax;
        }
        if budget_constraint_re().is_match(line) {
            return PriceContext::BudgetMax;
        }
        if re!(r"\bsomething for \$?\d|\bunder \$?\d", "i").is_match(line)
            && parse_money_amount(line).is_some()
        {
            return PriceContext::BudgetTarget;
        }
        return PriceContext::None;
    }
    if re!(r"\bover (my |our |the )?budget\b|\btoo much\b", "i").is_match(line) {
        return PriceContext::None;
    }
    if re!(r"\$\s*\d|\b\d+\s*k\b", "i").is_match(line) {
        return PriceContext::UnknownPrice;
    }
    PriceContext::None
}

/// A dollar amount on a line: `5k`, `$5,000`.
pub fn parse_money_amount(text: &str) -> Option<f64> {
    if let Some(k) = re!(r"\b(\d+(?:\.\d+)?)\s*k\b", "i")
        .captures(text)
        .and_then(|caps| caps.get(1))
    {
        return k
            .as_str()
            .parse::<f64>()
            .ok()
            .map(|value| js_round(value * 1000.0));
    }
    re!(r"\$\s*(\d{1,3}(?:,\d{3})+|\d+)")
        .captures(text)
        .and_then(|caps| caps.get(1))
        .and_then(|dollars| dollars.as_str().replace(',', "").parse::<f64>().ok())
}

/// The customer's budget, when a line states one.
pub fn extract_budget_target_approx(text: &str) -> Option<f64> {
    for raw in text.split('\n') {
        let context = classify_price_context(raw);
        if matches!(
            context,
            PriceContext::BudgetTarget | PriceContext::BudgetMax
        ) && let Some(amount) = parse_money_amount(raw)
        {
            return Some(amount);
        }
        if (budget_obstacle_re().is_match(raw) || budget_constraint_re().is_match(raw))
            && let Some(amount) = parse_money_amount(raw)
        {
            return Some(amount);
        }
    }
    None
}

/// The first budget context of the text, else the first price context.
pub fn extract_primary_price_context(text: &str) -> PriceContext {
    let mut found = PriceContext::None;
    for raw in text.split('\n') {
        let context = classify_price_context(raw);
        if matches!(
            context,
            PriceContext::BudgetTarget | PriceContext::BudgetMax
        ) {
            return context;
        }
        if context != PriceContext::None && found == PriceContext::None {
            found = context;
        }
    }
    found
}
