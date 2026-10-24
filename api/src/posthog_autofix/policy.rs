//! The intake's decisions as plain functions: which issues may start a run,
//! how many runs may start now, and when to retry.

use super::config::Config;
use super::model::{SkipReason, Source};
use chrono::{DateTime, Duration, NaiveDateTime, Utc};

/// A settled issue, as the intake read it from PostHog.
#[derive(Debug, Clone, Copy)]
pub struct Candidate<'a> {
    /// The issue's PostHog status now: `active`, `resolved`, `suppressed`...
    pub issue_status: &'a str,
    pub source: Source,
    pub exception_type: &'a str,
    pub message: &'a str,
    /// The browser's `$host` (web events only).
    pub host: Option<&'a str>,
    /// Events found for the issue.
    pub occurrences: u64,
}

const PRODUCTION_DOMAIN: &str = "granite-manager.com";
const RETRY_BASE_MINUTES: i64 = 5;
const RETRY_CAP_MINUTES: i64 = 4 * 60;

/// Which code raised the error, from an event's `distinct_id` and `$lib`.
pub fn source_of(distinct_id: Option<&str>, lib: Option<&str>) -> Source {
    match (distinct_id, lib) {
        (Some("granite-api"), _) => Source::Api,
        (Some("server-webhooks"), _) => Source::Webhooks,
        (Some("server-time-triggered"), _) => Source::TimeTriggered,
        (_, Some("web")) => Source::Web,
        (_, Some("posthog-node")) => Source::LegacyNode,
        _ => Source::Unknown,
    }
}

/// `Ok` when the issue may start a run, else why it is skipped.
pub fn eligibility(candidate: &Candidate<'_>, exclude: &[String]) -> Result<(), SkipReason> {
    match candidate.issue_status {
        "suppressed" => return Err(SkipReason::Suppressed),
        "active" => {}
        _ => return Err(SkipReason::Resolved),
    }
    if candidate.occurrences == 0 {
        return Err(SkipReason::NoEvents);
    }
    if candidate.source == Source::LegacyNode {
        return Err(SkipReason::LegacyRuntime);
    }
    if candidate.source == Source::Web && candidate.host.is_some_and(is_non_production_host) {
        return Err(SkipReason::NonProduction);
    }
    if is_excluded(candidate, exclude) {
        return Err(SkipReason::Excluded);
    }
    Ok(())
}

/// Localhost, IPs and `.local`/`.test` hosts all fail the domain check.
/// An empty host counts as missing.
fn is_non_production_host(host: &str) -> bool {
    let host = host.trim().to_ascii_lowercase();
    if host.is_empty() {
        return false;
    }
    let name = host
        .split_once(':')
        .map_or(host.as_str(), |(name, _port)| name);
    let subdomain_suffix = format!(".{PRODUCTION_DOMAIN}");
    name != PRODUCTION_DOMAIN && !name.ends_with(&subdomain_suffix)
}

fn is_excluded(candidate: &Candidate<'_>, exclude: &[String]) -> bool {
    let text = format!("{}: {}", candidate.exception_type, candidate.message).to_lowercase();
    exclude
        .iter()
        .filter(|pattern| !pattern.is_empty())
        .any(|pattern| text.contains(&pattern.to_lowercase()))
}

/// How many runs may be dispatched now.
pub fn dispatch_slots(in_flight: u32, dispatched_last_day: u32, config: &Config) -> u32 {
    config
        .max_concurrent
        .saturating_sub(in_flight)
        .min(config.max_per_day.saturating_sub(dispatched_last_day))
}

/// How long a queued row waits after its `attempts`-th failed dispatch.
pub fn retry_delay(attempts: u8) -> Duration {
    let Some(exponent) = attempts.checked_sub(1) else {
        return Duration::zero();
    };
    let minutes = RETRY_BASE_MINUTES
        .saturating_mul(3_i64.saturating_pow(u32::from(exponent)))
        .min(RETRY_CAP_MINUTES);
    Duration::minutes(minutes)
}

/// A fixed issue that PostHog reopened: resolved before, active now.
pub fn is_regression(previous: Option<&str>, current: &str) -> bool {
    previous == Some("resolved") && current == "active"
}

