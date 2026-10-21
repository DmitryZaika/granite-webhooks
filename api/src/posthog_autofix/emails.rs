//! The auto-fix emails, rendered as plain functions. Every value from the
//! ledger or the workflow is escaped: error text is user and third-party input.

use super::config::Config;
use super::model::{
    AgentResult, Conclusion, RunReport, RunView, TestReport, TestStatus, Verification,
};
use super::notify::Email;
use crate::routes::bug_reports::escape;
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
    let _ = config;
    let mut html = format!(
        "<h2>PostHog error storm</h2>\
         <p>{new_issues} new issues in {window_minutes} minutes. None were started; \
         they stay in the ledger.</p><p><strong>Most frequent:</strong></p><ol>"
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

/// Dry run: the runs this tick would have started.
pub fn preview_digest(runs: &[RunView], config: &Config) -> Email {
    let mut html = String::from(
        "<h2>PostHog auto-fix dry run</h2>\
         <p>These issues would be fixed in live mode.</p>\
         <table cellpadding=\"4\" border=\"1\" style=\"border-collapse: collapse;\">\
         <tr><th>Issue</th><th>Source</th><th>Occurrences</th><th>Users</th></tr>",
    );
    for run in runs {
        let _ = write!(
            html,
            "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>",
            issue_link(run, config),
            escape(run.source.as_deref().unwrap_or("unknown")),
            count(run.occurrences),
            count(run.users),
        );
    }
    html.push_str("</table>");
    Email {
        subject: format!("[AI fix] Dry run: {} issue(s) would be fixed", runs.len()),
        html,
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
    fn digest_subjects() {
        let config = Config::default();
        assert_eq!(
            storm_digest(25, 30, &[("TypeError: a".into(), 9)], &config).subject,
            "[AI fix] Error storm: 25 new issues in 30 minutes"
        );
        let runs = [run("a", "d"), run("b", "d")];
        assert_eq!(
            preview_digest(&runs, &config).subject,
            "[AI fix] Dry run: 2 issue(s) would be fixed"
        );
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
            preview_digest(std::slice::from_ref(&run), &config),
            run_finished(&run, &report(Some(agent(Outcome::NeedsHuman))), &config),
        ];
        assert!(emails[0].html.contains("check the PR's deploy"));
        for email in &emails {
            assert!(!email.subject.contains('\u{2014}') && !email.html.contains('\u{2014}'));
        }
    }
}
