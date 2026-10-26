//! What the engine reads from a customer's history: quotes, materials,
//! timing, budget, measurements. Port of `smsFlowAiFollowupFacts.ts`.

pub mod contact;
pub mod extract;
pub mod manual;
pub mod materials;
pub mod project;
pub mod quotes;
pub mod scope;
pub mod timing;
pub mod types;

pub use types::*;
