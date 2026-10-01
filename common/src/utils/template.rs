use crate::crud::template::{InfoVariableData, TemplateVariableData};
use chrono::Local;
use std::collections::HashMap;

fn get_first_name(full_name: &str) -> String {
    full_name.split(' ').next().unwrap_or(full_name).to_string()
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
    if slug.is_empty() {
        None
    } else {
        Some(slug)
    }
}

fn company_subdomain_slug(company: Option<&InfoVariableData>) -> Option<String> {
    let company = company?;
    let raw = company
        .subdomain
        .as_ref()
        .filter(|value| !value.trim().is_empty())
        .or(company.domain.as_ref().filter(|value| !value.trim().is_empty()))?;
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
    let first_name = data.user.name.as_ref().map(|n| get_first_name(n));
    let customer_name = customer.and_then(|c| c.name.as_ref().map(|n| get_first_name(n)));

    [
        ("user.name", data.user.name.clone()),
        ("user.first_name", first_name),
        ("user.email", data.user.email.clone()),
        ("user.phone_number", data.user.phone_number.clone()),
        ("customer.name", customer.and_then(|c| c.name.clone())),
        ("customer.first_name", customer_name),
        ("customer.address", customer.and_then(|c| c.address.clone())),
        (
            "customer.inventory_link",
            customer_inventory_link(company),
        ),
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
    .filter_map(|(k, v)| v.filter(|s| !s.is_empty()).map(|val| (k, val)))
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
            }),
        }
    }

    #[test]
    fn replaces_customer_first_name_in_thank_you_template() {
        let template = r#"<p><span style="color: rgb(0, 0, 0); background-color: transparent;">Hi {{customer.first_name}},</span></p><p><span style="color: rgb(0, 0, 0); background-color: transparent;">Thank you for your request! My name is {{user.first_name}}, and I'm the sales representative who will be helping you with your kitchen project.</span></p><p><br></p><p><span style="color: rgb(0, 0, 0); background-color: transparent;">I’ll be in touch with you shortly to learn more about your project and help you with the next steps.</span></p><p><span style="color: rgb(0, 0, 0); background-color: transparent;">In the meantime, you can browse our live stone inventory here:</span></p><p><u style="color: rgb(17, 85, 204); background-color: transparent;"><a href="{{customer.inventory_link}}" rel="noopener noreferrer" target="_blank">{{customer.inventory_link}}</a></u></p><p><span style="color: rgb(0, 0, 0); background-color: transparent;">We offer a wide selection of natural and man-made stone options for a variety of kitchen styles and projects.</span></p><p><br></p><p><span style="color: rgb(0, 0, 0); background-color: transparent;">I look forward to working with you!</span></p>"#;

        let result = replace_template_variables(template, &make_full_data());

        assert!(result.contains("Hi Jordan,"));
        assert!(result.contains("My name is Alice,"));
        assert!(result.contains(
            "https://example.granite-manager.com/customer/1/stones"
        ));
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
            customer: None,
            company: None,
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
        let result = replace_template_variables(
            "{{customer.inventory_link}}",
            &make_full_data(),
        );
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
}
