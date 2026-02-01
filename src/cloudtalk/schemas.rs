use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
pub struct CloudtalkSMS {
    id: i32,
    sender: String,
    recipient: String,
    text: String,
    agent: String,
}
