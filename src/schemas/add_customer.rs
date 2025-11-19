use serde::{Deserialize, Serialize};
use std::fmt;
use std::fmt::Write as _;
use serde::de::Deserializer;

fn clean_phone<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: Deserializer<'de>,
{
    let opt = Option::<String>::deserialize(deserializer)?;
    Ok(opt.map(|s| {
        let digits: String = s.chars()
            .filter(|c| c.is_ascii_digit())
            .collect();
        
        let digits = if digits.starts_with('1') && digits.len() == 11 {
            &digits[1..]
        } else {
            &digits
        };
        
        if digits.len() == 10 {
            format!("{}-{}-{}", &digits[0..3], &digits[3..6], &digits[6..10])
        } else {
            digits.to_string()
        }
    }))
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct WordpressContactForm {
    pub name: String,

    #[serde(rename = "Email")]
    pub email: Option<String>,

    #[serde(deserialize_with = "clean_phone", rename = "Phone")]
    pub phone: Option<String>,

    #[serde(rename = "Zip")]
    pub postal_code: Option<String>,

    #[serde(rename = "Address")]
    pub address: Option<String>,

    #[serde(rename = "Remodel")]
    pub remodal_type: Option<String>,

    #[serde(rename = "project")]
    pub project_size: Option<String>,

    #[serde(rename = "Contacted")]
    pub contact_time: Option<String>,

    #[serde(rename = "Remove")]
    pub remove_and_dispose: Option<String>,

    #[serde(rename = "Improve")]
    pub improve_offer: Option<String>,

    #[serde(rename = "Sink")]
    pub sink: Option<String>,

    #[serde(rename = "Backsplash")]
    pub backsplash: Option<String>,

    #[serde(rename = "Stove")]
    pub kitchen_stove: Option<String>,

    #[serde(rename = "Message")]
    pub your_message: Option<String>,

    #[serde(rename = "File")]
    pub attached_file: Option<String>,
}

impl fmt::Display for WordpressContactForm {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let message = format!(
            "Name: {}\n\
           Phone: {}\n\
           Email: {}\n\
           Address: {}\n\
           Zip: {}\n\
           Remodeling Type: {}\n\
           Project Size: {}\n\
           Contacted: {}\n\
           Remove and Dispose: {}\n\
           Improve Offer: {}\n\
           Sink: {}\n\
           Backsplash: {}\n\
           Stove: {}\n\
           Your Message: {}\n\
           Attached File: {}",
            self.name,
            self.phone.as_deref().unwrap_or("N/A"),
            self.email.as_deref().unwrap_or("N/A"),
            self.address.as_deref().unwrap_or("N/A"),
            self.postal_code.as_deref().unwrap_or("N/A"),
            self.remodal_type.as_deref().unwrap_or("N/A"),
            self.project_size.as_deref().unwrap_or("N/A"),
            self.contact_time.as_deref().unwrap_or("N/A"),
            self.remove_and_dispose.as_deref().unwrap_or("N/A"),
            self.improve_offer.as_deref().unwrap_or("N/A"),
            self.sink.as_deref().unwrap_or("N/A"),
            self.backsplash.as_deref().unwrap_or("N/A"),
            self.kitchen_stove.as_deref().unwrap_or("N/A"),
            self.your_message.as_deref().unwrap_or("N/A"),
            self.attached_file.as_deref().unwrap_or("N/A")
        );
        write!(f, "{message}")
    }
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct FaceBookContactForm {
    pub name: String,

    #[serde(deserialize_with = "clean_phone")]
    pub phone: Option<String>,

    #[serde(rename = "remove")]
    pub remove_and_dispose: Option<String>,

    #[serde(rename = "email")]
    pub email: Option<String>,

    #[serde(rename = "city")]
    pub city: Option<String>,

    #[serde(rename = "zip")]
    pub postal_code: Option<String>,

    #[serde(rename = "share")]
    pub details: Option<String>,

    #[serde(rename = "campaign")]
    pub compaign_name: Option<String>,

    #[serde(rename = "adsetname")]
    pub adset_name: Option<String>,

    #[serde(rename = "adname")]
    pub ad_name: Option<String>,
}

impl fmt::Display for FaceBookContactForm {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let message = format!(
            "Name: {}\n\
               Phone: {}\n\
               Remove and Dispose: {}\n\
               Email: {}\n\
               City: {}\n\
               Zip: {}\n\
               Details: {}\n\
               Campaign: {}\n\
               Adset: {}\n\
               Ad: {}",
            self.name,
            self.phone.as_deref().unwrap_or("N/A"),
            self.remove_and_dispose.as_deref().unwrap_or("N/A"),
            self.email.as_deref().unwrap_or("N/A"),
            self.city.as_deref().unwrap_or("N/A"),
            self.postal_code.as_deref().unwrap_or("N/A"),
            self.details.as_deref().unwrap_or("N/A"),
            self.compaign_name.as_deref().unwrap_or("N/A"),
            self.adset_name.as_deref().unwrap_or("N/A"),
            self.ad_name.as_deref().unwrap_or("N/A")
        );
        write!(f, "{message}")
    }
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct NewLeadForm {
    pub name: String,

    pub email: Option<String>,

    #[serde(default, deserialize_with = "clean_phone")]
    pub phone: Option<String>,

    pub postal_code: Option<String>,

    pub address: Option<String>,

    pub city: Option<String>,

    #[serde(rename = "remodel_type")]
    pub remodal_type: Option<String>,

    pub project_size: Option<String>,

    pub contact_time: Option<String>,

    #[serde(rename = "start_date")]
    pub when_start: Option<String>,

    #[serde(rename = "tear_out")]
    pub remove_and_dispose: Option<String>,

    pub improve_offer: Option<String>,

    pub sink: Option<String>,

    #[serde(rename = "stove_type")]
    pub kitchen_stove: Option<String>,

    pub backsplash: Option<String>,

    pub your_message: Option<String>,

    pub details: Option<String>,

    pub ad_name: Option<String>,

    pub adset_name: Option<String>,

    #[serde(rename = "campaign_name")]
    pub compaign_name: Option<String>,

    #[serde(rename = "file")]
    pub attached_file: Option<String>,

    pub referral_source: Option<String>,
}

impl fmt::Display for NewLeadForm {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let mut message = format!("Name: {}\n", self.name);

        if let Some(phone) = &self.phone {
            writeln!(message, "Phone: {phone}").unwrap();
        }
        if let Some(email) = &self.email {
            writeln!(message, "Email: {email}").unwrap();
        }
        if let Some(postal_code) = &self.postal_code {
            writeln!(message, "Postal Code: {postal_code}").unwrap();
        }
        if let Some(address) = &self.address {
            writeln!(message, "Address: {address}").unwrap();
        }
        if let Some(city) = &self.city {
            writeln!(message, "City: {city}").unwrap();
        }
        if let Some(remodal_type) = &self.remodal_type {
            writeln!(message, "Remodel Type: {remodal_type}").unwrap();
        }
        if let Some(project_size) = &self.project_size {
            writeln!(message, "Project Size: {project_size}").unwrap();
        }
        if let Some(contact_time) = &self.contact_time {
            writeln!(message, "Best Time to Contact: {contact_time}").unwrap();
        }
        if let Some(when_start) = &self.when_start {
            writeln!(message, "Start Date: {when_start}").unwrap();
        }
        if let Some(remove_and_dispose) = &self.remove_and_dispose {
            writeln!(message, "Tear Out: {remove_and_dispose}").unwrap();
        }
        if let Some(improve_offer) = &self.improve_offer {
            writeln!(message, "Improve Offer: {improve_offer}").unwrap();
        }
        if let Some(sink) = &self.sink {
            writeln!(message, "Sink: {sink}").unwrap();
        }
        if let Some(kitchen_stove) = &self.kitchen_stove {
            writeln!(message, "Stove Type: {kitchen_stove}").unwrap();
        }
        if let Some(backsplash) = &self.backsplash {
            writeln!(message, "Backsplash: {backsplash}").unwrap();
        }
        if let Some(your_message) = &self.your_message {
            writeln!(message, "Message: {your_message}").unwrap();
        }
        if let Some(details) = &self.details {
            writeln!(message, "Details: {details}").unwrap();
        }
        if let Some(ad_name) = &self.ad_name {
            writeln!(message, "Ad Name: {ad_name}").unwrap();
        }
        if let Some(adset_name) = &self.adset_name {
            writeln!(message, "Adset Name: {adset_name}").unwrap();
        }
        if let Some(compaign_name) = &self.compaign_name {
            writeln!(message, "Campaign Name: {compaign_name}").unwrap();
        }
        if let Some(attached_file) = &self.attached_file {
            writeln!(message, "File: {attached_file}").unwrap();
        }

        write!(f, "{message}")
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_clean_phone_from_list_parens_812() {
        let data = json!({ "name": "Test", "phone": "(812) 374-4195" });
        let lead: NewLeadForm = serde_json::from_value(data).unwrap();
        assert_eq!(lead.phone.unwrap(), "812-374-4195");
    }

    #[test]
    fn test_clean_phone_from_list_e164_812() {
        let data = json!({ "name": "Test", "phone": "+18125819268" });
        let lead: NewLeadForm = serde_json::from_value(data).unwrap();
        assert_eq!(lead.phone.unwrap(), "812-581-9268");
    }

    #[test]
    fn test_clean_phone_from_list_e164_317_a() {
        let data = json!({ "name": "Test", "phone": "+13174417059" });
        let lead: NewLeadForm = serde_json::from_value(data).unwrap();
        assert_eq!(lead.phone.unwrap(), "317-441-7059");
    }

    #[test]
    fn test_clean_phone_from_list_e164_317_b() {
        let data = json!({ "name": "Test", "phone": "+13179995973" });
        let lead: NewLeadForm = serde_json::from_value(data).unwrap();
        assert_eq!(lead.phone.unwrap(), "317-999-5973");
    }

    #[test]
    fn test_clean_phone_from_list_e164_806() {
        let data = json!({ "name": "Test", "phone": "+18064016803" });
        let lead: NewLeadForm = serde_json::from_value(data).unwrap();
        assert_eq!(lead.phone.unwrap(), "806-401-6803");
    }

    #[test]
    fn test_clean_phone_from_list_e164_317_c() {
        let data = json!({ "name": "Test", "phone": "+13174072028" });
        let lead: NewLeadForm = serde_json::from_value(data).unwrap();
        assert_eq!(lead.phone.unwrap(), "317-407-2028");
    }

    #[test]
    fn test_clean_phone_from_list_already_normalized() {
        let data = json!({ "name": "Test", "phone": "614-469-1230" });
        let lead: NewLeadForm = serde_json::from_value(data).unwrap();
        assert_eq!(lead.phone.unwrap(), "614-469-1230");
    }

    #[test]
    fn test_clean_phone_from_list_no_delimiters_other() {
        let data = json!({ "name": "Test", "phone": "7657643147" });
        let lead: NewLeadForm = serde_json::from_value(data).unwrap();
        assert_eq!(lead.phone.unwrap(), "765-764-3147");
    }

    #[test]
    fn test_clean_phone_from_list_parens_317_a() {
        let data = json!({ "name": "Test", "phone": "(317) 522-8701" });
        let lead: NewLeadForm = serde_json::from_value(data).unwrap();
        assert_eq!(lead.phone.unwrap(), "317-522-8701");
    }

    #[test]
    fn test_clean_phone_from_list_plain_574() {
        let data = json!({ "name": "Test", "phone": "574-612-5902" });
        let lead: NewLeadForm = serde_json::from_value(data).unwrap();
        assert_eq!(lead.phone.unwrap(), "574-612-5902");
    }

    #[test]
    fn test_clean_phone_from_list_parens_317_b() {
        let data = json!({ "name": "Test", "phone": "(317) 402-1551" });
        let lead: NewLeadForm = serde_json::from_value(data).unwrap();
        assert_eq!(lead.phone.unwrap(), "317-402-1551");
    }

    #[test]
    fn test_clean_phone_from_list_dup_example() {
        let data = json!({ "name": "Test", "phone": "317-750-6474" });
        let lead: NewLeadForm = serde_json::from_value(data).unwrap();
        assert_eq!(lead.phone.unwrap(), "317-750-6474");
    }

    #[test]
    fn test_wordpress_contact_form_deserialize() {
        let data = json!({
            "name": "John Doe",
            "Email": "john@example.com",
            "Phone": "317-750-6474",
            "Zip": "46201",
            "Address": "123 Main St",
            "Remodel": "Kitchen",
            "project": "Medium",
            "Contacted": "Evening",
            "Remove": "Yes",
            "Improve": "New counters",
            "Sink": "Undermount",
            "Backsplash": "Tile",
            "Stove": "Gas",
            "Message": "Looking for a quote",
            "File": "https://www.google.com"
        });

        let form: WordpressContactForm = serde_json::from_value(data).unwrap();

        assert_eq!(form.name, "John Doe");
        assert_eq!(form.email.clone().unwrap(), "john@example.com");
        assert_eq!(form.phone, Some("317-750-6474".to_string()));
        assert_eq!(form.postal_code.clone().unwrap(), "46201");
        assert_eq!(form.address.clone().unwrap(), "123 Main St");
        assert_eq!(form.remodal_type.clone().unwrap(), "Kitchen");
        assert_eq!(form.project_size.clone().unwrap(), "Medium");
        assert_eq!(form.contact_time.clone().unwrap(), "Evening");
        assert_eq!(form.remove_and_dispose.clone().unwrap(), "Yes");
        assert_eq!(form.improve_offer.clone().unwrap(), "New counters");
        assert_eq!(form.sink.clone().unwrap(), "Undermount");
        assert_eq!(form.backsplash.clone().unwrap(), "Tile");
        assert_eq!(form.kitchen_stove.clone().unwrap(), "Gas");
        assert_eq!(form.your_message.clone().unwrap(), "Looking for a quote");
        assert_eq!(form.attached_file.clone().unwrap(), "https://www.google.com");

        let text = form.to_string();
        assert!(text.contains("Name: John Doe"));
        assert!(text.contains("Phone: 317-750-6474"));
        assert!(text.contains("Email: john@example.com"));
        assert!(text.contains("Address: 123 Main St"));
        assert!(text.contains("Zip: 46201"));
        assert!(text.contains("Remodeling Type: Kitchen"));
        assert!(text.contains("Project Size: Medium"));
        assert!(text.contains("Contacted: Evening"));
        assert!(text.contains("Remove and Dispose: Yes"));
        assert!(text.contains("Improve Offer: New counters"));
        assert!(text.contains("Sink: Undermount"));
        assert!(text.contains("Backsplash: Tile"));
        assert!(text.contains("Stove: Gas"));
        assert!(text.contains("Your Message: Looking for a quote"));
        assert!(text.contains("Attached File: https://www.google.com"));
    }

    #[test]
    fn test_facebook_contact_form_deserialize() {
        let data = json!({
            "name": "Jane Smith",
            "phone": "812-374-4195",
            "remove": "No",
            "email": "jane@example.com",
            "city": "Columbus",
            "zip": "47201",
            "share": "Full kitchen remodel",
            "campaign": "Fall Promo",
            "adsetname": "Indiana Leads",
            "adname": "Kitchen Ad 1"
        });

        let form: FaceBookContactForm = serde_json::from_value(data).unwrap();

        assert_eq!(form.name, "Jane Smith");
        assert_eq!(form.phone, Some("812-374-4195".to_string()));
        assert_eq!(form.remove_and_dispose.clone().unwrap(), "No");
        assert_eq!(form.email.clone().unwrap(), "jane@example.com");
        assert_eq!(form.city.clone().unwrap(), "Columbus");
        assert_eq!(form.postal_code.clone().unwrap(), "47201");
        assert_eq!(form.details.clone().unwrap(), "Full kitchen remodel");
        assert_eq!(form.compaign_name.clone().unwrap(), "Fall Promo");
        assert_eq!(form.adset_name.clone().unwrap(), "Indiana Leads");
        assert_eq!(form.ad_name.clone().unwrap(), "Kitchen Ad 1");

        let text = form.to_string();
        assert!(text.contains("Name: Jane Smith"));
        assert!(text.contains("Phone: 812-374-4195"));
        assert!(text.contains("Remove and Dispose: No"));
        assert!(text.contains("Email: jane@example.com"));
        assert!(text.contains("City: Columbus"));
        assert!(text.contains("Zip: 47201"));
        assert!(text.contains("Details: Full kitchen remodel"));
        assert!(text.contains("Campaign: Fall Promo"));
        assert!(text.contains("Adset: Indiana Leads"));
        assert!(text.contains("Ad: Kitchen Ad 1"));
    }
}
