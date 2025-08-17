use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
pub struct WordpressContactForm {
<<<<<<< HEAD
    #[serde(rename = "your-name")]
    pub name: String,

    #[serde(rename = "your-email")]
    pub email: Option<String>,
=======
    pub name: String,
    pub email: String,
    message: Option<String>,
>>>>>>> 1155f55723d22b93bb4638a64a54162c7d4d2c31

    #[serde(rename = "mask-277")]
    pub phone: String,

    #[serde(rename = "your-zip")]
<<<<<<< HEAD
    pub postal_code: Option<String>,

    #[serde(rename = "your-address")]
    pub address: Option<String>,
=======
    pub your_zip: Option<String>,
>>>>>>> 1155f55723d22b93bb4638a64a54162c7d4d2c31

    #[serde(rename = "menu-185")]
    remodal_type: Option<String>,

    #[serde(rename = "number-629")]
    project_size: Option<String>,

    #[serde(rename = "contacttime")]
    contact_time: Option<String>,

    #[serde(rename = "menu-186")]
    remove_and_dispose: Option<String>,

    #[serde(rename = "menu-395")]
    improve_offer: Option<String>,

    #[serde(rename = "menu-189")]
    sink: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct FaceBookContactForm {
    #[serde(rename = "your-name")]
    pub name: String,

    #[serde(rename = "your-phone")]
    pub phone: String,

    #[serde(rename = "your-date????")]
    pub when_start: Option<String>,

    #[serde(rename = "your-details???")]
    pub details: Option<String>,

    #[serde(rename = "your-email")]
    pub email: Option<String>,

    #[serde(rename = "your-city????")]
    pub city: Option<String>,

    #[serde(rename = "zip-code????")]
    pub postal_code: Option<String>,

    #[serde(rename = "compaign_name???")]
    pub compaign_name: Option<String>,

    #[serde(rename = "adset_name???")]
    pub adset_name: Option<String>,

    #[serde(rename = "ad_name???")]
    pub ad_name: Option<String>,
}
