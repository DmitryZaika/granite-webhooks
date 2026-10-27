//! The auto-fix emails, rendered as plain functions. Every value from the
//! ledger or the workflow is escaped: error text is user and third-party input.

use super::config::Config;
use super::model::{
    AgentResult, Conclusion, LastSeen, RunReport, RunView, Source, TestReport, TestStatus,
    Verification,
};
use super::notify::Email;
use crate::html::escape;
use std::collections::HashMap;
use std::fmt::Write;

const NAME_MAX: usize = 120;

/// A run reported its result: subject `[AI fix] <outcome>: <issue name>`.
pub fn run_finished(run: &RunView, report: &RunReport, config: &Config) -> Email {
    let mut html = String::from("<h2>PostHog auto-fix result</h2>");
    html.push_str(&issue_section(run, config));
    let _ = write!(html, "{}", field("Result", &label(report)));
    html.push_str(&pull_request(report));
    if let Some(agent) = &report.agent {
        html.push_str(&agent_section(agent));
    }
    if let Some(notes) = &report.notes {
        html.push_str(&block("Verification notes", notes));
    }
    html.push_str(&run_line(report.github_run_url.as_deref()));
    if let Some(cost) = report.cost_usd {
        html.push_str(&field(
            "Cost",
            &format!("${cost:.2} (Claude Code estimate)"),
        ));
    }
    Email {
        subject: subject(&label(report), run),
        html,
    }
}

/// The watchdog gave up on a run that never reported back.
pub fn run_timed_out(run: &RunView, config: &Config) -> Email {
    let mut html = String::from("<h2>PostHog auto-fix timed out</h2>");
    html.push_str(&issue_section(run, config));
    let _ = write!(
        html,
        "<p>No result arrived within {} minutes. A result that arrives later is still \
         recorded and emailed.</p>",
        config.run_timeout_minutes
    );
    html.push_str(&run_line(run.github_run_url.as_deref()));
    Email {
        subject: subject("Timed out", run),
        html,
    }
}

/// A run could not be started after every attempt.
pub fn dispatch_failed(run: &RunView, error: &str, config: &Config) -> Email {
    let mut html = String::from("<h2>PostHog auto-fix could not start</h2>");
    html.push_str(&issue_section(run, config));
    html.push_str(&field("Attempts", &run.attempts.to_string()));
    html.push_str(&block("Last error", error));
    Email {
        subject: subject("Could not start", run),
        html,
    }
}

/// More new issues than the storm threshold arrived in one window. `top` is
/// `(exception type: message, issue count)`, most frequent first.
pub fn storm_digest(
    new_issues: usize,
    window_minutes: i64,
    top: &[(String, usize)],
    config: &Config,
) -> Email {
    let max_per_day = config.max_per_day;
    let mut html = format!(
        "<h2>PostHog error storm</h2>\
         <p>{new_issues} new issues in {window_minutes} minutes. The storm itself starts \
         nothing: each issue is still judged on its own once it settles. Noise is \
         skipped, repeats fold into one run, and at most {max_per_day} runs start a \
         day.</p><p><strong>Most frequent:</strong></p><ol>"
    );
    for (text, count) in top {
        let _ = write!(
            html,
            "<li>{} ({count})</li>",
            escape(&clean(text, NAME_MAX))
        );
    }
    html.push_str("</ol>");
    Email {
        subject: format!(
            "[AI fix] Error storm: {new_issues} new issues in {window_minutes} minutes"
        ),
        html,
    }
}

/// Dry run: the errors this tick would have started fixing, each with where
/// and to whom it last happened (`seen`, by PostHog issue id).
pub fn preview_digest(
    runs: &[RunView],
    seen: &HashMap<String, LastSeen>,
    config: &Config,
) -> Email {
    let mut html = String::from(
        "<h2>New errors (dry run)</h2>\
         <p>Auto-fix is in dry-run mode, so it changed nothing. In live mode it would \
         try to fix each error below.</p>",
    );
    for (number, run) in runs.iter().enumerate() {
        let _ = write!(
            html,
            "<hr><h3>{}. {}</h3>",
            number + 1,
            escape(&clean(&run.issue_name, NAME_MAX))
        );
        let message = run.issue_description.trim();
        html.push_str(&block(
            "Description",
            if message.is_empty() {
                "(no message)"
            } else {
                message
            },
        ));
        html.push_str(&last_seen(seen.get(&run.posthog_issue_id)));
        let source = run
            .source
            .as_deref()
            .and_then(Source::parse)
            .unwrap_or(Source::Unknown);
        html.push_str(&field("Happened", &happened(run, source)));
        html.push_str(&field("Source", source.label()));
        let _ = write!(
            html,
            "<p>{}</p>",
            link(&config.issue_url(&run.posthog_issue_id), "Open in PostHog")
        );
    }
    let errors = if runs.len() == 1 { "error" } else { "errors" };
    Email {
        subject: format!("[AI fix] Dry run: {} new {errors}", runs.len()),
        html,
    }
}

