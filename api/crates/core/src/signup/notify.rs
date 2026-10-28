//! The notices of a completed signup.
//!
//! The welcome email to the owner, and the staff notice (Telegram to
//! `SIGNUP_NOTIFY_USER_ID`, email to `SIGNUP_NOTIFY_EMAIL`) so someone calls
//! the new company. Every send is best effort: a failure is logged with the
//! company id and nothing else.

use super::consent::long_date;
use super::intent::Intent;
use crate::billing::catalog::SIGNUP_TZ;
use crate::entitlements::AddonKey;
use crate::state::AppState;
use crate::webhooks_client::{OutgoingEmail, SendFailure};
use chrono::{NaiveDate, NaiveDateTime, TimeZone};
use lambda_http::tracing;
use std::fmt::Write as _;

const FROM_ADDRESS: &str = "noreply@granite-manager.com";
const SUPPORT_EMAIL: &str = "support@granite-manager.com";

fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// The buyer's calendar day of a UTC time, `January 23, 2027`.
fn local_day(at: Option<NaiveDateTime>) -> String {
    at.map_or_else(
        || "not set".into(),
        |at| long_date(SIGNUP_TZ.from_utc_datetime(&at).date_naive()),
    )
}

fn addons(intent: &Intent) -> String {
    if intent.plan.addons.is_empty() {
        return "none".into();
    }
    intent
        .plan
        .addons
        .iter()
        .map(|key| key.label())
        .collect::<Vec<_>>()
        .join(", ")
}

fn email(to: &str, subject: String, body: String) -> OutgoingEmail {
    OutgoingEmail {
        from_name: Some("Granite Manager".into()),
        from_address: FROM_ADDRESS.into(),
        to: vec![to.to_string()],
        cc: Vec::new(),
        bcc: Vec::new(),
        subject,
        body,
        signature: None,
        reply_to: Vec::new(),
        in_reply_to: None,
        references: Vec::new(),
        attachments: Vec::new(),
        transactional: true,
    }
}

/// Sends the welcome email and the staff notice for a completed signup.
pub async fn after_signup(state: &AppState, intent: &Intent, company_id: i32, user_id: i32) {
    let welcome = email(
        &intent.email,
        "Welcome to Granite Manager".into(),
        welcome_html(intent, &state.app_url),
    );
    if let Err(err) = state.webhooks.send_email(&welcome).await {
        tracing::warn!(
            company_id,
            kind = failure_kind(&err),
            "signup welcome email failed"
        );
    }
    if let Some(staff) = state.signup.notify_user_id
        && !state
            .webhooks
            .notify_telegram(staff, &telegram_text(intent, company_id, user_id))
            .await
    {
        tracing::warn!(company_id, "signup Telegram notice failed");
    }
    if let Some(to) = state.signup.notify_email.as_deref() {
        let notice = email(
            to,
            format!("New signup: {}", intent.company.name),
            staff_html(intent, company_id, user_id),
        );
        if let Err(err) = state.webhooks.send_email(&notice).await {
            tracing::warn!(
                company_id,
                kind = failure_kind(&err),
                "signup staff email failed"
            );
        }
    }
}

