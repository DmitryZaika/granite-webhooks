use crate::crud::template::TemplateVariableData;
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
        ("company.name", company.and_then(|c| c.name.clone())),
        ("company.address", company.and_then(|c| c.address.clone())),
        ("current_date", Some(format_current_date())),
    ]
    .into_iter()
    .filter_map(|(k, v)| v.filter(|s| !s.is_empty()).map(|val| (k, val)))
    .collect()
}

pub fn replace_template_variables(template: &str, data: &TemplateVariableData) -> String {
    let mut result = template.to_string();

    for (key, value) in &build_variable_map(data) {
        result = result.replace(&format!("{{{{{}}}}}", key), value);
    }

    result
}
