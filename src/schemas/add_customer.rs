use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
pub struct WordpressContactForm {
    pub name: String,
    pub email: String,
    message: Option<String>,

    #[serde(rename = "mask-712")]
    pub phone: Option<String>,

    #[serde(rename = "your-zip")]
    pub your_zip: Option<String>,

    #[serde(rename = "file-506")]
    file: Option<String>,

    #[serde(rename = "menu-395")]
    checked_competitor: Option<String>,

    #[serde(rename = "Wishlist")]
    wishlist: Option<String>,
}
