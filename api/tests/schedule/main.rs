//! Calendars, events, the schedule board and its notifications (api-schedule).
//!
//! One file's tests: `cargo test -p api --test schedule <file>::`.

#[path = "../common/mod.rs"]
mod common;

mod calendars;
mod estimates;
mod events;
mod schedule;
mod schedule_board;
mod schedule_notifications;