/// Tells staff a signup's provisioning was given up on (Telegram and email),
/// with the hashed signup id (`signup` = `funnel::distinct_id`), never the id
/// or the buyer's details. Best effort.
pub async fn provisioning_given_up(
    state: &AppState,
    signup: &str,
    company_id: Option<i32>,
    last_error: Option<&str>,
) {
    let company = company_id.map_or_else(
        || "none yet (the signup expired; the buyer starts again)".to_string(),
        |id| format!("{id} (finish its subscription by hand)"),
    );
    let error = last_error.unwrap_or("unknown");
    let text = format!(
        "Signup provisioning gave up after 26 hours\nSignup: {signup}\nCompany: {company}\nLast error: {error}"
    );
    if let Some(staff) = state.signup.notify_user_id
        && !state.webhooks.notify_telegram(staff, &text).await
    {
        tracing::warn!(?company_id, "signup give-up Telegram notice failed");
    }
    if let Some(to) = state.signup.notify_email.as_deref() {
        let body = format!(
            r#"<div style="font-family: Arial, sans-serif; max-width: 480px; margin: 0 auto;">
  <h2 style="color: #1e293b;">Signup provisioning gave up</h2>
  <p style="color: #475569; line-height: 1.6;">The maintenance job stopped retrying a self-serve signup 26 hours after the buyer accepted the terms.</p>
  <p style="color: #475569; line-height: 1.6;">Signup: {}<br>Company: {}<br>Last error: {}</p>
</div>"#,
            escape(signup),
            escape(&company),
            escape(error)
        );
        let notice = email(to, "Signup provisioning gave up".into(), body);
        if let Err(err) = state.webhooks.send_email(&notice).await {
            tracing::warn!(
                ?company_id,
                kind = failure_kind(&err),
                "signup give-up email failed"
            );
        }
    }
}

/// `Acme Stone accepted their plan: Quotes, first charge Nov 23, 2026`.
fn plan_accepted_text(
    company_name: &str,
    addons: &[AddonKey],
    first_charge_on: NaiveDate,
) -> String {
    let addons = if addons.is_empty() {
        "no add-ons".to_string()
    } else {
        addons
            .iter()
            .map(|key| key.label())
            .collect::<Vec<_>>()
            .join(", ")
    };
    format!(
        "{company_name} accepted their plan: {addons}, first charge {}",
        first_charge_on.format("%b %-d, %Y")
    )
}

/// Tells staff (Telegram and email, as after a signup) that a company
/// accepted its plan invite. Best effort.
pub async fn after_plan_accepted(
    state: &AppState,
    company_name: &str,
    company_id: i32,
    addons: &[AddonKey],
    first_charge_on: NaiveDate,
) {
    let text = plan_accepted_text(company_name, addons, first_charge_on);
    if let Some(staff) = state.signup.notify_user_id
        && !state
            .webhooks
            .notify_telegram(staff, &format!("{text}\nCompany id: {company_id}"))
            .await
    {
        tracing::warn!(company_id, "plan accepted Telegram notice failed");
    }
    if let Some(to) = state.signup.notify_email.as_deref() {
        let body = format!(
            r#"<div style="font-family: Arial, sans-serif; max-width: 480px; margin: 0 auto;">
  <h2 style="color: #1e293b;">Plan accepted</h2>
  <p style="color: #475569; line-height: 1.6;">{}.</p>
  <p style="color: #475569; line-height: 1.6;">Company id: {company_id}</p>
</div>"#,
            escape(&text)
        );
        let notice = email(to, format!("Plan accepted: {company_name}"), body);
        if let Err(err) = state.webhooks.send_email(&notice).await {
            tracing::warn!(
                company_id,
                kind = failure_kind(&err),
                "plan accepted staff email failed"
            );
        }
    }
}

/// Tells staff the maintenance job stopped finishing a company's plan
/// acceptance (Telegram and email). Best effort.
pub async fn plan_acceptance_given_up(state: &AppState, company_id: i32, last_error: Option<&str>) {
    let error = last_error.unwrap_or("unknown");
    let text = format!(
        "Plan acceptance gave up after 26 hours\nCompany: {company_id} (finish its subscription by hand)\nLast error: {error}"
    );
    if let Some(staff) = state.signup.notify_user_id
        && !state.webhooks.notify_telegram(staff, &text).await
    {
        tracing::warn!(company_id, "plan acceptance give-up Telegram notice failed");
    }
    if let Some(to) = state.signup.notify_email.as_deref() {
        let body = format!(
            r#"<div style="font-family: Arial, sans-serif; max-width: 480px; margin: 0 auto;">
  <h2 style="color: #1e293b;">Plan acceptance gave up</h2>
  <p style="color: #475569; line-height: 1.6;">The maintenance job stopped retrying a company's plan acceptance 26 hours after a Manager accepted it.</p>
  <p style="color: #475569; line-height: 1.6;">Company: {company_id}<br>Last error: {}</p>
</div>"#,
            escape(error)
        );
        let notice = email(to, "Plan acceptance gave up".into(), body);
        if let Err(err) = state.webhooks.send_email(&notice).await {
            tracing::warn!(
                company_id,
                kind = failure_kind(&err),
                "plan acceptance give-up email failed"
            );
        }
    }
}

