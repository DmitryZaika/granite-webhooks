//! Telnyx: its call and messaging webhooks, dialing and recordings, 10DLC
//! (api-phone).
//!
//! One file's tests: `cargo test -p api --test telnyx <file>::`.

#[path = "../common/mod.rs"]
mod common;
#[path = "../phone_common/mod.rs"]
mod phone_common;

mod phone_telnyx_dial;
mod phone_telnyx_inbound;
mod phone_telnyx_outbound;
mod phone_telnyx_recording;
mod phone_telnyx_via_cell;
mod telnyx_10dlc;
mod telnyx_calls;
mod telnyx_sms;
