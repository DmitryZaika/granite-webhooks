use serde::{Deserialize, Serialize};

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

    #[serde(rename = "Remodal")]
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

#[derive(Debug, Serialize, Deserialize)]
pub struct FaceBookContactForm {
    #[serde(rename = "1.data.full_name")]
    pub name: String,

    #[serde(rename = "1.data.phone_number")]
    pub phone: String,

    #[serde(rename = "1.data.would_you_like_us_to_remove_and_dispose_of_your_old_countertops?")]
    pub remove_and_dispose: Option<String>,

    #[serde(rename = "1.data.email")]
    pub email: Option<String>,

    #[serde(rename = "1.data.city")]
    pub city: Option<String>,

    #[serde(rename = "1.data.zip_code")]
    pub postal_code: Option<String>,

    #[serde(rename = "1.data.what_other_information_you'd_like_to_share?_(e.g_sqft,_state_etc.)")]
    pub details: Option<String>,

    #[serde(rename = "1.campaignName")]
    pub compaign_name: Option<String>,

    #[serde(rename = "1.adsetName")]
    pub adset_name: Option<String>,

    #[serde(rename = "1.adName")]
    pub ad_name: Option<String>,
}
