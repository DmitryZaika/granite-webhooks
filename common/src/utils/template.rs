use crate::crud::template::TemplateVariableData;
use chrono::Local;

pub const VARIABLE_KEYS: &[&str] = &[
    "user.name",
    "user.first_name",
    "customer.name",
    "customer.first_name",
    "current_date",
    "company.name",
    "company.address",
    "customer.address",
    "user.phone_number",
    "user.email",
];

fn get_first_name(full_name: &str) -> String {
    full_name.split(' ').next().unwrap_or("").to_string()
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

fn get_variable_value(key: &str, data: &TemplateVariableData) -> Option<String> {
    let value = match key {
        "user.name" => data.user.name.clone(),
        "user.first_name" => data.user.name.as_ref().map(|n| get_first_name(n)),
        "user.email" => data.user.email.clone(),
        "user.phone_number" => data.user.phone_number.clone(),
        "customer.name" => data.customer.as_ref()?.name.clone(),
        "customer.first_name" => {
            let customer = data.customer.as_ref()?;
            customer.name.as_ref().map(|n| get_first_name(n))
        }
        "customer.address" => data.customer.as_ref()?.address.clone(),
        "company.name" => data.company.as_ref()?.name.clone(),
        "company.address" => data.company.as_ref()?.address.clone(),
        "current_date" => Some(format_current_date()),
        _ => None,
    };

    value.filter(|s| !s.is_empty())
}

pub fn replace_template_variables(template: &str, data: &TemplateVariableData) -> String {
    let mut result = template.to_string();

    for &key in VARIABLE_KEYS {
        if let Some(value) = get_variable_value(key, data) {
            result = result.replace(&format!("{{{{{}}}}}", key), &value);
        }
    }

    result
}
