//! Sales questionnaires: discovery questions reps answer per deal.
//!
//! - `GET    /v1/questionnaires`                                 → the caller's own questionnaires
//! - `POST   /v1/questionnaires`                                 → create one with its questions
//! - `PATCH  /v1/questionnaires/{id}`, `DELETE`                  → rename, delete
//! - `POST   /v1/questionnaires/{id}/questions`                  → add a question
//! - `PATCH  /v1/questionnaires/{id}/questions/{question_id}`, `DELETE`
//! - `GET    /v1/deals/{id}/questionnaires`                      → company + own questionnaires with the deal's answers
//! - `PUT    /v1/deals/{id}/questionnaire-answers`               → save answers (optionally as a note)
//! - `GET/PUT /v1/me/default-questionnaire`                       → the one opened by default
//!
//! Replaces `app/lib/dealQuestionnaires.server.ts` in the Remix app.

mod handlers;
pub mod schemas;

use crate::state::AppState;
use utoipa_axum::{router::OpenApiRouter, routes};

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(handlers::list_own, handlers::create))
        .routes(routes!(handlers::rename, handlers::delete))
        .routes(routes!(handlers::add_question))
        .routes(routes!(
            handlers::update_question,
            handlers::delete_question
        ))
        .routes(routes!(handlers::deal_questionnaires))
        .routes(routes!(handlers::save_answers))
        .routes(routes!(handlers::get_default, handlers::set_default))
}