/// The failure without its text, which can quote the recipient's address.
const fn failure_kind(err: &SendFailure) -> &'static str {
    match err {
        SendFailure::Rejected(_) => "rejected",
        SendFailure::Failed(_) => "failed",
    }
}

fn welcome_html(intent: &Intent, app_url: &str) -> String {
    let name = escape(&intent.owner_name);
    let company = escape(&intent.company.name);
    let first_charge = local_day(intent.trial_end_at);
    let deadline = local_day(intent.deadline_at);
    format!(
        r#"<div style="font-family: Arial, sans-serif; max-width: 480px; margin: 0 auto;">
  <h2 style="color: #1e293b;">Welcome to Granite Manager</h2>
  <p style="color: #475569; line-height: 1.6;">Hello {name},</p>
  <p style="color: #475569; line-height: 1.6;">{company} is ready. Sign in with your email and the password you chose.</p>
  <a href="{app_url}/login" style="display: inline-block; background-color: #2563eb; color: white; padding: 12px 24px; border-radius: 6px; text-decoration: none; font-weight: 600; margin: 16px 0;">Sign in</a>
  <p style="color: #475569; line-height: 1.6;">Your free period runs until {first_charge}. Nothing is charged before that day.</p>
  <p style="color: #475569; line-height: 1.6;">Book your onboarding call and complete onboarding by {deadline}: <a href="{app_url}/employee?welcome=1" style="color: #2563eb;">book your call</a>.</p>
  <p style="color: #94a3b8; font-size: 13px; margin-top: 24px;">Questions? Write to <a href="mailto:{SUPPORT_EMAIL}" style="color: #2563eb;">{SUPPORT_EMAIL}</a>.</p>
</div>"#
    )
}

fn telegram_text(intent: &Intent, company_id: i32, user_id: i32) -> String {
    format!(
        "New self-serve signup\nCompany: {} (id {company_id})\nOwner: {} (user {user_id})\nUsers (estimate): {}\nAdd-ons: {}\nFirst charge: {}",
        intent.company.name,
        intent.owner_name,
        intent.plan.users,
        addons(intent),
        local_day(intent.trial_end_at),
    )
}

