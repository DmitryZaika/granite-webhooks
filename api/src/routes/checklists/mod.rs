//! Post-installation checklists installers fill in and customers sign.
//!
//! - `GET /v1/checklists` → the company's checklists, newest first
//! - `POST /v1/checklists` → an installer submits one
//! - `GET /v1/checklists/{id}/pdf` → one as a PDF
//! - `GET /v1/installer/context` → the installer portal's company and logo
//!
//! Replaces the loaders of the Remix `employee.checklists.tsx` and
//! `installers.$companyId.checklist.tsx` routes and `api.checklist-pdf.$id.ts`.

mod handlers;
mod pdf;
pub mod queries;
pub mod schemas;

use crate::state::AppState;
use utoipa_axum::{router::OpenApiRouter, routes};

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(handlers::list, handlers::create))
        .routes(routes!(pdf::pdf))
        .routes(routes!(handlers::installer_context))
}
