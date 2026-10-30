//! The auto-fix emails, rendered as plain functions. Every value from the
//! ledger or the workflow is escaped: error text is user and third-party input.

use super::config::Config;
use super::model::{AgentResult, LastSeen, RunReport, RunView, Source};
use super::notify::Email;
use crate::html::escape;
use std::collections::HashMap;
use std::fmt::Write;

const NAME_MAX: usize = 120;
/// One line of the bug report; the agent is asked for less.
const LINE_MAX: usize = 400;

/// A run opened a pull request: a short bug report and the pull request.
///
/// Subject `[AI fix] <title>`, then Given / When / Then, Actual / Expected /
/// Impact and Fix / Test. A result from an older workflow, without the
/// report's fields, shows its Issue and Fix lines instead.
pub fn pull_request_opened(
    run: &RunView,
    report: &RunReport,
    pr_url: &str,
    config: &Config,
) -> Email {
    let agent = report.agent.as_ref();
    let title = [
        agent.and_then(|agent| agent.pr_title.as_deref()),
        agent.map(|agent| agent.summary.as_str()),
        Some(run.issue_name.as_str()),
    ]
    .into_iter()
    .flatten()
    .find(|text| !text.trim().is_empty())
    .unwrap_or_default();
    let mut html = String::new();
    if let Some(agent) = agent {
        html.push_str(&bug_report(run, agent));
    }
    let draft = if report.pr_draft {
        " (draft: a check failed, see the PR)"
    } else {
        ""
    };
    let _ = write!(
        html,
        "<p><strong>PR:</strong> {}{draft}</p>",
        link(pr_url, pr_url)
    );
    let bug = run
        .signature
        .as_deref()
        .map(|signature| {
            format!(
                "Bug {} &middot; ",
                escape(super::signature::bug_id(signature))
            )
        })
        .unwrap_or_default();
    let _ = write!(
        html,
        "<p><small>{bug}{}</small></p>",
        link(&config.issue_url(&run.posthog_issue_id), "PostHog issue")
    );
    Email {
        subject: format!("[AI fix] {}", clean(title, NAME_MAX)),
        html,
    }
}

/// The agent's report as three short paragraphs; empty lines are left out.
fn bug_report(run: &RunView, agent: &AgentResult) -> String {
    let flow = paragraph(&[
        ("<strong>Given</strong> ", &agent.given),
        ("<strong>When</strong> ", &agent.when),
        ("<strong>Then</strong> ", &agent.then),
    ]);
    let fix = paragraph(&[("<strong>Fix:</strong> ", &agent.fix_summary)]);
    if flow.is_empty() {
        return format!("{}{fix}", field("Issue", &clean(&agent.summary, LINE_MAX)));
    }
    let source = run
        .source
        .as_deref()
        .and_then(Source::parse)
        .unwrap_or(Source::Unknown);
    let impact = format!("{}. {}", happened(run, source), agent.impact);
    let outcome = paragraph(&[
        ("<strong>Actual:</strong> ", &agent.actual),
        ("<strong>Expected:</strong> ", &agent.expected),
        ("<strong>Impact:</strong> ", &impact),
    ]);
    let change = paragraph(&[
        ("<strong>Fix:</strong> ", &agent.fix_summary),
        ("<strong>Test:</strong> ", &agent.check),
    ]);
    format!("{flow}{outcome}{change}")
}

