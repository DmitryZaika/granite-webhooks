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

    #[serde(rename = "Phone")]
    pub phone: String,

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
            "New lead received.\n\n\
           Name: {}\n\
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
            self.phone,
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
    #[serde(rename = "name")]
    pub name: String,

    #[serde(rename = "phone")]
    pub phone: String,

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
            "New lead received.\n\n\
               Name: {}\n\
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
            self.phone,
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
        let mut message = format!("New lead received.\n\nName: {}\n", self.name);

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
    fn test_clean_phone_basic() {
        let data = json!({ "name": "Test", "phone": "(317) 555-1234" });
        let lead: NewLeadForm = serde_json::from_value(data).unwrap();
        assert_eq!(lead.phone.unwrap(), "317-555-1234");
    }

    #[test]
    fn test_clean_phone_with_spaces() {
        let data = json!({ "name": "Test", "phone": "317 555  6789" });
        let lead: NewLeadForm = serde_json::from_value(data).unwrap();
        assert_eq!(lead.phone.unwrap(), "317-555-6789");
    }

    #[test]
    fn test_clean_phone_with_plus_sign_and_country_code() {
        let data = json!({ "name": "Test", "phone": "+1 (463) 999-0000" });
        let lead: NewLeadForm = serde_json::from_value(data).unwrap();
        assert_eq!(lead.phone.unwrap(), "463-999-0000");
    }

    #[test]
    fn test_clean_phone_with_symbols_and_text() {
        let data = json!({ "name": "Test", "phone": "Call: 317-555-8888" });
        let lead: NewLeadForm = serde_json::from_value(data).unwrap();
        assert_eq!(lead.phone.unwrap(), "317-555-8888");
    }

    #[test]
    fn test_clean_phone_none() {
        let data = json!({ "name": "Test" });
        let lead: NewLeadForm = serde_json::from_value(data).unwrap();
        assert_eq!(lead.phone, None);
    }
    #[test]
    fn test_clean_phone_with_country_code() {
        let data = json!({ "name": "Test", "phone": "+13175556789" });
        let lead: NewLeadForm = serde_json::from_value(data).unwrap();
        assert_eq!(lead.phone.unwrap(), "317-555-6789");
    }
}