const NOT_KNOWN: &str = "not known";

/// The page (or the request, for an API error), the user and their company.
fn last_seen(seen: Option<&LastSeen>) -> String {
    let none = LastSeen::default();
    let seen = seen.unwrap_or(&none);
    let mut html = match (&seen.url, &seen.request) {
        // The page comes from the browser: only a web address becomes a link.
        (Some(url), _) if url.starts_with("https://") || url.starts_with("http://") => {
            format!("<p><strong>Page:</strong> {}</p>", link(url, url))
        }
        (Some(url), _) => field("Page", url),
        (None, Some(request)) => field("Request", request),
        (None, None) => field("Page", NOT_KNOWN),
    };
    let user = match (seen.user_id, seen.user_name.as_deref().map(str::trim)) {
        (Some(id), Some(name)) if !name.is_empty() => format!("{name} (user {id})"),
        (Some(id), _) => format!("user {id}"),
        (None, _) => NOT_KNOWN.into(),
    };
    html.push_str(&field("User", &user));
    let company = seen
        .company_id
        .map_or_else(|| NOT_KNOWN.into(), |id| id.to_string());
    html.push_str(&field("Company ID", &company));
    html
}

/// `4 times to 2 users`. Servers report under one shared id, so only browser
/// errors count users.
fn happened(run: &RunView, source: Source) -> String {
    let times = plural(run.occurrences, "time", "times");
    if source == Source::Web {
        format!("{times} to {}", plural(run.users, "user", "users"))
    } else {
        times
    }
}

fn plural(value: Option<u32>, one: &str, many: &str) -> String {
    match value {
        Some(1) => format!("1 {one}"),
        Some(value) => format!("{value} {many}"),
        None => format!("? {many}"),
    }
}

/// A fixed issue was reopened in PostHog.
pub fn regression(run: &RunView, config: &Config) -> Email {
    let mut html = String::from("<h2>PostHog reopened a fixed issue</h2>");
    html.push_str("<p>PostHog reopened an issue this pipeline fixed; check the PR's deploy.</p>");
    html.push_str(&issue_section(run, config));
    if let Some(pr) = &run.pr_url {
        let _ = write!(
            html,
            "<p><strong>Pull request:</strong> {}</p>",
            link(pr, pr)
        );
    }
    Email {
        subject: subject("Regression", run),
        html,
    }
}

/// `[AI fix] <label>: <issue name>`, on one line and cut to a sane length.
fn subject(label: &str, run: &RunView) -> String {
    format!("[AI fix] {label}: {}", clean(&run.issue_name, NAME_MAX))
}

fn label(report: &RunReport) -> String {
    let Some(agent) = report
        .agent
        .as_ref()
        .filter(|_| report.conclusion == Conclusion::Completed)
    else {
        return "Failed".into();
    };
    if agent.outcome.is_fix() && report.verification == Verification::Unverified {
        "Fixed, unverified (draft PR)".into()
    } else {
        agent.outcome.label().into()
    }
}

/// Collapses whitespace (a subject must stay on one line) and cuts to `max`.
fn clean(text: &str, max: usize) -> String {
    let one_line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if one_line.chars().count() <= max {
        return one_line;
    }
    let cut: String = one_line.chars().take(max.saturating_sub(3)).collect();
    format!("{}...", cut.trim_end())
}

fn issue_section(run: &RunView, config: &Config) -> String {
    let mut html = format!("<p><strong>Issue:</strong> {}</p>", issue_link(run, config));
    html.push_str(&block("Description", &run.issue_description));
    html.push_str(&field("Source", run.source.as_deref().unwrap_or("unknown")));
    let _ = write!(
        html,
        "<p><strong>Occurrences:</strong> {} &middot; <strong>Users:</strong> {}</p>",
        count(run.occurrences),
        count(run.users)
    );
    html
}

fn issue_link(run: &RunView, config: &Config) -> String {
    link(
        &config.issue_url(&run.posthog_issue_id),
        &clean(&run.issue_name, NAME_MAX),
    )
}

