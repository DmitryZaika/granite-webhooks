//! Contact pressure: how much we have contacted the customer, on which
//! channels, what they said back, and the holds and facts that shape the
//! next follow-up. Port of `smsFlowAiContactPressure.ts`.

pub mod build;
pub mod channels;
pub mod copy;
pub mod format;
pub mod repair;
pub mod state;
pub mod timeline;
pub mod types;
pub mod validate;

pub use types::*;
