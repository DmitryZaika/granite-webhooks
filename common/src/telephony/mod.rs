//! Clients of the telephony providers' REST APIs (`CloudTalk`, `RingCentral`),
//! shared by the `webhooks` Lambda (sending) and the API (call history).

pub mod cloudtalk;
pub mod ringcentral;
pub mod ringcentral_auth;
