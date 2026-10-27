//! Granite Phone: the app's sign-in, devices, calls, contacts, texts and
//! settings, and their admin pages (api-phone).
//!
//! One file's tests: `cargo test -p api --test phone <file>::`.

#[path = "../common/mod.rs"]
mod common;
#[path = "../phone_common/mod.rs"]
mod phone_common;

mod device_crm_routes;
mod phone_admin;
mod phone_auth;
mod phone_bootstrap;
mod phone_call_settings;
mod phone_calls;
mod phone_connect;
mod phone_contacts;
mod phone_crm_push;
mod phone_devices;
mod phone_email_push;
mod phone_jobs;
mod phone_porting;
mod phone_revocation;
mod phone_settings;
mod phone_texts;
mod phone_web;
mod phone_web_recording;
