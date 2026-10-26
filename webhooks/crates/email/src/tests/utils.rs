pub use webhooks_core::tests::utils::*;

use crate::amazonses::unprocessed::UnprocessedEmailReporter;
use axum_test::TestServer;
use sqlx::MySqlPool;
use std::sync::{Arc, Mutex};

/// This crate's routes on `pool`.
pub fn new_test_app(pool: MySqlPool) -> TestServer {
    TestServer::builder().build(crate::router().with_state(pool))
}

/// Records `(bucket, key, error_kind)` for each unprocessed-email report
/// instead of sending it to `PostHog`.
type UnprocessedReport = (String, String, &'static str);

#[derive(Clone, Default)]
pub struct MockUnprocessedReporter {
    reports: Arc<Mutex<Vec<UnprocessedReport>>>,
}

impl MockUnprocessedReporter {
    pub fn reports(&self) -> Vec<UnprocessedReport> {
        self.reports.lock().unwrap().clone()
    }
}

impl UnprocessedEmailReporter for MockUnprocessedReporter {
    async fn report(&self, bucket: &str, key: &str, error_kind: &'static str) {
        self.reports
            .lock()
            .unwrap()
            .push((bucket.to_string(), key.to_string(), error_kind));
    }
}
