//! Post-installation checklists installers fill in and customers sign.
//!
//! - `GET /v1/checklists` → the company's checklists, newest first
//!
//! Replaces the loader of the Remix `employee.checklists.tsx` route. The
//! checklist form (`installers.$companyId.checklist.tsx`) and the PDF
//! (`api.checklist-pdf.$id.ts`) are still on Remix.

mod handlers;
pub mod queries;
pub mod schemas;

use crate::state::AppState;
use utoipa_axum::{router::OpenApiRouter, routes};

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new().routes(routes!(handlers::list, handlers::create))
}
