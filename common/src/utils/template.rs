use crate::crud::template::{InfoVariableData, TemplateVariableData};
use chrono::Local;
use std::collections::HashMap;

fn get_first_name(full_name: &str) -> String {
    full_name.split(' ').next().unwrap_or(full_name).to_string()
}

/// Everything after the first word ("Mary Ann Smith" -> "Ann Smith"); none
/// for a one-word name.
fn get_last_name(full_name: &str) -> Option<String> {
    let rest: Vec<&str> = full_name.split_whitespace().skip(1).collect();
    (!rest.is_empty()).then(|| rest.join(" "))
}

/// `12345.5` -> `$12,345.50`.
pub fn format_usd(amount: f64) -> String {
    let cents = (amount.abs() * 100.0).round();
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let cents = cents as u64;
    let digits = (cents / 100).to_string();
    let mut whole = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            whole.push(',');
        }
        whole.push(c);
    }
    let sign = if amount < 0.0 && cents > 0 { "-" } else { "" };
    format!("{sign}${whole}.{:02}", cents % 100)
}

/// `2026-10-14` -> `October 14, 2026`; anything else is kept as it is.
fn format_long_date(raw: &str) -> String {
    let raw = raw.trim();
    chrono::NaiveDate::parse_from_str(raw.get(..10).unwrap_or(raw), "%Y-%m-%d")
        .map_or_else(|_| raw.to_string(), |d| d.format("%B %-d, %Y").to_string())
}

fn finite_usd(amount: Option<f64>) -> Option<String> {
    amount.filter(|a| a.is_finite()).map(format_usd)
}

/// The `{{key}}` names in `text`, trimmed, in order (no markup handling).
pub fn placeholder_keys(text: &str) -> Vec<String> {
    let mut keys = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find("{{") {
        let after = &rest[start + 2..];
        let Some(end) = after.find("}}") else {
            break;
        };
        keys.push(after[..end].trim().to_string());
        rest = &after[end + 2..];
    }
    keys
}

fn format_current_date() -> String {
    // %e produces space-padded day (e.g. " 3"), so we clean up double spaces
    Local::now()
        .format("%B %e")
        .to_string()
        .replace("  ", " ")
        .trim()
        .to_string()
}

fn inventory_subdomain_slug(raw: &str) -> Option<String> {
    let slug = raw
        .trim()
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .split('.')
        .next()
        .unwrap_or("")
        .trim()
        .to_lowercase();
    if slug.is_empty() { None } else { Some(slug) }
}

fn company_subdomain_slug(company: Option<&InfoVariableData>) -> Option<String> {
    let company = company?;
    let raw = company
        .subdomain
        .as_ref()
        .filter(|value| !value.trim().is_empty())
        .or(company
            .domain
            .as_ref()
            .filter(|value| !value.trim().is_empty()))?;
    inventory_subdomain_slug(raw)
}

fn company_id_value(company: Option<&InfoVariableData>) -> Option<String> {
    company.and_then(|c| c.id).map(|id| id.to_string())
}

fn customer_inventory_link(company: Option<&InfoVariableData>) -> Option<String> {
    let slug = company_subdomain_slug(company)?;
    let id = company_id_value(company)?;
    Some(format!(
        "https://{slug}.granite-manager.com/customer/{id}/stones"
    ))
}