fn pull_request(report: &RunReport) -> String {
    if let Some(url) = &report.pr_url {
        let draft = if report.pr_draft { " (draft)" } else { "" };
        return format!(
            "<p><strong>Pull request:</strong> {}{draft}</p>",
            link(url, url)
        );
    }
    let fixed = report
        .agent
        .as_ref()
        .is_some_and(|agent| agent.outcome.is_fix());
    let why = if fixed {
        "the workflow did not open one; see the run and the notes"
    } else {
        "this result changes no code"
    };
    field("Pull request", &format!("none, {why}"))
}

fn agent_section(agent: &AgentResult) -> String {
    let mut html = block("Summary", &agent.summary);
    html.push_str(&block("Root cause", &agent.root_cause));
    if !agent.fix_summary.trim().is_empty() {
        html.push_str(&block("Fix", &agent.fix_summary));
    }
    html.push_str(&block("Risk", &agent.risk));
    html.push_str(&test_section(&agent.test));
    if let Some(recommendation) = &agent.recommendation {
        html.push_str(&block("Recommendation", recommendation));
    }
    if let Some(duplicate_of) = &agent.duplicate_of {
        html.push_str(&field("Already covered by", duplicate_of));
    }
    html
}

fn test_section(test: &TestReport) -> String {
    let mut html = String::new();
    match test.status {
        TestStatus::Written => {
            if let Some(command) = &test.command {
                let _ = write!(
                    html,
                    "<p><strong>Test command:</strong> <code>{}</code></p>",
                    escape(command)
                );
            }
            if !test.files.is_empty() {
                let files: Vec<String> = test
                    .files
                    .iter()
                    .map(|file| format!("<code>{}</code>", escape(file)))
                    .collect();
                let _ = write!(
                    html,
                    "<p><strong>Test files:</strong> {}</p>",
                    files.join(", ")
                );
            }
            if let Some(steps) = &test.manual_test_steps {
                html.push_str(&block("Check it in the app", steps));
            }
        }
        TestStatus::NotPossible => {
            let why = test.why_not_possible.as_deref().unwrap_or("not stated");
            html.push_str(&block("No automated test, because", why));
        }
        TestStatus::NotApplicable => {
            html.push_str("<p><strong>Test:</strong> none, there is no fix.</p>");
        }
    }
    if let Some(steps) = &test.manual_test_steps {
        html.push_str(&block("Manual test steps", steps));
    }
    html
}

fn run_line(url: Option<&str>) -> String {
    url.map(|url| format!("<p><strong>GitHub run:</strong> {}</p>", link(url, url)))
        .unwrap_or_default()
}

fn count(value: Option<u32>) -> String {
    value.map_or_else(|| "?".into(), |value| value.to_string())
}

fn field(label: &str, value: &str) -> String {
    format!("<p><strong>{label}:</strong> {}</p>", escape(value))
}

/// A labelled paragraph that keeps the text's line breaks.
fn block(label: &str, text: &str) -> String {
    format!(
        "<p><strong>{label}:</strong></p><p style=\"white-space: pre-wrap;\">{}</p>",
        escape(text)
    )
}