/// The start of the `minutes`-long window holding `now` (UTC, aligned to the
/// epoch), the key for once-per-window claims.
pub fn window_start(now: DateTime<Utc>, minutes: i64) -> NaiveDateTime {
    let seconds = minutes.max(1).saturating_mul(60);
    let start = now.timestamp().div_euclid(seconds) * seconds;
    DateTime::from_timestamp(start, 0).map_or_else(|| now.naive_utc(), |time| time.naive_utc())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn candidate() -> Candidate<'static> {
        Candidate {
            issue_status: "active",
            source: Source::Web,
            exception_type: "TypeError",
            message: "Failed to fetch",
            host: Some("app.granite-manager.com"),
            occurrences: 3,
        }
    }

    fn check(candidate: &Candidate<'_>) -> Result<(), SkipReason> {
        eligibility(candidate, &[])
    }

    #[test]
    fn sources_come_from_distinct_id_then_lib() {
        let cases = [
            (Some("granite-api"), Some("web"), Source::Api),
            (Some("server-webhooks"), None, Source::Webhooks),
            (Some("server-time-triggered"), None, Source::TimeTriggered),
            (
                Some("server-time-triggered"),
                Some("web"),
                Source::TimeTriggered,
            ),
            (Some("user-1"), Some("web"), Source::Web),
            (None, Some("posthog-node"), Source::LegacyNode),
            (Some("user-1"), Some("other"), Source::Unknown),
            (None, None, Source::Unknown),
        ];
        for (distinct_id, lib, expected) in cases {
            assert_eq!(
                source_of(distinct_id, lib),
                expected,
                "{distinct_id:?} {lib:?}"
            );
        }
    }

    #[test]
    fn a_normal_production_issue_is_eligible() {
        assert_eq!(check(&candidate()), Ok(()));
        let api = Candidate {
            source: Source::Api,
            host: None,
            ..candidate()
        };
        assert_eq!(check(&api), Ok(()));
        let no_host = Candidate {
            host: None,
            ..candidate()
        };
        assert_eq!(check(&no_host), Ok(()));
    }

    #[test]
    fn status_decides_before_anything_else() {
        let worst = Candidate {
            source: Source::LegacyNode,
            occurrences: 0,
            ..candidate()
        };
        let suppressed = Candidate {
            issue_status: "suppressed",
            ..worst
        };
        let resolved = Candidate {
            issue_status: "resolved",
            ..worst
        };
        let pending = Candidate {
            issue_status: "pending_release",
            ..worst
        };
        assert_eq!(check(&suppressed), Err(SkipReason::Suppressed));
        assert_eq!(check(&resolved), Err(SkipReason::Resolved));
        assert_eq!(check(&pending), Err(SkipReason::Resolved));
    }

    #[test]
    fn eligibility_checks_run_in_order() {
        let exclude = vec!["Failed To Fetch".to_owned()];
        let all_bad = Candidate {
            source: Source::LegacyNode,
            occurrences: 0,
            host: Some("localhost"),
            ..candidate()
        };
        assert_eq!(eligibility(&all_bad, &exclude), Err(SkipReason::NoEvents));
        let legacy = Candidate {
            occurrences: 1,
            ..all_bad
        };
        assert_eq!(
            eligibility(&legacy, &exclude),
            Err(SkipReason::LegacyRuntime)
        );
        let local = Candidate {
            source: Source::Web,
            ..legacy
        };
        assert_eq!(
            eligibility(&local, &exclude),
            Err(SkipReason::NonProduction)
        );
        let excluded = Candidate {
            host: None,
            ..local
        };
        assert_eq!(eligibility(&excluded, &exclude), Err(SkipReason::Excluded));
        assert_eq!(eligibility(&excluded, &[]), Ok(()));
    }

    #[test]
    fn only_web_issues_are_checked_for_their_host() {
        let api = Candidate {
            source: Source::Api,
            host: Some("localhost"),
            ..candidate()
        };
        assert_eq!(check(&api), Ok(()));
    }

    #[test]
    fn hosts_outside_production_are_skipped() {
        for host in [
            "localhost",
            "localhost:3000",
            "127.0.0.1",
            "[::1]",
            "dev.local",
            "app.test",
            "example.com",
            "evilgranite-manager.com",
            "granite-manager.com.evil.io",
        ] {
            let web = Candidate {
                host: Some(host),
                ..candidate()
            };
            assert_eq!(check(&web), Err(SkipReason::NonProduction), "{host}");
        }
        for host in [
            "granite-manager.com",
            "Acme.Granite-Manager.com",
            "a.granite-manager.com:443",
        ] {
            let web = Candidate {
                host: Some(host),
                ..candidate()
            };
            assert_eq!(check(&web), Ok(()), "{host}");
        }
    }

    #[test]
    fn exclude_matches_type_and_message_case_insensitively() {
        let exclude = vec![String::new(), "typeerror: failed to".to_owned()];
        assert_eq!(
            eligibility(&candidate(), &exclude),
            Err(SkipReason::Excluded)
        );
        assert_eq!(eligibility(&candidate(), &[String::new()]), Ok(()));
    }

    #[test]
    fn slots_are_the_tighter_cap_and_never_negative() {
        let config = Config {
            max_concurrent: 3,
            max_per_day: 10,
            ..Config::default()
        };
        assert_eq!(dispatch_slots(0, 0, &config), 3);
        assert_eq!(dispatch_slots(2, 0, &config), 1);
        assert_eq!(dispatch_slots(0, 9, &config), 1);
        assert_eq!(dispatch_slots(5, 0, &config), 0);
        assert_eq!(dispatch_slots(0, 12, &config), 0);
        assert_eq!(dispatch_slots(5, 12, &config), 0);
    }

    #[test]
    fn retries_back_off_threefold_up_to_four_hours() {
        let minutes = |attempts| retry_delay(attempts).num_minutes();
        assert_eq!(minutes(0), 0);
        assert_eq!(minutes(1), 5);
        assert_eq!(minutes(2), 15);
        assert_eq!(minutes(3), 45);
        assert_eq!(minutes(4), 135);
        assert_eq!(minutes(5), 240);
        assert_eq!(minutes(u8::MAX), 240);
    }

    #[test]
    fn a_regression_is_resolved_then_active() {
        assert!(is_regression(Some("resolved"), "active"));
        assert!(!is_regression(Some("active"), "active"));
        assert!(!is_regression(Some("resolved"), "resolved"));
        assert!(!is_regression(None, "active"));
    }

    #[test]
    fn windows_align_to_the_epoch() {
        let at = |h, m, s| Utc.with_ymd_and_hms(2026, 10, 20, h, m, s).unwrap();
        let start = |time, minutes| window_start(time, minutes).to_string();
        assert_eq!(start(at(10, 7, 31), 5), "2026-10-20 10:05:00");
        assert_eq!(start(at(10, 5, 0), 5), "2026-10-20 10:05:00");
        assert_eq!(start(at(10, 59, 59), 60), "2026-10-20 10:00:00");
        assert_eq!(start(at(10, 7, 31), 1), "2026-10-20 10:07:00");
        assert_eq!(start(at(10, 7, 31), 0), "2026-10-20 10:07:00");
        assert_eq!(start(at(10, 7, 31), -3), "2026-10-20 10:07:00");
    }
}