fn build_variable_map(data: &TemplateVariableData) -> HashMap<&'static str, String> {
    let customer = data.customer.as_ref();
    let company = data.company.as_ref();
    let deal = data.deal.as_ref();
    let quote = data.quote.as_ref();
    let first_name = data.user.name.as_ref().map(|n| get_first_name(n));
    let customer_name = customer.and_then(|c| c.name.as_ref().map(|n| get_first_name(n)));

    [
        ("user.name", data.user.name.clone()),
        ("user.first_name", first_name),
        (
            "user.last_name",
            data.user.name.as_deref().and_then(get_last_name),
        ),
        ("user.email", data.user.email.clone()),
        ("user.phone_number", data.user.phone_number.clone()),
        ("customer.name", customer.and_then(|c| c.name.clone())),
        ("customer.first_name", customer_name),
        (
            "customer.last_name",
            customer.and_then(|c| c.name.as_deref().and_then(get_last_name)),
        ),
        ("customer.email", customer.and_then(|c| c.email.clone())),
        ("customer.phone", customer.and_then(|c| c.phone.clone())),
        ("customer.address", customer.and_then(|c| c.address.clone())),
        ("deal.name", deal.and_then(|d| d.name.clone())),
        ("deal.amount", deal.and_then(|d| finite_usd(d.amount))),
        ("quote.name", quote.and_then(|q| q.name.clone())),
        ("quote.total", quote.and_then(|q| finite_usd(q.total))),
        ("quote.color", quote.and_then(|q| q.color.clone())),
        ("quote.link", quote.and_then(|q| q.link.clone())),
        (
            "quote.expiration",
            quote.and_then(|q| q.expiration.as_deref().map(format_long_date)),
        ),
        ("customer.inventory_link", customer_inventory_link(company)),
        ("company.name", company.and_then(|c| c.name.clone())),
        ("company.address", company.and_then(|c| c.address.clone())),
        ("company.subdomain", company_subdomain_slug(company)),
        ("company.id", company_id_value(company)),
        (
            "company.hours_of_operation",
            company.and_then(|c| c.hours_of_operation.clone()),
        ),
        ("company.domain", company.and_then(|c| c.domain.clone())),
        ("current_date", Some(format_current_date())),
    ]
    .into_iter()
    .filter_map(|(k, v)| v.filter(|s| !s.trim().is_empty()).map(|val| (k, val)))
    .collect()
}