fn staff_html(intent: &Intent, company_id: i32, user_id: i32) -> String {
    let details = &intent.company;
    let rows = [
        ("Company", format!("{} (id {company_id})", details.name)),
        ("Owner", format!("{} (user {user_id})", intent.owner_name)),
        ("Owner email", intent.email.clone()),
        ("Company phone", details.phone.clone()),
        (
            "Address",
            format!(
                "{}, {}, {} {}",
                details.street, details.city, details.state, details.zip
            ),
        ),
        (
            "Website",
            details.website.clone().unwrap_or_else(|| "none".into()),
        ),
        ("Users (estimate)", intent.plan.users.to_string()),
        ("Add-ons", addons(intent)),
        ("First charge", local_day(intent.trial_end_at)),
        ("Onboarding deadline", local_day(intent.deadline_at)),
    ];
    let rows = rows.iter().fold(String::new(), |mut out, (label, value)| {
        let _ = writeln!(
            out,
            r#"    <tr><td style="color: #94a3b8; padding: 4px 12px 4px 0;">{label}</td><td style="color: #1e293b; padding: 4px 0;">{}</td></tr>"#,
            escape(value)
        );
        out
    });
    format!(
        r#"<div style="font-family: Arial, sans-serif; max-width: 480px; margin: 0 auto;">
  <h2 style="color: #1e293b;">New self-serve signup</h2>
  <p style="color: #475569; line-height: 1.6;">A company signed up on payments.granite-manager.com. Call them to book onboarding.</p>
  <table style="font-size: 14px; border-collapse: collapse;">
{rows}  </table>
</div>"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::billing::catalog::PlanChoice;
    use crate::signup::intent::{CompanyDetails, SignupStatus};

    fn intent() -> Intent {
        let at = NaiveDate::from_ymd_opt(2026, 10, 23)
            .unwrap()
            .and_hms_opt(15, 0, 0)
            .unwrap();
        Intent {
            id: "6f1c1f0e-1111-4222-8333-444455556666".into(),
            status: SignupStatus::Completed,
            email: "owner@stone.test".into(),
            code_hash: None,
            code_sent_at: None,
            code_expires_at: None,
            code_attempts: 0,
            codes_sent: 1,
            verified_at: Some(at),
            company: CompanyDetails {
                name: "Rock & <Roll>".into(),
                phone: "+13175550100".into(),
                website: None,
                street: "1 Main St".into(),
                city: "Indianapolis".into(),
                state: "IN".into(),
                zip: "46204".into(),
            },
            owner_name: "Olive \"O\" Owner".into(),
            password_hash: String::new(),
            plan: PlanChoice {
                users: 12,
                addons: vec![AddonKey::Quotes],
            },
            stripe_customer_id: Some("cus_1".into()),
            setup_intent_id: Some("seti_1".into()),
            setup_attempts: 1,
            payment_method_id: Some("pm_1".into()),
            accepted_at: Some(at),
            // 03:00 UTC on Jan 24 is still Jan 23 in Indianapolis.
            trial_end_at: NaiveDate::from_ymd_opt(2027, 1, 24)
                .unwrap()
                .and_hms_opt(3, 0, 0),
            fee_cents: Some(150_000),
            deadline_at: NaiveDate::from_ymd_opt(2026, 11, 22)
                .unwrap()
                .and_hms_opt(15, 0, 0),
            price_ids: Some(vec!["price_base".into()]),
            stripe_subscription_id: Some("sub_1".into()),
            provision_attempts: 0,
            last_error: None,
            company_id: Some(7),
            user_id: Some(9),
            ip: None,
            user_agent: None,
            created_at: at,
            updated_at: at,
            expires_at: at,
            completed_at: Some(at),
            bank_verification_until: None,
        }
    }

    #[test]
    fn welcome_email_escapes_names_and_shows_local_dates() {
        let html = welcome_html(&intent(), "https://granite-manager.com");
        assert!(html.contains("Hello Olive &quot;O&quot; Owner,"));
        assert!(html.contains("Rock &amp; &lt;Roll&gt; is ready"));
        assert!(html.contains("until January 23, 2027"));
        assert!(html.contains("by November 22, 2026"));
        assert!(html.contains("https://granite-manager.com/login"));
        assert!(html.contains("https://granite-manager.com/employee?welcome=1"));
        assert!(!html.contains('\u{2014}'));
    }

    #[test]
    fn the_plan_accepted_notice_names_the_add_ons_and_first_charge() {
        let day = NaiveDate::from_ymd_opt(2026, 11, 23).unwrap();
        assert_eq!(
            plan_accepted_text("Acme Stone", &[AddonKey::Quotes], day),
            "Acme Stone accepted their plan: Quotes, first charge Nov 23, 2026"
        );
        assert_eq!(
            plan_accepted_text("Acme Stone", &[], day),
            "Acme Stone accepted their plan: no add-ons, first charge Nov 23, 2026"
        );
    }

    #[test]
    fn telegram_has_no_contact_details_and_staff_email_has_them() {
        let text = telegram_text(&intent(), 7, 9);
        assert!(text.contains("Rock & <Roll> (id 7)"));
        assert!(text.contains("Add-ons: Quotes"));
        assert!(text.contains("First charge: January 23, 2027"));
        assert!(!text.contains("owner@stone.test") && !text.contains("5550100"));
        let html = staff_html(&intent(), 7, 9);
        assert!(html.contains("owner@stone.test") && html.contains("+13175550100"));
        assert!(html.contains("Rock &amp; &lt;Roll&gt;"));
    }
}