fn link(url: &str, text: &str) -> String {
    format!("<a href=\"{}\">{}</a>", escape(url), escape(text))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::posthog_autofix::model::{Outcome, TargetRepo};
    use chrono::Utc;

    const ISSUE: &str = "01a158ec-b8d5-7002-bb99-07e4e724acee";
    const PR: &str = "https://github.com/o/r/pull/7";

    fn run(name: &str, description: &str) -> RunView {
        RunView {
            id: 1,
            posthog_issue_id: ISSUE.into(),
            event_uuid: None,
            issue_created_at: Utc::now(),
            issue_name: name.into(),
            issue_description: description.into(),
            source: Some("web".into()),
            occurrences: Some(12),
            users: Some(3),
            signature: None,
            status: "running".into(),
            outcome: None,
            target_repo: None,
            pr_url: Some(PR.into()),
            github_run_url: None,
            attempts: 1,
            dispatched_at: None,
            started_at: None,
            finished_at: None,
        }
    }

    fn agent(outcome: Outcome) -> AgentResult {
        AgentResult {
            outcome,
            target_repo: Some(TargetRepo::GraniteWebhooks),
            summary: "Crash".into(),
            root_cause: "A null".into(),
            fix_summary: "Guard it".into(),
            risk: "Low".into(),
            test: TestReport {
                status: TestStatus::Written,
                files: vec!["api/tests/x.rs".into()],
                command: Some("cargo test -p api --test x".into()),
                why_not_possible: None,
                manual_test_steps: None,
            },
            recommendation: None,
            duplicate_of: None,
            pr_title: None,
        }
    }

    fn report(agent: Option<AgentResult>) -> RunReport {
        RunReport {
            issue_id: ISSUE.into(),
            conclusion: Conclusion::Completed,
            agent,
            verification: Verification::Verified,
            notes: None,
            pr_url: Some(PR.into()),
            pr_draft: false,
            cost_usd: Some(1.5),
            github_run_url: Some("https://github.com/o/r/actions/runs/9".into()),
        }
    }

    #[test]
    fn subjects_follow_the_outcome() {
        let config = Config::default();
        let run = run("TypeError: x is null", "d");
        let fixed = run_finished(&run, &report(Some(agent(Outcome::FixedWithTest))), &config);
        assert_eq!(
            fixed.subject,
            "[AI fix] Fixed with a test: TypeError: x is null"
        );

        let mut draft = report(Some(agent(Outcome::FixedWithTest)));
        draft.verification = Verification::Unverified;
        draft.pr_draft = true;
        let email = run_finished(&run, &draft, &config);
        assert!(
            email
                .subject
                .starts_with("[AI fix] Fixed, unverified (draft PR): ")
        );
        assert!(email.html.contains("(draft)"));

        let mut failed = report(None);
        failed.conclusion = Conclusion::Failed;
        failed.pr_url = None;
        assert!(
            run_finished(&run, &failed, &config)
                .subject
                .starts_with("[AI fix] Failed: ")
        );
    }

    #[test]
    fn the_storm_digest_says_issues_are_still_judged() {
        let config = Config::default();
        let html = storm_digest(25, 30, &[("TypeError: a".into(), 9)], &config).html;
        assert!(html.contains("each issue is still judged on its own"));
        assert!(html.contains(&format!("at most {} runs start a day", config.max_per_day)));
        assert!(!html.contains("None were started"));
    }

    #[test]
    fn digest_subjects() {
        let config = Config::default();
        assert_eq!(
            storm_digest(25, 30, &[("TypeError: a".into(), 9)], &config).subject,
            "[AI fix] Error storm: 25 new issues in 30 minutes"
        );
        let runs = [run("a", "d"), run("b", "d")];
        let seen = HashMap::new();
        assert_eq!(
            preview_digest(&runs, &seen, &config).subject,
            "[AI fix] Dry run: 2 new errors"
        );
        assert_eq!(
            preview_digest(&runs[..1], &seen, &config).subject,
            "[AI fix] Dry run: 1 new error"
        );
    }

    #[test]
    fn a_dry_run_names_the_error_the_page_the_user_and_the_company() {
        let run = run(
            "TypeError",
            "Cannot read properties of undefined (reading 'id')",
        );
        let page = "https://acme.granite-manager.com/employee/deals/7?tab=notes&x=1";
        let seen = HashMap::from([(
            ISSUE.to_string(),
            LastSeen {
                url: Some(page.into()),
                user_id: Some(103),
                user_name: Some("Sam <Super>".into()),
                company_id: Some(101),
                ..LastSeen::default()
            },
        )]);
        let html = preview_digest(std::slice::from_ref(&run), &seen, &Config::default()).html;
        assert!(html.contains("<h3>1. TypeError</h3>"));
        assert!(html.contains("Cannot read properties of undefined (reading 'id')"));
        let escaped_page = escape(page);
        assert!(html.contains(&format!(
            "<strong>Page:</strong> <a href=\"{escaped_page}\">{escaped_page}</a>"
        )));
        assert!(html.contains("<strong>User:</strong> Sam &lt;Super&gt; (user 103)"));
        assert!(html.contains("<strong>Company ID:</strong> 101"));
        assert!(html.contains("<strong>Happened:</strong> 12 times to 3 users"));
        assert!(html.contains("<strong>Source:</strong> web app"));
        assert!(html.contains(&format!(
            "<a href=\"{}\">Open in PostHog</a>",
            Config::default().issue_url(ISSUE)
        )));
        assert!(!html.contains("<table"));
    }

    #[test]
    fn a_dry_run_of_a_server_error_shows_its_request_and_no_user() {
        let mut api_run = run("ApiError", "  ");
        api_run.source = Some("granite-api".into());
        api_run.occurrences = Some(1);
        let seen = HashMap::from([(
            ISSUE.to_string(),
            LastSeen {
                request: Some("GET /v1/slabs?id=6074".into()),
                ..LastSeen::default()
            },
        )]);
        let html = preview_digest(std::slice::from_ref(&api_run), &seen, &Config::default()).html;
        assert!(html.contains("(no message)"));
        assert!(html.contains("<strong>Request:</strong> GET /v1/slabs?id=6074"));
        assert!(html.contains("<strong>User:</strong> not known"));
        assert!(html.contains("<strong>Company ID:</strong> not known"));
        assert!(html.contains("<strong>Happened:</strong> 1 time</p>"));
        assert!(html.contains("<strong>Source:</strong> API"));

        // PostHog unreadable: the digest still goes out, without the details.
        let html = preview_digest(&[run("e", "d")], &HashMap::new(), &Config::default()).html;
        assert!(html.contains("<strong>Page:</strong> not known"));
        // Only a web address becomes a link.
        let seen = HashMap::from([(
            ISSUE.to_string(),
            LastSeen {
                url: Some("javascript:alert(1)".into()),
                ..LastSeen::default()
            },
        )]);
        let html = preview_digest(&[run("e", "d")], &seen, &Config::default()).html;
        assert!(html.contains("<strong>Page:</strong> javascript:alert(1)</p>"));
    }

    #[test]
    fn long_names_are_cut_and_stay_on_one_line() {
        let name = format!("Error\n{}", "x".repeat(300));
        let email = run_timed_out(&run(&name, "d"), &Config::default());
        assert!(!email.subject.contains('\n'));
        assert!(email.subject.chars().count() <= "[AI fix] Timed out: ".len() + NAME_MAX);
        assert!(email.subject.ends_with("..."));
    }

    #[test]
    fn markup_in_ledger_values_is_escaped() {
        let config = Config::default();
        let run = run("<b>name</b>", "<script>alert(1)</script>");
        let mut report = report(Some(agent(Outcome::FixedWithTest)));
        report.notes = Some("<img src=x onerror=y>".into());
        let html = run_finished(&run, &report, &config).html;
        assert!(!html.contains("<script>") && !html.contains("<img"));
        assert!(html.contains("&lt;script&gt;alert(1)&lt;/script&gt;"));
        let failed = dispatch_failed(&run, "<i>boom</i>", &config).html;
        assert!(!failed.contains("<i>"));
    }

    #[test]
    fn a_fix_email_has_the_pull_request_and_the_test_command() {
        let email = run_finished(
            &run("e", "d"),
            &report(Some(agent(Outcome::FixedWithTest))),
            &Config::default(),
        );
        assert!(email.html.contains(&format!("href=\"{PR}\"")));
        assert!(email.html.contains("cargo test -p api --test x"));
        assert!(email.html.contains("api/tests/x.rs"));
        assert!(email.html.contains("$1.50 (Claude Code estimate)"));
        assert!(email.html.contains(&Config::default().issue_url(ISSUE)));
    }

    #[test]
    fn a_not_a_code_bug_email_has_the_recommendation() {
        let mut answer = agent(Outcome::NotACodeBug);
        answer.recommendation = Some("Suppress the rule in PostHog".into());
        answer.test.status = TestStatus::NotApplicable;
        let mut report = report(Some(answer));
        report.pr_url = None;
        report.verification = Verification::NotApplicable;
        let email = run_finished(&run("e", "d"), &report, &Config::default());
        assert!(email.subject.starts_with("[AI fix] Not a code bug: "));
        assert!(email.html.contains("Suppress the rule in PostHog"));
        assert!(email.html.contains("changes no code"));
    }

    #[test]
    fn regression_points_at_the_deploy_and_nothing_has_an_em_dash() {
        let config = Config::default();
        let run = run("e", "d");
        let emails = [
            regression(&run, &config),
            run_timed_out(&run, &config),
            dispatch_failed(&run, "boom", &config),
            storm_digest(21, 30, &[("a: b".into(), 4)], &config),
            preview_digest(std::slice::from_ref(&run), &HashMap::new(), &config),
            run_finished(&run, &report(Some(agent(Outcome::NeedsHuman))), &config),
        ];
        assert!(emails[0].html.contains("check the PR's deploy"));
        for email in &emails {
            assert!(!email.subject.contains('\u{2014}') && !email.html.contains('\u{2014}'));
        }
    }
}
