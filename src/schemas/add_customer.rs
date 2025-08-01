use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
pub struct WordpressContactForm {
    #[serde(rename = "your-name")]
    pub your_name: String,

    #[serde(rename = "your-email")]
    pub your_email: String,

    #[serde(rename = "mask-712")]
    pub phone: Option<String>,

    #[serde(rename = "your-zip")]
    pub your_zip: String,

    #[serde(rename = "your-message")]
    your_message: Option<String>,

    #[serde(rename = "file-506")]
    file: Option<String>,

    #[serde(rename = "menu-395")]
    checked_competitor: Option<String>,

    #[serde(rename = "Wishlist")]
    wishlist: Option<String>,
}