/// Replaces `{{variable.key}}` placeholders using fetched template data.
///
/// Matching is case-insensitive for the key (`{{Customer.first_name}}` and
/// `{{customer.first_name}}` both resolve), because templates authored in the
/// editor sometimes capitalize variable segments.
pub fn replace_template_variables(template: &str, data: &TemplateVariableData) -> String {
    let map = build_variable_map(data);
    let mut result = String::with_capacity(template.len());
    let mut rest = template;

    while let Some(start) = rest.find("{{") {
        result.push_str(&rest[..start]);
        let after_open = &rest[start + 2..];
        match after_open.find("}}") {
            Some(end) => {
                let key = &after_open[..end];
                match map
                    .iter()
                    .find(|(known_key, _)| known_key.eq_ignore_ascii_case(key))
                    .map(|(_, value)| value.as_str())
                {
                    Some(value) => result.push_str(value),
                    None => {
                        result.push_str("{{");
                        result.push_str(key);
                        result.push_str("}}");
                    }
                }
                rest = &after_open[end + 2..];
            }
            None => {
                result.push_str("{{");
                rest = after_open;
            }
        }
    }

    result.push_str(rest);
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crud::template::{InfoVariableData, UserVariableData};

    fn make_full_data() -> TemplateVariableData {
        TemplateVariableData {
            user: UserVariableData {
                name: Some("Alice Johnson".to_string()),
                email: Some("alice@test.com".to_string()),
                email_name: Some("Alice Johnson".to_string()),
                phone_number: Some("555-1234".to_string()),
            },
            customer: Some(InfoVariableData {
                name: Some("Jordan Smith".to_string()),
                address: Some("456 Market St".to_string()),
                ..Default::default()
            }),
            company: Some(InfoVariableData {
                id: Some(1),
                name: Some("Granite Depot".to_string()),
                address: Some("123 Main St".to_string()),
                hours_of_operation: Some("Monday - Friday 9 to 6, Saturday 10 to 3".to_string()),
                domain: Some("example.granite-manager.com".to_string()),
                subdomain: Some("example".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[test]
    fn replaces_customer_first_name_in_thank_you_template() {
        let template = r#"<p><span style="color: rgb(0, 0, 0); background-color: transparent;">Hi {{customer.first_name}},</span></p><p><span style="color: rgb(0, 0, 0); background-color: transparent;">Thank you for your request! My name is {{user.first_name}}, and I'm the sales representative who will be helping you with your kitchen project.</span></p><p><br></p><p><span style="color: rgb(0, 0, 0); background-color: transparent;">I’ll be in touch with you shortly to learn more about your project and help you with the next steps.</span></p><p><span style="color: rgb(0, 0, 0); background-color: transparent;">In the meantime, you can browse our live stone inventory here:</span></p><p><u style="color: rgb(17, 85, 204); background-color: transparent;"><a href="{{customer.inventory_link}}" rel="noopener noreferrer" target="_blank">{{customer.inventory_link}}</a></u></p><p><span style="color: rgb(0, 0, 0); background-color: transparent;">We offer a wide selection of natural and man-made stone options for a variety of kitchen styles and projects.</span></p><p><br></p><p><span style="color: rgb(0, 0, 0); background-color: transparent;">I look forward to working with you!</span></p>"#;

        let result = replace_template_variables(template, &make_full_data());

        assert!(result.contains("Hi Jordan,"));
        assert!(result.contains("My name is Alice,"));
        assert!(result.contains("https://example.granite-manager.com/customer/1/stones"));
        assert!(!result.contains("{{customer.first_name}}"));
        assert!(!result.contains("{{user.first_name}}"));
        assert!(!result.contains("{{customer.inventory_link}}"));
    }

    #[test]
    fn replaces_customer_first_name_when_placeholder_is_capitalized() {
        // Real Indianapolis drip template used {{Customer.first_name}} and customers
        // received the literal placeholder because matching used to be case-sensitive.
        let template =
            "<p>Hi {{Customer.first_name}}, this is {{User.first_name}} with {{Company.name}}.</p>";
        let result = replace_template_variables(template, &make_full_data());

        assert_eq!(
            result,
            "<p>Hi Jordan, this is Alice with Granite Depot.</p>"
        );
        assert!(!result.contains("{{Customer.first_name}}"));
        assert!(!result.contains("{{User.first_name}}"));
        assert!(!result.contains("{{Company.name}}"));
    }

    #[test]
    fn leaves_customer_first_name_when_customer_data_is_missing() {
        let data = TemplateVariableData {
            user: UserVariableData {
                name: Some("Alice Johnson".to_string()),
                email: Some("alice@test.com".to_string()),
                email_name: None,
                phone_number: None,
            },
            ..Default::default()
        };
        let result = replace_template_variables("Hi {{customer.first_name}}", &data);

        assert_eq!(result, "Hi {{customer.first_name}}");
    }

    #[test]
    fn leaves_customer_first_name_when_customer_name_is_empty() {
        let mut data = make_full_data();
        data.customer = Some(InfoVariableData {
            name: Some("".to_string()),
            address: Some("456 Market St".to_string()),
            ..Default::default()
        });
        let result = replace_template_variables("Hi {{customer.first_name}}", &data);

        assert_eq!(result, "Hi {{customer.first_name}}");
    }

    #[test]
    fn replaces_customer_first_name_from_full_customer_name() {
        let result =
            replace_template_variables("Hello {{customer.first_name}}!", &make_full_data());
        assert_eq!(result, "Hello Jordan!");
    }

    #[test]
    fn replaces_all_lead_drip_variables_used_in_default_templates() {
        let template = "Hi {{customer.first_name}} from {{company.name}} at {{company.address}} open {{company.hours_of_operation}}. Rep {{user.first_name}} ({{user.name}}) inventory {{customer.inventory_link}}";
        let result = replace_template_variables(template, &make_full_data());

        assert_eq!(
            result,
            "Hi Jordan from Granite Depot at 123 Main St open Monday - Friday 9 to 6, Saturday 10 to 3. Rep Alice (Alice Johnson) inventory https://example.granite-manager.com/customer/1/stones"
        );
    }

    #[test]
    fn replaces_company_subdomain_and_id_in_inventory_url() {
        let result = replace_template_variables(
            "https://{{company.subdomain}}.granite-manager.com/customer/{{company.id}}/stones",
            &make_full_data(),
        );
        assert_eq!(
            result,
            "https://example.granite-manager.com/customer/1/stones"
        );
    }

    #[test]
    fn replaces_customer_inventory_link_from_company_subdomain_and_id() {
        let result = replace_template_variables("{{customer.inventory_link}}", &make_full_data());
        assert_eq!(
            result,
            "https://example.granite-manager.com/customer/1/stones"
        );
    }

    #[test]
    fn leaves_customer_inventory_link_when_company_id_is_missing() {
        let mut data = make_full_data();
        if let Some(company) = data.company.as_mut() {
            company.id = None;
        }
        let result = replace_template_variables("{{customer.inventory_link}}", &data);
        assert_eq!(result, "{{customer.inventory_link}}");
    }

    #[test]
    fn leaves_company_subdomain_and_id_when_company_is_missing() {
        let mut data = make_full_data();
        data.company = None;
        let result = replace_template_variables(
            "https://{{company.subdomain}}.granite-manager.com/customer/{{company.id}}/stones",
            &data,
        );
        assert_eq!(
            result,
            "https://{{company.subdomain}}.granite-manager.com/customer/{{company.id}}/stones"
        );
    }

    #[test]
    fn does_not_partially_replace_overlapping_variable_names() {
        let result =
            replace_template_variables("{{user.name}} vs {{user.first_name}}", &make_full_data());
        assert_eq!(result, "Alice Johnson vs Alice");
    }

    #[test]
    fn leaves_unknown_placeholders_untouched() {
        let result = replace_template_variables(
            "{{unknown.var}} and {{customer.first_name}}",
            &make_full_data(),
        );
        assert_eq!(result, "{{unknown.var}} and Jordan");
    }

    fn make_quote_email_data() -> TemplateVariableData {
        use crate::crud::template::{DealVariableData, QuoteVariableData};
        let mut data = make_full_data();
        data.user.name = Some("Alice Mae Johnson".to_string());
        if let Some(customer) = data.customer.as_mut() {
            customer.email = Some("jordan@example.com".to_string());
            customer.phone = Some("(317) 555-0100".to_string());
        }
        data.deal = Some(DealVariableData {
            name: Some("Kitchen remodel".to_string()),
            amount: Some(4500.0),
        });
        data.quote = Some(QuoteVariableData {
            name: Some("Smith Kitchen".to_string()),
            total: Some(12345.678),
            color: Some("Quartz Calacatta Laza".to_string()),
            link: Some("https://example.granite-manager.com/quote-portal/abc?e=def".to_string()),
            expiration: Some("2026-11-05".to_string()),
        });
        data
    }

    #[test]
    fn replaces_last_names_and_customer_contact_details() {
        let result = replace_template_variables(
            "{{customer.last_name}} | {{user.last_name}} | {{customer.email}} | {{customer.phone}}",
            &make_quote_email_data(),
        );
        assert_eq!(
            result,
            "Smith | Mae Johnson | jordan@example.com | (317) 555-0100"
        );
    }

    #[test]
    fn leaves_last_name_for_a_one_word_name() {
        let mut data = make_full_data();
        data.user.name = Some("Cher".to_string());
        let result = replace_template_variables("{{user.last_name}}", &data);
        assert_eq!(result, "{{user.last_name}}");
    }

    #[test]
    fn replaces_deal_variables_when_sent_from_a_deal() {
        let result = replace_template_variables(
            "{{deal.name}} for {{Deal.Amount}}",
            &make_quote_email_data(),
        );
        assert_eq!(result, "Kitchen remodel for $4,500.00");
    }

    #[test]
    fn leaves_deal_and_quote_variables_without_a_deal_or_quote() {
        let template = "{{deal.name}} {{deal.amount}} {{quote.total}} {{quote.link}}";
        assert_eq!(
            replace_template_variables(template, &make_full_data()),
            template
        );
    }

    #[test]
    fn replaces_quote_variables_in_a_quote_email() {
        let result = replace_template_variables(
            "{{quote.name}}: {{quote.color}} for {{quote.total}}, valid until {{quote.expiration}}. {{quote.link}}",
            &make_quote_email_data(),
        );
        assert_eq!(
            result,
            "Smith Kitchen: Quartz Calacatta Laza for $12,345.68, valid until November 5, 2026. https://example.granite-manager.com/quote-portal/abc?e=def"
        );
    }

    #[test]
    fn leaves_quote_total_when_it_is_not_a_number() {
        let mut data = make_quote_email_data();
        if let Some(quote) = data.quote.as_mut() {
            quote.total = Some(f64::NAN);
        }
        assert_eq!(
            replace_template_variables("{{quote.total}}", &data),
            "{{quote.total}}"
        );
    }

    #[test]
    fn formats_dollars() {
        assert_eq!(format_usd(0.0), "$0.00");
        assert_eq!(format_usd(999.999), "$1,000.00");
        assert_eq!(format_usd(1_234_567.5), "$1,234,567.50");
        assert_eq!(format_usd(-12.3), "-$12.30");
    }

    #[test]
    fn lists_placeholder_keys() {
        assert_eq!(
            placeholder_keys("Hi {{ customer.name }}, {{quote.total}} {{ broken"),
            vec!["customer.name".to_string(), "quote.total".to_string()]
        );
    }
}