/// `<p>label text<br>label text</p>` for the lines with text, each escaped and
/// cut to one line; empty when no line has text.
fn paragraph(lines: &[(&str, &str)]) -> String {
    let lines: Vec<String> = lines
        .iter()
        .map(|(label, text)| (label, clean(text, LINE_MAX)))
        .filter(|(_, text)| !text.is_empty())
        .map(|(label, text)| format!("{label}{}", escape(&text)))
        .collect();
    if lines.is_empty() {
        return String::new();
    }
    format!("<p>{}</p>", lines.join("<br>"))
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
    use crate::posthog_autofix::model::{
        AgentResult, Conclusion, Outcome, TargetRepo, TestReport, TestStatus, Verification,
    };
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
            given: "a company with calling turned off".into(),
            when: "a user opens /employee and the dock calls POST /v1/phone/web/devices".into(),
            then: "the API answers 403 calling_disabled".into(),
            actual: "the SPA reports the 403 to PostHog as an error".into(),
            expected: "a normal refusal; the dock shows calling is off".into(),
            impact: "Users see nothing; PostHog noise only.".into(),
            check: "Repeat Given/When: no calling_disabled error in PostHog.".into(),
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
    fn a_pull_request_email_is_a_short_bug_report_with_the_links() {
        let config = Config::default();
        let mut run = run("TypeError: x is null", "d");
        let signature = "3fa9c0d1b2e4".repeat(5);
        run.signature = Some(signature.clone());
        let mut answer = agent(Outcome::FixedWithTest);
        answer.summary = "Print fails on /employee/quote/:quoteId".into();
        answer.pr_title = Some("Fix quote print\nwith an open preview".into());
        let email = pull_request_opened(&run, &report(Some(answer.clone())), PR, &config);
        assert_eq!(
            email.subject,
            "[AI fix] Fix quote print with an open preview"
        );
        assert!(email.html.starts_with(
            "<p><strong>Given</strong> a company with calling turned off<br>\
             <strong>When</strong> a user opens /employee and the dock calls \
             POST /v1/phone/web/devices<br>\
             <strong>Then</strong> the API answers 403 calling_disabled</p>\
             <p><strong>Actual:</strong> the SPA reports the 403 to PostHog as an error<br>\
             <strong>Expected:</strong> a normal refusal; the dock shows calling is off<br>\
             <strong>Impact:</strong> 12 times to 3 users. Users see nothing; PostHog \
             noise only.</p>\
             <p><strong>Fix:</strong> Guard it<br>\
             <strong>Test:</strong> Repeat Given/When: no calling_disabled error in \
             PostHog.</p>"
        ));
        assert!(!email.html.contains("Issue:"));
        assert!(email.html.contains(&format!(
            "<p><strong>PR:</strong> <a href=\"{PR}\">{PR}</a></p>"
        )));
        assert!(!email.html.contains("draft"));
        assert!(
            email
                .html
                .contains(&format!("Bug {} &middot; ", &signature[..12]))
        );
        assert!(email.html.contains(&format!(
            "<a href=\"{}\">PostHog issue</a>",
            config.issue_url(ISSUE)
        )));
        for left_out in ["Root cause", "Risk", "cargo test", "$1.50", "actions/runs"] {
            assert!(!email.html.contains(left_out), "{left_out}");
        }

        // No pr_title: the summary names it; no summary either: the issue name.
        answer.pr_title = None;
        let email = pull_request_opened(&run, &report(Some(answer.clone())), PR, &config);
        assert_eq!(
            email.subject,
            "[AI fix] Print fails on /employee/quote/:quoteId"
        );
        answer.summary = " ".into();
        let email = pull_request_opened(&run, &report(Some(answer)), PR, &config);
        assert_eq!(email.subject, "[AI fix] TypeError: x is null");
    }

    #[test]
    fn a_draft_pull_request_says_so_and_a_missing_line_or_bug_id_is_left_out() {
        let config = Config::default();
        let mut answer = agent(Outcome::FixedWithTest);
        answer.fix_summary = "  ".into();
        answer.then = String::new();
        answer.impact = String::new();
        let mut draft = report(Some(answer));
        draft.pr_draft = true;
        let mut server = run("e", "d");
        server.source = Some("granite-api".into());
        let email = pull_request_opened(&server, &draft, PR, &config);
        assert!(email.html.contains("(draft: a check failed, see the PR)"));
        assert!(!email.html.contains("<strong>Fix:</strong>"));
        assert!(!email.html.contains("<strong>Then</strong>"));
        assert!(email.html.contains("<p><strong>Test:</strong> Repeat"));
        // A server error has no users; the counts stand alone.
        assert!(
            email
                .html
                .contains("<strong>Impact:</strong> 12 times.</p>")
        );
        assert!(!email.html.contains("Bug "));
        assert!(email.html.contains("PostHog issue"));
    }

    #[test]
    fn a_result_from_an_older_workflow_shows_its_issue_line() {
        let answer: AgentResult = serde_json::from_value(serde_json::json!({
            "outcome": "fixed_with_test",
            "target_repo": "general_datebase",
            "summary": "Print fails\non /employee/quote/:quoteId",
            "root_cause": "r",
            "fix_summary": "Guard it",
            "risk": "",
            "test": {"status": "written", "files": [], "command": "bunx vitest run a.test.ts",
                     "why_not_possible": null, "manual_test_steps": null},
            "recommendation": null,
            "duplicate_of": null,
            "pr_title": null,
        }))
        .expect("the old result shape still parses");
        let html = pull_request_opened(
            &run("e", "d"),
            &report(Some(answer)),
            PR,
            &Config::default(),
        )
        .html;
        assert!(html.starts_with(
            "<p><strong>Issue:</strong> Print fails on /employee/quote/:quoteId</p>\
             <p><strong>Fix:</strong> Guard it</p><p><strong>PR:</strong>"
        ));
        assert!(!html.contains("Given") && !html.contains("Impact"));
    }

    #[test]
    fn markup_in_agent_text_is_escaped() {
        let mut answer = agent(Outcome::FixedWithTest);
        answer.given = "<script>alert(1)</script>".into();
        answer.fix_summary = "<img src=x onerror=y>".into();
        answer.pr_title = Some("<b>title</b>".into());
        let email = pull_request_opened(
            &run("<i>name</i>", "d"),
            &report(Some(answer)),
            "https://github.com/o/r/pull/7?a=\"b\"",
            &Config::default(),
        );
        assert!(!email.html.contains("<script>") && !email.html.contains("<img"));
        assert!(email.html.contains("&lt;script&gt;alert(1)&lt;/script&gt;"));
        assert!(email.html.contains("&lt;img src=x onerror=y&gt;"));
        assert!(!email.html.contains("pull/7?a=\"b\""));
    }

    #[test]
    fn digest_subjects() {
        let config = Config::default();
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
        let email = pull_request_opened(&run(&name, "d"), &report(None), PR, &Config::default());
        assert!(!email.subject.contains('\n'));
        assert!(email.subject.chars().count() <= "[AI fix] ".len() + NAME_MAX);
        assert!(email.subject.ends_with("..."));
    }

    #[test]
    fn regression_points_at_the_deploy_and_nothing_has_an_em_dash() {
        let config = Config::default();
        let run = run("e", "d");
        let emails = [
            regression(&run, &config),
            preview_digest(std::slice::from_ref(&run), &HashMap::new(), &config),
            pull_request_opened(
                &run,
                &report(Some(agent(Outcome::FixedWithTest))),
                PR,
                &config,
            ),
        ];
        assert!(emails[0].html.contains("check the PR's deploy"));
        for email in &emails {
            assert!(!email.subject.contains('\u{2014}') && !email.html.contains('\u{2014}'));
        }
    }
}
