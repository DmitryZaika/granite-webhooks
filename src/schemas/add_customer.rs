use serde::{Deserialize, Serialize};
use std::fmt;

#[derive(Debug, Serialize, Deserialize)]
pub struct WordpressContactForm {
    #[serde(rename = "name")]
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
        let message = format!("New lead received.\n\nName: {}\nPhone: {}\nEmail: {}\nPostal Code: {}\nRemove and Dispose: {}\nImprove Offer: {}\nSink: {}\nBacksplash: {}\nKitchen Stove: {}\nYour Message: {}\nAttached File: {}",
            self.name, self.phone, self.email.clone().unwrap_or("N/A".into()),
            self.postal_code.clone().unwrap_or("N/A".into()),
            self.remove_and_dispose.clone().unwrap_or("N/A".into()),
            self.improve_offer.clone().unwrap_or("N/A".into()),
            self.sink.clone().unwrap_or("N/A".into()),
            self.backsplash.clone().unwrap_or("N/A".into()),
            self.kitchen_stove.clone().unwrap_or("N/A".into()),
            self.your_message.clone().unwrap_or("N/A".into()),
            self.attached_file.clone().unwrap_or("N/A".into())
        );
        write!(f, "{message}")
    }
}

#[derive(Debug, Serialize, Deserialize)]
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
        let message = format!("New lead received.\n\nName: {}", self.name,);
        write!(f, "{message}")
    }
}
