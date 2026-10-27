//! North American phone number validity as the CRM checks it
//! (`isValidNanpPhone`: libphonenumber-js `isValidPhoneNumber(digits, 'US')`
//! with its minimal metadata, embedded in `nanp_metadata.json`).

use serde::Deserialize;
use std::sync::OnceLock;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct NumberType {
    #[serde(rename = "type")]
    kind: String,
    pattern: String,
    possible_lengths: Option<Vec<usize>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Country {
    leading_digits: Option<String>,
    national_number_pattern: String,
    has_types: bool,
    types: Vec<NumberType>,
}

struct Plan {
    leading_digits: Option<regex::Regex>,
    national_number: regex::Regex,
    has_types: bool,
    /// (type, whole-match pattern, possible lengths), empty patterns dropped.
    types: Vec<(String, regex::Regex, Option<Vec<usize>>)>,
}

fn whole(pattern: &str) -> regex::Regex {
    regex::Regex::new(&format!("^(?:{pattern})$"))
        .unwrap_or_else(|err| panic!("phone pattern {pattern}: {err}"))
}

fn plans() -> &'static [Plan] {
    static PLANS: OnceLock<Vec<Plan>> = OnceLock::new();
    PLANS.get_or_init(|| {
        let countries: Vec<Country> = serde_json::from_str(include_str!("nanp_metadata.json"))
            .unwrap_or_else(|err| panic!("NANP metadata: {err}"));
        countries
            .into_iter()
            .map(|country| Plan {
                leading_digits: country.leading_digits.filter(|d| !d.is_empty()).map(|d| {
                    regex::Regex::new(&format!("^(?:{d})"))
                        .unwrap_or_else(|err| panic!("leading digits {d}: {err}"))
                }),
                national_number: whole(&country.national_number_pattern),
                has_types: country.has_types,
                types: country
                    .types
                    .into_iter()
                    .filter(|t| !t.pattern.is_empty())
                    .map(|t| (t.kind, whole(&t.pattern), t.possible_lengths))
                    .collect(),
            })
            .collect()
    })
}

/// `getNumberType(...) !== undefined`: the number matches the plan and one
/// of its number types.
fn has_number_type(plan: &Plan, number: &str) -> bool {
    plan.national_number.is_match(number)
        && plan.types.iter().any(|(_, pattern, lengths)| {
            lengths
                .as_ref()
                .is_none_or(|lengths| lengths.contains(&number.len()))
                && pattern.is_match(number)
        })
}

fn is_valid_national_number(number: &str) -> bool {
    let plans = plans();
    // The country by leading digits, else the first whose types match; US by default.
    let plan = plans
        .iter()
        .find(|plan| {
            plan.leading_digits.as_ref().map_or_else(
                || has_number_type(plan, number),
                |leading| leading.is_match(number),
            )
        })
        .unwrap_or(&plans[0]);
    if plan.has_types {
        has_number_type(plan, number)
    } else {
        plan.national_number.is_match(number)
    }
}

/// A valid 10-digit NANP number (an 11-digit one with a leading 1 too).
pub fn is_valid_nanp_phone(phone: &str) -> bool {
    let digits: String = phone.chars().filter(char::is_ascii_digit).collect();
    if digits.len() == 11 && digits.starts_with('1') {
        return is_valid_nanp_phone(&digits[1..]);
    }
    let bytes = digits.as_bytes();
    digits.len() == 10
        && (b'2'..=b'9').contains(&bytes[0])
        && (b'2'..=b'9').contains(&bytes[3])
        && is_valid_national_number(&digits)
}

pub fn any_valid_nanp_phone(phones: &[String]) -> bool {
    phones.iter().any(|phone| is_valid_nanp_phone(phone))
}
