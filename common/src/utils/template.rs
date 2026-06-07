use crate::crud::template::TemplateVariableData;
use chrono::Local;

// --- Data Models ---

#[derive(Debug, Clone, Default)]
pub struct CustomerData {
    pub name: Option<String>,
    pub address: Option<String>,
}

pub struct EmailTemplateVariable {
    pub key: &'static str,
    pub label: &'static str,
}

#[derive(Debug, Clone)]
pub struct TemplateValidationResult {
    pub is_valid: bool,
    pub error: Option<String>,
}

// --- Constants ---

pub const EMAIL_TEMPLATE_VARIABLES: &[EmailTemplateVariable] = &[
    EmailTemplateVariable {
        key: "user.name",
        label: "Full name",
    },
    EmailTemplateVariable {
        key: "user.first_name",
        label: "First name",
    },
    EmailTemplateVariable {
        key: "customer.name",
        label: "Customer Full name",
    },
    EmailTemplateVariable {
        key: "customer.first_name",
        label: "Customer First name",
    },
    EmailTemplateVariable {
        key: "current_date",
        label: "Current day and month",
    },
    EmailTemplateVariable {
        key: "company.name",
        label: "Company name",
    },
    EmailTemplateVariable {
        key: "company.address",
        label: "Company address",
    },
    EmailTemplateVariable {
        key: "customer.address",
        label: "Customer address",
    },
    EmailTemplateVariable {
        key: "user.phone_number",
        label: "Employee phone number",
    },
    EmailTemplateVariable {
        key: "user.email",
        label: "Employee email",
    },
];

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

// --- Regex Lazy Initialization ---

fn get_first_name(full_name: &str) -> String {
    full_name.split(' ').next().unwrap_or("").to_string()
}

fn format_current_date() -> String {
    // Formats to "Month Day" (e.g., "October 24")
    Local::now().format("%B %e").to_string().tweak_spaces()
}

// Helper trait to clean up double spaces from %e formatting in Chrono if needed
trait TweakSpaces {
    fn tweak_spaces(self) -> String;
}
impl TweakSpaces for String {
    fn tweak_spaces(self) -> String {
        self.replace("  ", " ").trim().to_string()
    }
}

fn format_variable_for_template(key: &str) -> String {
    format!("{{{{{}}}}}", key)
}

// --- Core API Functions ---

fn get_variable_value(key: &str, data: &TemplateVariableData) -> Option<String> {
    let value = match key {
        "user.name" => data.user.name.clone(),
        "user.first_name" => data.user.name.as_ref().map(|n| get_first_name(n)),
        "user.email" => data.user.email.clone(),
        "user.phone_number" => data.user.phone_number.clone(),
        "customer.name" => data.customer.as_ref()?.name.clone(),
        "customer.first_name" => data
            .customer
            .as_ref()
            .and_then(|c| c.name.as_ref().map(|n| get_first_name(n))),
        "customer.address" => data.customer.as_ref()?.address.clone(),
        "company.name" => data.company.as_ref()?.name.clone(),
        "company.address" => data.company.as_ref()?.address.clone(),
        "current_date" => Some(format_current_date()),
        _ => None,
    };

    // Filters out empty strings to replicate JavaScript's truthy `if (!value)` check
    value.filter(|s| !s.is_empty())
}

pub fn replace_template_variables(template: &str, data: &TemplateVariableData) -> String {
    let mut result = template.to_string();

    for &key in VARIABLE_KEYS {
        if let Some(value) = get_variable_value(key, data) {
            let placeholder = format_variable_for_template(key);
            result = result.replace(&placeholder, &value);
        }
    }

    result
}
