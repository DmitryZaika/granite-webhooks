//! Sending a company's 10DLC registration to Telnyx and following it
//! (superusers only; the form is [`super::tendlc`]).
//!
//! - [`send`]: the first send. Registers the brand (never a second one: a
//!   create whose answer never came is looked up on Telnyx first), then the
//!   campaign once the brand is registered (its `referenceId` finds a
//!   campaign an earlier try created).
//! - [`resend`]: after a rejection. The edited brand, the campaign's
//!   samples, message flow and help message, an appeal; a new campaign when
//!   TCR refused the first one.
//! - [`refresh`]: reads the brand and campaign (and why they failed) and
//!   moves the registration on; a sent registration whose brand is now
//!   registered gets its campaign. The superuser's Refresh, Telnyx's 10DLC
//!   notifications and the phone job call it.
//! - [`assign_numbers`]: the company's active numbers to the approved
//!   campaign.
//!
//! Each status change happens once (compare and set), so its notification
//! (superusers when a company marks it ready; the company's admins and
//! Managers when it is approved, rejected or suspended) goes out once,
//! whichever path saw it first. Sends and assignments hold a lock on the
//! registration, so two never race to create brands, campaigns or
//! assignments (each costs money). Nothing here logs the form or phone
//! numbers.

use super::alerts::company_admins;
use super::company::{self, CompanyConfig};
use super::tendlc::{
    self, BRAND_FAILED, Row, TenDlcDraft, TenDlcFeedback, TenDlcStatus, audit, brand_body,
    campaign_body, campaign_update_body, editable_fields_changed, move_status,
};
use super::tendlc_check as ai_check;
use crate::auth::sha256_hex;
use crate::state::AppState;
use crate::telnyx::tendlc::{Brand, Campaign, TenDlcError};
use chrono::{DateTime, Utc};
use lambda_http::tracing;
use serde_json::{Value, json};
use sqlx::MySqlPool;
use std::collections::HashMap;
use std::time::Instant;

/// Telnyx's campaign statuses of a campaign that was refused.
const CAMPAIGN_FAILED: [&str; 4] = [
    "TCR_FAILED",
    "TELNYX_FAILED",
    "MNO_REJECTED",
    "MNO_PROVISIONING_FAILED",
];
/// Ours, for a campaign Telnyx refused to create (there is no campaign).
pub const CAMPAIGN_REFUSED: &str = "REFUSED";
/// Who the notifications are from.
const ACTOR: &str = "Telnyx 10DLC";
/// `notifications.notification_type` of a registration marked ready (to
/// superusers) and of its outcome (to the company).
pub const READY_NOTIFICATION: &str = "tendlc_ready";
pub const STATUS_NOTIFICATION: &str = "tendlc_status";
/// `positions.id` of a Manager.
const MANAGER_POSITION: i32 = 5;
/// Registrations, and number assignments, read again per job run.
const JOB_BATCH: i64 = 5;
const JOB_NUMBER_BATCH: i64 = 10;
/// Longest appeal, in characters.
pub const APPEAL_CHARS: usize = 1000;

/// Why sending, refreshing or assigning failed.
#[derive(Debug, thiserror::Error)]
pub enum RegistryError {
    /// Something blocks it (400): what.
    #[error("{0}")]
    Invalid(String),
    /// Not now (409): why.
    #[error("{0}")]
    Conflict(String),
    /// Telnyx refused (400): its reason, phone numbers masked.
    #[error("{0}")]
    Refused(String),
    /// Telnyx did not answer (502).
    #[error("{0}")]
    Unanswered(String),
    /// Telnyx is not set up on this server (503).
    #[error("Texting registration is not set up on the server.")]
    NotConfigured,
    #[error(transparent)]
    Database(#[from] sqlx::Error),
}

impl RegistryError {
    /// What failed, for logs: Telnyx's reasons may quote the form, so they
    /// are never logged.
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::Invalid(_) => "invalid",
            Self::Conflict(_) => "conflict",
            Self::Refused(_) => "refused",
            Self::Unanswered(_) => "unanswered",
            Self::NotConfigured => "not_configured",
            Self::Database(_) => "database",
        }
    }
}

/// A failed Telnyx request as a [`RegistryError`] about `what` (logged
/// without Telnyx's reason).
fn failed(err: &TenDlcError, what: &'static str) -> RegistryError {
    tracing::warn!(error = %err, what, "Telnyx 10DLC request failed");
    match err {
        TenDlcError::NotConfigured => RegistryError::NotConfigured,
        err if err.refused() => RegistryError::Refused(match err.detail() {
            Some(reason) => format!("Telnyx did not accept the {what}: {reason}"),
            None => format!("Telnyx did not accept the {what}. Check the details and try again."),
        }),
        _ => RegistryError::Unanswered(format!(
            "Telnyx did not answer about the {what}. Try again in a minute."
        )),
    }
}

/// 400 naming the first thing missing, when the form is not complete (the
/// IRS letter too when `letter_needed`: the server reads letters).
pub fn complete(draft: &TenDlcDraft, letter_needed: bool) -> Result<(), RegistryError> {
    match tendlc::problems_with_letter(draft, letter_needed).first() {
        Some(problem) => Err(RegistryError::Invalid(format!(
            "The registration is not complete: {}",
            problem.message
        ))),
        None => Ok(()),
    }
}

/// The company's registration: 400 when it has none.
async fn registration(pool: &MySqlPool, company_id: i32) -> Result<Row, RegistryError> {
    tendlc::load(pool, company_id).await?.ok_or_else(|| {
        RegistryError::Invalid("The company has not filled in its texting registration yet.".into())
    })
}

/// The company's Telnyx setup (its webhook token names the company in
/// Telnyx's notifications): 400 without one, 503 when Telnyx is not set up
/// on the server.
async fn setup(state: &AppState, company_id: i32) -> Result<CompanyConfig, RegistryError> {
    let config = company::load(&state.pool, company_id)
        .await?
        .ok_or_else(|| {
            RegistryError::Invalid(
                "Save the company's Telnyx setup first: it makes the address Telnyx reports to."
                    .into(),
            )
        })?;
    if !state.telnyx.is_enabled() {
        return Err(RegistryError::NotConfigured);
    }
    Ok(config)
}

/// Where Telnyx sends the company's 10DLC notifications.
pub fn webhook_url(state: &AppState, config: &CompanyConfig) -> String {
    format!("{}/10dlc", config.webhook_base(&state.phone))
}

/// Takes the registration's send lock: false while another send holds it
/// (a lock older than 2 minutes is stale).
async fn lock(pool: &MySqlPool, id: i32) -> sqlx::Result<bool> {
    Ok(sqlx::query(
        "UPDATE tendlc_registrations SET send_lock_at = UTC_TIMESTAMP()
          WHERE id = ? AND (send_lock_at IS NULL
                            OR send_lock_at < UTC_TIMESTAMP() - INTERVAL 2 MINUTE)",
    )
    .bind(id)
    .execute(pool)
    .await?
    .rows_affected()
        == 1)
}

async fn unlock(pool: &MySqlPool, id: i32) -> sqlx::Result<()> {
    sqlx::query("UPDATE tendlc_registrations SET send_lock_at = NULL WHERE id = ?")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

async fn reload(pool: &MySqlPool, row: &Row) -> sqlx::Result<Row> {
    Ok(tendlc::load(pool, row.company_id)
        .await?
        .unwrap_or_else(|| row.clone()))
}

/// Saves what went wrong last (no personal data).
async fn set_error(pool: &MySqlPool, id: i32, error: &str) -> sqlx::Result<()> {
    sqlx::query(
        "UPDATE tendlc_registrations SET last_error = ?, last_error_at = UTC_TIMESTAMP()
          WHERE id = ?",
    )
    .bind(error.chars().take(500).collect::<String>())
    .bind(id)
    .execute(pool)
    .await?;
    Ok(())
}

fn feedback_json(items: &[TenDlcFeedback]) -> Option<String> {
    (!items.is_empty()).then(|| serde_json::to_string(items).unwrap_or_else(|_| "[]".into()))
}

/// Adds Telnyx's reasons about the campaign (once each).
pub async fn add_campaign_feedback(
    pool: &MySqlPool,
    row: &Row,
    items: &[TenDlcFeedback],
) -> sqlx::Result<()> {
    let mut all = row.campaign_feedback();
    for item in items {
        if !item.description.is_empty()
            && !all
                .iter()
                .any(|known| known.description == item.description)
        {
            all.push(item.clone());
        }
    }
    all.truncate(20);
    sqlx::query("UPDATE tendlc_registrations SET campaign_feedback = ? WHERE id = ?")
        .bind(feedback_json(&all))
        .bind(row.id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Saves the brand as Telnyx describes it, and `sent` as what it was sent.
async fn save_brand(
    pool: &MySqlPool,
    id: i32,
    brand: &Brand,
    sent: Option<&tendlc::TenDlcBrandDraft>,
) -> sqlx::Result<()> {
    let clip = |text: &Option<String>, chars: usize| {
        text.as_deref()
            .map(|text| text.chars().take(chars).collect::<String>())
    };
    sqlx::query(
        "UPDATE tendlc_registrations
            SET telnyx_brand_id = ?, tcr_brand_id = COALESCE(?, tcr_brand_id),
                brand_status = COALESCE(?, brand_status),
                brand_identity_status = COALESCE(?, brand_identity_status),
                brand_create_started_at = NULL, sent_brand = COALESCE(?, sent_brand)
          WHERE id = ?",
    )
    .bind(brand.brand_id.chars().take(64).collect::<String>())
    .bind(clip(&brand.tcr_brand_id, 32))
    .bind(clip(&brand.status, 32))
    .bind(clip(&brand.identity_status, 32))
    .bind(sent.map(|sent| serde_json::to_string(sent).unwrap_or_default()))
    .bind(id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Saves the campaign as Telnyx describes it (an expired one as
/// `TCR_EXPIRED`), and `sent` as what it was sent.
async fn save_campaign(
    pool: &MySqlPool,
    id: i32,
    campaign: &Campaign,
    sent: Option<&tendlc::TenDlcCampaignDraft>,
) -> sqlx::Result<()> {
    let status = if campaign.status.as_deref() == Some("EXPIRED") {
        Some("TCR_EXPIRED".to_string())
    } else {
        campaign.campaign_status.clone()
    };
    sqlx::query(
        "UPDATE tendlc_registrations
            SET telnyx_campaign_id = ?, tcr_campaign_id = COALESCE(?, tcr_campaign_id),
                campaign_status = COALESCE(?, campaign_status),
                sent_campaign = COALESCE(?, sent_campaign)
          WHERE id = ?",
    )
    .bind(campaign.campaign_id.chars().take(64).collect::<String>())
    .bind(
        campaign
            .tcr_campaign_id
            .as_deref()
            .map(|id| id.chars().take(32).collect::<String>()),
    )
    .bind(status.map(|status| status.chars().take(32).collect::<String>()))
    .bind(sent.map(|sent| serde_json::to_string(sent).unwrap_or_default()))
    .bind(id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Where Telnyx's statuses put a sent registration: refused (the brand, or
/// the campaign), suspended or expired, approved once the carriers
/// provisioned the campaign, else pending.
pub fn derived(row: &Row) -> TenDlcStatus {
    if row.brand_failed() {
        return TenDlcStatus::Rejected;
    }
    let status = row.campaign_status.as_deref();
    if row.telnyx_campaign_id.is_none() {
        return if status == Some(CAMPAIGN_REFUSED) {
            TenDlcStatus::Rejected
        } else {
            TenDlcStatus::Sent
        };
    }
    match status {
        Some(status) if CAMPAIGN_FAILED.contains(&status) => TenDlcStatus::Rejected,
        Some("TCR_SUSPENDED" | "TCR_EXPIRED") => TenDlcStatus::Suspended,
        Some("MNO_PROVISIONED") => TenDlcStatus::Approved,
        _ => TenDlcStatus::CampaignPending,
    }
}

/// Moves a sent registration to where Telnyx's statuses put it (once), and
/// tells the company when it is approved, rejected or suspended. A draft or
/// a ready one stays: the company is working on it.
async fn settle(state: &AppState, row: &Row) -> sqlx::Result<Row> {
    let pool = &state.pool;
    let row = reload(pool, row).await?;
    if matches!(row.status, TenDlcStatus::Draft | TenDlcStatus::Ready) {
        return Ok(row);
    }
    let target = derived(&row);
    if target != row.status && move_status(pool, row.id, row.status, target, None).await? {
        let outcome = match target {
            TenDlcStatus::Approved => Some(Outcome::Approved),
            TenDlcStatus::Rejected => Some(Outcome::Rejected),
            TenDlcStatus::Suspended => Some(Outcome::Suspended),
            _ => None,
        };
        if let Some(outcome) = outcome {
            notify_company(state, &row, outcome).await?;
        }
        return reload(pool, &row).await;
    }
    Ok(row)
}

/// Telnyx's reasons about a brand that failed (or whose identity TCR could
/// not verify): its feedback and its failure reasons. None otherwise, or
/// when Telnyx cannot be read just now.
async fn brand_reasons(state: &AppState, brand: &Brand) -> Vec<TenDlcFeedback> {
    let failed = brand.status.as_deref() == Some(BRAND_FAILED)
        || brand.identity_status.as_deref() == Some("UNVERIFIED");
    if !failed {
        return Vec::new();
    }
    let mut items: Vec<TenDlcFeedback> = match state.telnyx.brand_feedback(&brand.brand_id).await {
        Ok(found) => found
            .into_iter()
            .map(|item| TenDlcFeedback {
                title: item.display_name.unwrap_or(item.id),
                description: item.description.unwrap_or_default(),
                fields: item.fields,
            })
            .collect(),
        Err(err) => {
            tracing::warn!(error = %err, "brand feedback not read");
            Vec::new()
        }
    };
    if let Some(reason) = &brand.failure_reasons
        && !items.iter().any(|item| &item.description == reason)
    {
        items.push(TenDlcFeedback {
            title: "Brand registration".into(),
            description: reason.clone(),
            fields: Vec::new(),
        });
    }
    items
}

/// The brand Telnyx lists that is this form's (an earlier create whose
/// answer never came).
fn same_brand(brand: &Brand, draft: &tendlc::TenDlcBrandDraft) -> bool {
    let same = |theirs: &Option<String>, ours: &str| {
        theirs
            .as_deref()
            .is_none_or(|theirs| theirs.trim().eq_ignore_ascii_case(ours.trim()))
    };
    brand
        .display_name
        .as_deref()
        .is_some_and(|name| name.trim().eq_ignore_ascii_case(draft.display_name.trim()))
        && same(&brand.company_name, &draft.company_name)
        && same(&brand.email, &draft.email)
}

/// Creates the registration's campaign (the lock is held): one an earlier
/// try created (same `referenceId`) is taken instead. A refusal is saved as
/// the campaign's (`REFUSED`, with Telnyx's reason) and Telnyx not answering
/// as the last error; both come back as the error.
async fn create_campaign(
    state: &AppState,
    row: &Row,
    config: &CompanyConfig,
) -> Result<(), RegistryError> {
    let pool = &state.pool;
    let Some(brand_id) = row.telnyx_brand_id.as_deref() else {
        return Ok(());
    };
    let draft = row.draft();
    let reference = format!(
        "granite-c{}-r{}-{}",
        row.company_id, row.id, row.campaign_seq
    );
    let earlier = state
        .telnyx
        .brand_campaigns(brand_id)
        .await
        .map_err(|err| failed(&err, "campaign"));
    let earlier = match earlier {
        Ok(campaigns) => campaigns
            .into_iter()
            .find(|campaign| campaign.reference_id.as_deref() == Some(reference.as_str())),
        Err(err) => {
            set_error(pool, row.id, &err.to_string()).await?;
            return Err(err);
        }
    };
    let adopted = earlier.is_some();
    let campaign = match earlier {
        Some(campaign) => campaign,
        None => {
            let body = campaign_body(
                &draft.campaign,
                brand_id,
                &reference,
                &webhook_url(state, config),
            );
            match state.telnyx.create_campaign(&body).await {
                Ok(campaign) => campaign,
                Err(err) => {
                    let mapped = failed(&err, "campaign");
                    if let RegistryError::Refused(reason) = &mapped {
                        sqlx::query(
                            "UPDATE tendlc_registrations
                                SET campaign_status = ?, sent_campaign = ? WHERE id = ?",
                        )
                        .bind(CAMPAIGN_REFUSED)
                        .bind(serde_json::to_string(&draft.campaign).unwrap_or_default())
                        .bind(row.id)
                        .execute(pool)
                        .await?;
                        add_campaign_feedback(
                            pool,
                            row,
                            &[TenDlcFeedback {
                                title: "Campaign".into(),
                                description: err.detail().unwrap_or(reason.as_str()).to_string(),
                                fields: Vec::new(),
                            }],
                        )
                        .await?;
                    }
                    set_error(pool, row.id, &mapped.to_string()).await?;
                    return Err(mapped);
                }
            }
        }
    };
    save_campaign(pool, row.id, &campaign, Some(&draft.campaign)).await?;
    sqlx::query(
        "UPDATE tendlc_registrations SET last_error = NULL, last_error_at = NULL WHERE id = ?",
    )
    .bind(row.id)
    .execute(pool)
    .await?;
    audit(
        pool,
        row.company_id,
        None,
        "tendlc_campaign_created",
        row.id,
        &json!({ "campaignId": campaign.campaign_id, "referenceId": reference, "found": adopted }),
    )
    .await?;
    Ok(())
}

/// After the brand was sent (the lock is held): a registered brand gets
/// its campaign, a refused one Telnyx's reasons; then the registration
/// moves on.
async fn after_brand(
    state: &AppState,
    row: &Row,
    config: &CompanyConfig,
    brand: Option<&Brand>,
) -> sqlx::Result<Row> {
    let pool = &state.pool;
    if let Some(brand) = brand {
        let reasons = brand_reasons(state, brand).await;
        sqlx::query("UPDATE tendlc_registrations SET brand_feedback = ? WHERE id = ?")
            .bind(feedback_json(&reasons))
            .bind(row.id)
            .execute(pool)
            .await?;
    }
    let row = reload(pool, row).await?;
    if row.status == TenDlcStatus::Sent
        && row.telnyx_campaign_id.is_none()
        && row.campaign_status.as_deref() != Some(CAMPAIGN_REFUSED)
        && row.brand_usable()
    {
        match create_campaign(state, &row, config).await {
            Ok(()) | Err(RegistryError::Refused(_) | RegistryError::Unanswered(_)) => {}
            Err(RegistryError::Database(err)) => return Err(err),
            Err(err) => tracing::warn!(kind = err.kind(), "campaign not created"),
        }
    }
    settle(state, &row).await
}

/// Send a company's registration to Telnyx (the first time)
///
/// See the module docs. 400 when the form is not complete, the company has
/// no Telnyx setup, or Telnyx refused the brand (nothing was made); 409
/// when it was sent already or another send is running; 502 when Telnyx
/// did not answer about the brand (sending again looks it up first, so no
/// second brand is made). Once the brand is made, the answer is the
/// registration: its campaign, or why Telnyx refused it, or the last error
/// when Telnyx did not answer (the phone job tries again).
pub async fn send(state: &AppState, company_id: i32, actor: i32) -> Result<Row, RegistryError> {
    let pool = &state.pool;
    let row = registration(pool, company_id).await?;
    if row.telnyx_brand_id.is_some() {
        return Err(RegistryError::Conflict(
            "It was sent already. After a rejection, send it again.".into(),
        ));
    }
    if !row.status.editable() {
        return Err(RegistryError::Conflict("It is with Telnyx already.".into()));
    }
    let draft = row.draft();
    complete(&draft, state.ai.is_enabled())?;
    if let Some(why) = ai_check::blocks_sending(state, &row, actor).await? {
        return Err(RegistryError::Conflict(why));
    }
    let config = setup(state, company_id).await?;
    if !lock(pool, row.id).await? {
        return Err(RegistryError::Conflict(
            "A send is in progress. Try again in a minute.".into(),
        ));
    }
    let result = first_send(state, &row, &config, &draft, actor).await;
    unlock(pool, row.id).await?;
    result
}

async fn first_send(
    state: &AppState,
    row: &Row,
    config: &CompanyConfig,
    draft: &TenDlcDraft,
    actor: i32,
) -> Result<Row, RegistryError> {
    let pool = &state.pool;
    // A create whose answer never came may have made the brand.
    let found = if row.brand_create_started_at.is_some() {
        let brands = state
            .telnyx
            .find_brands(&draft.brand.display_name)
            .await
            .map_err(|err| failed(&err, "brand"))?;
        brands
            .into_iter()
            .find(|brand| same_brand(brand, &draft.brand))
    } else {
        None
    };
    let found_earlier = found.is_some();
    let brand = match found {
        Some(brand) => brand,
        None => {
            sqlx::query(
                "UPDATE tendlc_registrations SET brand_create_started_at = UTC_TIMESTAMP()
                  WHERE id = ?",
            )
            .bind(row.id)
            .execute(pool)
            .await?;
            match state
                .telnyx
                .create_brand(&brand_body(&draft.brand, &webhook_url(state, config)))
                .await
            {
                Ok(brand) => brand,
                Err(err) => {
                    let mapped = failed(&err, "brand");
                    if err.refused() || matches!(err, TenDlcError::NotConfigured) {
                        // Nothing was made: the next send creates it.
                        sqlx::query(
                            "UPDATE tendlc_registrations SET brand_create_started_at = NULL
                              WHERE id = ?",
                        )
                        .bind(row.id)
                        .execute(pool)
                        .await?;
                    }
                    set_error(pool, row.id, &mapped.to_string()).await?;
                    return Err(mapped);
                }
            }
        }
    };
    save_brand(pool, row.id, &brand, Some(&draft.brand)).await?;
    sqlx::query(
        "UPDATE tendlc_registrations
            SET sent_at = COALESCE(sent_at, UTC_TIMESTAMP()),
                sent_by_user_id = COALESCE(sent_by_user_id, ?),
                brand_feedback = NULL, campaign_feedback = NULL,
                last_error = NULL, last_error_at = NULL
          WHERE id = ?",
    )
    .bind(actor)
    .bind(row.id)
    .execute(pool)
    .await?;
    move_status(pool, row.id, row.status, TenDlcStatus::Sent, Some(actor)).await?;
    audit(
        pool,
        row.company_id,
        Some(actor),
        "tendlc_sent",
        row.id,
        &json!({ "brandId": brand.brand_id, "found": found_earlier }),
    )
    .await?;
    let row = reload(pool, row).await?;
    Ok(after_brand(state, &row, config, Some(&brand)).await?)
}

/// What a re-send did, for the audit log.
#[derive(Debug, Default)]
struct Resent {
    brand: bool,
    campaign_updated: bool,
    appealed: bool,
    new_campaign: bool,
}

/// Send a registration again after Telnyx or a carrier refused it
///
/// See the module docs: the brand when it changed (or was refused), then
/// for a refused campaign its samples, message flow and help message when
/// they changed and the appeal when one is written; a new campaign when TCR
/// refused the first (or Telnyx refused to create one). 400 when the form
/// is not complete, nothing changed, or Telnyx refused the first change;
/// 409 when it was never sent or Telnyx is reviewing it. A refusal after a
/// first change was taken is saved on the registration (its feedback) and
/// the answer is the registration.
pub async fn resend(
    state: &AppState,
    company_id: i32,
    actor: i32,
    appeal: Option<&str>,
) -> Result<Row, RegistryError> {
    let pool = &state.pool;
    let row = registration(pool, company_id).await?;
    if row.telnyx_brand_id.is_none() {
        return Err(RegistryError::Conflict(
            "It has not been sent yet: send it to Telnyx first.".into(),
        ));
    }
    if !row.status.editable() {
        return Err(RegistryError::Conflict(
            "Telnyx is reviewing it (or approved it): there is nothing to send again.".into(),
        ));
    }
    let draft = row.draft();
    complete(&draft, state.ai.is_enabled())?;
    if let Some(why) = ai_check::blocks_sending(state, &row, actor).await? {
        return Err(RegistryError::Conflict(why));
    }
    let config = setup(state, company_id).await?;
    if !lock(pool, row.id).await? {
        return Err(RegistryError::Conflict(
            "A send is in progress. Try again in a minute.".into(),
        ));
    }
    let result = send_again(state, &row, &config, &draft, actor, appeal).await;
    unlock(pool, row.id).await?;
    result
}

async fn send_again(
    state: &AppState,
    row: &Row,
    config: &CompanyConfig,
    draft: &TenDlcDraft,
    actor: i32,
    appeal: Option<&str>,
) -> Result<Row, RegistryError> {
    let pool = &state.pool;
    let telnyx = &state.telnyx;
    let hook = webhook_url(state, config);
    let mut done = Resent::default();
    let mut sent_brand = None;
    // A refusal after something was taken is kept on the registration.
    let mut refusal: Option<RegistryError> = None;
    let brand_id = row.telnyx_brand_id.clone().unwrap_or_default();
    let brand_changed = row
        .sent_brand()
        .is_none_or(|sent| tendlc::brand_as_sent(&sent) != tendlc::brand_as_sent(&draft.brand));
    if brand_changed || row.brand_failed() {
        match telnyx
            .update_brand(&brand_id, &brand_body(&draft.brand, &hook))
            .await
        {
            Ok(brand) => {
                save_brand(pool, row.id, &brand, Some(&draft.brand)).await?;
                sent_brand = Some(brand);
                done.brand = true;
            }
            Err(err) => {
                let mapped = failed(&err, "brand");
                set_error(pool, row.id, &mapped.to_string()).await?;
                return Err(mapped);
            }
        }
    }
    let row = reload(pool, row).await?;
    let changed = |done: &Resent| done.brand || done.campaign_updated || done.appealed;
    match (
        row.telnyx_campaign_id.clone(),
        row.campaign_status.as_deref(),
    ) {
        (Some(_), Some("TCR_FAILED")) => {
            // TCR refused that campaign for good: a new one replaces it.
            sqlx::query(
                "UPDATE tendlc_registrations
                    SET campaign_seq = campaign_seq + 1, telnyx_campaign_id = NULL,
                        tcr_campaign_id = NULL, campaign_status = NULL, sent_campaign = NULL
                  WHERE id = ?",
            )
            .bind(row.id)
            .execute(pool)
            .await?;
            done.new_campaign = true;
        }
        (None, _) => {
            sqlx::query("UPDATE tendlc_registrations SET campaign_status = NULL WHERE id = ?")
                .bind(row.id)
                .execute(pool)
                .await?;
            done.new_campaign = true;
        }
        (Some(campaign_id), Some(status)) if CAMPAIGN_FAILED.contains(&status) => {
            // The appeal first: Telnyx takes one only while the campaign is
            // refused, and a change puts it back in review.
            if let Some(reason) = appeal {
                match telnyx.appeal_campaign(&campaign_id, reason).await {
                    Ok(()) => done.appealed = true,
                    Err(err) if changed(&done) => refusal = Some(failed(&err, "appeal")),
                    Err(err) => {
                        let mapped = failed(&err, "appeal");
                        set_error(pool, row.id, &mapped.to_string()).await?;
                        return Err(mapped);
                    }
                }
            }
            let sent = row.sent_campaign().unwrap_or_default();
            if refusal.is_none() && editable_fields_changed(&sent, &draft.campaign) {
                match telnyx
                    .update_campaign(&campaign_id, &campaign_update_body(&draft.campaign, &hook))
                    .await
                {
                    Ok(campaign) => {
                        save_campaign(pool, row.id, &campaign, Some(&draft.campaign)).await?;
                        done.campaign_updated = true;
                    }
                    Err(err) if changed(&done) => refusal = Some(failed(&err, "campaign changes")),
                    Err(err) => {
                        let mapped = failed(&err, "campaign changes");
                        set_error(pool, row.id, &mapped.to_string()).await?;
                        return Err(mapped);
                    }
                }
            }
            if done.campaign_updated || done.appealed {
                // Telnyx puts a changed or appealed campaign back in review
                // (TCR_ACCEPTED); the next read says where it is.
                sqlx::query(
                    "UPDATE tendlc_registrations SET campaign_status = 'TCR_ACCEPTED'
                      WHERE id = ? AND campaign_status IN
                            ('TELNYX_FAILED', 'MNO_REJECTED', 'MNO_PROVISIONING_FAILED')",
                )
                .bind(row.id)
                .execute(pool)
                .await?;
            }
        }
        // The campaign is fine (in review or approved): the brand was refused.
        _ => {}
    }
    if !changed(&done) && !done.new_campaign {
        return Err(RegistryError::Invalid(
            "Nothing to send again: change the details, or write what was fixed so Telnyx \
             looks again (appeal)."
                .into(),
        ));
    }
    sqlx::query(
        "UPDATE tendlc_registrations
            SET resent_at = UTC_TIMESTAMP(), brand_feedback = NULL, campaign_feedback = NULL,
                last_error = NULL, last_error_at = NULL
          WHERE id = ?",
    )
    .bind(row.id)
    .execute(pool)
    .await?;
    let row = reload(pool, &row).await?;
    if let Some(err) = &refusal {
        set_error(pool, row.id, &err.to_string()).await?;
        add_campaign_feedback(
            pool,
            &row,
            &[TenDlcFeedback {
                title: "Campaign".into(),
                description: err.to_string(),
                fields: Vec::new(),
            }],
        )
        .await?;
    }
    move_status(pool, row.id, row.status, TenDlcStatus::Sent, Some(actor)).await?;
    audit(
        pool,
        row.company_id,
        Some(actor),
        "tendlc_resent",
        row.id,
        &json!({ "brand": done.brand, "campaignUpdated": done.campaign_updated,
                 "appealed": done.appealed, "newCampaign": done.new_campaign }),
    )
    .await?;
    let row = reload(pool, &row).await?;
    Ok(after_brand(state, &row, config, sent_brand.as_ref()).await?)
}

/// Read a registration again from Telnyx
///
/// See the module docs. With `continue_send`, a sent registration whose
/// brand is now registered gets its campaign (when no send holds the lock).
pub async fn refresh(
    state: &AppState,
    row: &Row,
    continue_send: bool,
) -> Result<Row, RegistryError> {
    let pool = &state.pool;
    let telnyx = &state.telnyx;
    let brand = match row.telnyx_brand_id.as_deref() {
        Some(id) => Some(
            telnyx
                .get_brand(id)
                .await
                .map_err(|err| failed(&err, "brand"))?,
        ),
        None => None,
    };
    let campaign = match row.telnyx_campaign_id.as_deref() {
        Some(id) => Some(
            telnyx
                .get_campaign(id)
                .await
                .map_err(|err| failed(&err, "campaign"))?,
        ),
        None => None,
    };
    if let Some(brand) = &brand {
        save_brand(pool, row.id, brand, None).await?;
        let reasons = brand_reasons(state, brand).await;
        sqlx::query("UPDATE tendlc_registrations SET brand_feedback = ? WHERE id = ?")
            .bind(feedback_json(&reasons))
            .bind(row.id)
            .execute(pool)
            .await?;
    }
    if let Some(campaign) = &campaign {
        save_campaign(pool, row.id, campaign, None).await?;
        if let Some(reason) = &campaign.failure_reasons {
            add_campaign_feedback(
                pool,
                row,
                &[TenDlcFeedback {
                    title: "Campaign".into(),
                    description: reason.clone(),
                    fields: Vec::new(),
                }],
            )
            .await?;
        }
    }
    sqlx::query("UPDATE tendlc_registrations SET refreshed_at = UTC_TIMESTAMP() WHERE id = ?")
        .bind(row.id)
        .execute(pool)
        .await?;
    let row = reload(pool, row).await?;
    let campaign_due = row.status == TenDlcStatus::Sent
        && row.telnyx_campaign_id.is_none()
        && row.campaign_status.as_deref() != Some(CAMPAIGN_REFUSED)
        && row.brand_usable();
    if continue_send && campaign_due && lock(pool, row.id).await? {
        let created = match company::load(pool, row.company_id).await? {
            Some(config) => create_campaign(state, &row, &config).await,
            None => Ok(()),
        };
        unlock(pool, row.id).await?;
        if let Err(RegistryError::Database(err)) = created {
            return Err(err.into());
        }
    }
    Ok(settle(state, &row).await?)
}

/// What an assignment did.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct AssignRun {
    /// Numbers Telnyx took (assigned, or being assigned).
    pub assigned: usize,
    /// Numbers Telnyx refused, or did not answer about.
    pub failed: usize,
    /// Numbers assigned to the campaign already.
    pub skipped: usize,
}

/// Saves a number's assignment.
async fn save_number(
    pool: &MySqlPool,
    row: &Row,
    e164: &str,
    campaign_id: &str,
    status: &str,
    error: Option<&str>,
    actor: Option<i32>,
) -> sqlx::Result<()> {
    sqlx::query(
        "INSERT INTO tendlc_numbers
           (company_id, registration_id, e164, campaign_id, status, error, assigned_by_user_id,
            refreshed_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, UTC_TIMESTAMP()) AS new
         ON DUPLICATE KEY UPDATE registration_id = new.registration_id,
            campaign_id = new.campaign_id, status = new.status, error = new.error,
            assigned_by_user_id = COALESCE(new.assigned_by_user_id, tendlc_numbers.assigned_by_user_id),
            refreshed_at = new.refreshed_at",
    )
    .bind(row.company_id)
    .bind(row.id)
    .bind(e164)
    .bind(campaign_id.chars().take(64).collect::<String>())
    .bind(status.chars().take(32).collect::<String>())
    .bind(error.map(|error| error.chars().take(300).collect::<String>()))
    .bind(actor)
    .execute(pool)
    .await?;
    Ok(())
}

/// Assign the company's numbers to its approved campaign
///
/// Every active number of the company not assigned to the campaign yet
/// (`tendlc_numbers` keeps Telnyx's answer for each). 409 until the
/// carriers approved the campaign or while another send runs; 400 when the
/// company has no active numbers.
pub async fn assign_numbers(
    state: &AppState,
    company_id: i32,
    actor: i32,
) -> Result<(Row, AssignRun), RegistryError> {
    let pool = &state.pool;
    let row = registration(pool, company_id).await?;
    let campaign_id = match (&row.telnyx_campaign_id, row.status) {
        (Some(id), TenDlcStatus::Approved) => id.clone(),
        _ => {
            return Err(RegistryError::Conflict(
                "Numbers are assigned once the carriers approve the campaign.".into(),
            ));
        }
    };
    if !state.telnyx.is_enabled() {
        return Err(RegistryError::NotConfigured);
    }
    let numbers: Vec<String> = sqlx::query_scalar(
        "SELECT e164 FROM telnyx_numbers WHERE company_id = ? AND active = 1 ORDER BY id",
    )
    .bind(company_id)
    .fetch_all(pool)
    .await?;
    if numbers.is_empty() {
        return Err(RegistryError::Invalid(
            "The company has no active numbers on Granite Phone. Import them on its Phone page \
             first."
                .into(),
        ));
    }
    if !lock(pool, row.id).await? {
        return Err(RegistryError::Conflict(
            "A send is in progress. Try again in a minute.".into(),
        ));
    }
    let run = assign_locked(state, &row, &campaign_id, &numbers, actor).await;
    unlock(pool, row.id).await?;
    let run = run?;
    audit(
        pool,
        company_id,
        Some(actor),
        "tendlc_numbers_assigned",
        row.id,
        &json!({ "assigned": run.assigned, "failed": run.failed, "skipped": run.skipped }),
    )
    .await?;
    Ok((reload(pool, &row).await?, run))
}

async fn assign_locked(
    state: &AppState,
    row: &Row,
    campaign_id: &str,
    numbers: &[String],
    actor: i32,
) -> Result<AssignRun, RegistryError> {
    let pool = &state.pool;
    let existing: HashMap<String, (String, String)> =
        sqlx::query_as::<_, (String, String, String)>(
            "SELECT e164, campaign_id, status FROM tendlc_numbers WHERE company_id = ?",
        )
        .bind(row.company_id)
        .fetch_all(pool)
        .await?
        .into_iter()
        .map(|(e164, campaign, status)| (e164, (campaign, status)))
        .collect();
    let mut run = AssignRun::default();
    for e164 in numbers {
        if existing.get(e164).is_some_and(|(campaign, status)| {
            campaign == campaign_id && matches!(status.as_str(), "ASSIGNED" | "PENDING_ASSIGNMENT")
        }) {
            run.skipped += 1;
            continue;
        }
        match state.telnyx.assign_number_campaign(e164, campaign_id).await {
            Ok(assignment) => {
                let status = assignment
                    .assignment_status
                    .unwrap_or_else(|| "PENDING_ASSIGNMENT".into());
                if status.starts_with("FAILED") {
                    run.failed += 1;
                } else {
                    run.assigned += 1;
                }
                save_number(
                    pool,
                    row,
                    e164,
                    campaign_id,
                    &status,
                    assignment.failure_reasons.as_deref(),
                    Some(actor),
                )
                .await?;
            }
            Err(TenDlcError::NotConfigured) => return Err(RegistryError::NotConfigured),
            Err(err) => {
                tracing::warn!(error = %err, "number not assigned to the campaign");
                run.failed += 1;
                let reason = err
                    .detail()
                    .map(str::to_string)
                    .unwrap_or_else(|| "Telnyx did not answer. Assign the numbers again.".into());
                save_number(
                    pool,
                    row,
                    e164,
                    campaign_id,
                    "FAILED_ASSIGNMENT",
                    Some(&reason),
                    Some(actor),
                )
                .await?;
            }
        }
    }
    Ok(run)
}

/// Updates a number's assignment from a Telnyx notification (an unknown
/// number changes nothing).
pub async fn number_status(
    pool: &MySqlPool,
    company_id: i32,
    e164: &str,
    status: &str,
    error: Option<&str>,
) -> sqlx::Result<bool> {
    Ok(sqlx::query(
        "UPDATE tendlc_numbers SET status = ?, error = ?, refreshed_at = UTC_TIMESTAMP()
          WHERE company_id = ? AND e164 = ?",
    )
    .bind(status)
    .bind(error.map(|error| error.chars().take(300).collect::<String>()))
    .bind(company_id)
    .bind(e164)
    .execute(pool)
    .await?
    .rows_affected()
        > 0)
}

/// One of the company's numbers, with its assignment.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct NumberRow {
    pub e164: String,
    pub label: String,
    /// Telnyx's assignment status; `None` while never assigned.
    pub status: Option<String>,
    pub error: Option<String>,
    pub updated_at: Option<DateTime<Utc>>,
}

/// The company's active numbers and their assignments, in number order.
pub async fn numbers(pool: &MySqlPool, company_id: i32) -> sqlx::Result<Vec<NumberRow>> {
    sqlx::query_as(
        "SELECT n.e164, n.label, t.status, t.error, t.updated_at
           FROM telnyx_numbers n
           LEFT JOIN tendlc_numbers t ON t.company_id = n.company_id AND t.e164 = n.e164
          WHERE n.company_id = ? AND n.active = 1
          ORDER BY n.id",
    )
    .bind(company_id)
    .fetch_all(pool)
    .await
}

/// Takes a Telnyx 10DLC notification once: `Some(key)` when it is new (give
/// it back with [`release_event`] when handling it fails), `None` when it
/// was taken before. One without an id is always new.
pub async fn claim_event(
    pool: &MySqlPool,
    company_id: i32,
    event_id: Option<&str>,
    event_type: &str,
) -> sqlx::Result<Option<String>> {
    let key = match event_id.map(str::trim).filter(|id| !id.is_empty()) {
        Some(id) => sha256_hex(&format!("tendlc|{id}")),
        None => sha256_hex(&uuid::Uuid::new_v4().to_string()),
    };
    let inserted = sqlx::query(
        "INSERT INTO tendlc_webhook_events (company_id, event_type, dedupe_key) VALUES (?, ?, ?)",
    )
    .bind(company_id)
    .bind(event_type.chars().take(64).collect::<String>())
    .bind(&key)
    .execute(pool)
    .await;
    match inserted {
        Ok(_) => Ok(Some(key)),
        Err(err)
            if err
                .as_database_error()
                .is_some_and(sqlx::error::DatabaseError::is_unique_violation) =>
        {
            Ok(None)
        }
        Err(err) => Err(err),
    }
}

/// Gives a notification back, so Telnyx's retry is handled.
pub async fn release_event(pool: &MySqlPool, key: &str) -> sqlx::Result<()> {
    sqlx::query("DELETE FROM tendlc_webhook_events WHERE dedupe_key = ?")
        .bind(key)
        .execute(pool)
        .await?;
    Ok(())
}

/// What the company is told.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    Approved,
    Rejected,
    Suspended,
}

/// The company's admins and Managers, by id.
async fn company_people(pool: &MySqlPool, company_id: i32) -> sqlx::Result<Vec<i32>> {
    let mut people = company_admins(pool, company_id, None).await?;
    let managers: Vec<i32> = sqlx::query_scalar(
        "SELECT u.id FROM users_positions up JOIN users u ON u.id = up.user_id AND u.is_deleted = 0
          WHERE up.company_id = ? AND up.position_id = ? ORDER BY u.id",
    )
    .bind(company_id)
    .bind(MANAGER_POSITION)
    .fetch_all(pool)
    .await?;
    for manager in managers {
        if !people.contains(&manager) {
            people.push(manager);
        }
    }
    Ok(people)
}

async fn bell(
    pool: &MySqlPool,
    people: &[i32],
    message: &str,
    kind: &str,
    actor: &str,
) -> sqlx::Result<()> {
    if people.is_empty() {
        return Ok(());
    }
    let message: String = message.chars().take(255).collect();
    let actor: String = actor.chars().take(100).collect();
    let mut tx = pool.begin().await?;
    for person in people {
        sqlx::query(
            "INSERT INTO notifications (user_id, message, notification_type, actor_name, due_at)
             VALUES (?, ?, ?, ?, NOW())",
        )
        .bind(person)
        .bind(&message)
        .bind(kind)
        .bind(&actor)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await
}

/// Tells the company's admins and Managers (bell) that the registration was
/// approved, rejected or suspended.
async fn notify_company(state: &AppState, row: &Row, outcome: Outcome) -> sqlx::Result<()> {
    let message = match outcome {
        Outcome::Approved => {
            "Your texting registration (10DLC) is approved: carriers take your business texts \
             once your numbers are assigned to it."
        }
        Outcome::Rejected => {
            "Your texting registration (10DLC) needs changes. Open Admin → Phone to see why, fix \
             it and mark it ready again."
        }
        Outcome::Suspended => {
            "Your texting campaign (10DLC) was suspended or expired. Open Admin → Phone for \
             details."
        }
    };
    let people = company_people(&state.pool, row.company_id).await?;
    bell(&state.pool, &people, message, STATUS_NOTIFICATION, ACTOR).await
}

/// Tells the superusers (bell) that a company marked its registration
/// ready.
pub async fn notify_ready(state: &AppState, row: &Row, by: &str) -> sqlx::Result<()> {
    let name: String = row.company_name.trim().chars().take(100).collect();
    notify_superusers(
        state,
        &format!("{name} marked its texting registration (10DLC) ready to send to Telnyx."),
        by,
    )
    .await
}

/// Tells the superusers (bell) about a registration ready to send.
pub async fn notify_superusers(state: &AppState, message: &str, by: &str) -> sqlx::Result<()> {
    let superusers: Vec<i32> = sqlx::query_scalar(
        "SELECT id FROM users WHERE COALESCE(is_superuser, 0) <> 0 AND is_deleted = 0 ORDER BY id",
    )
    .fetch_all(&state.pool)
    .await?;
    bell(&state.pool, &superusers, message, READY_NOTIFICATION, by).await
}

/// What the job's 10DLC step did.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct JobRun {
    /// Registrations read again from Telnyx.
    pub refreshed: usize,
    /// Number assignments read again from Telnyx.
    pub numbers_refreshed: usize,
}

/// The phone job's 10DLC step: reads sent registrations from Telnyx again
/// (every 10 minutes while the brand registers, hourly while the campaign
/// is reviewed, daily once approved; Telnyx's notifications are the fast
/// path), giving a sent one its campaign once the brand is registered, then
/// the number assignments still pending (every 15 minutes). A few each
/// run, none started after `deadline`.
pub async fn run_job(state: &AppState, deadline: Instant) -> sqlx::Result<JobRun> {
    let mut run = JobRun::default();
    if !state.telnyx.is_enabled() {
        return Ok(run);
    }
    let pool = &state.pool;
    let due: Vec<(i32, i32, Option<DateTime<Utc>>)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT r.id, r.company_id, r.refreshed_at FROM tendlc_registrations r
               JOIN company c ON c.id = r.company_id AND c.deleted_at IS NULL
              WHERE r.status IN ('sent', 'campaign_pending', 'approved')
                AND (r.refreshed_at IS NULL OR r.refreshed_at < UTC_TIMESTAMP() - INTERVAL
                     (CASE r.status WHEN 'sent' THEN 10 WHEN 'campaign_pending' THEN 60
                                    ELSE 1440 END) MINUTE)
              ORDER BY r.refreshed_at IS NOT NULL, r.refreshed_at, r.id LIMIT {JOB_BATCH}"
    )))
    .fetch_all(pool)
    .await?;
    for (id, company_id, seen) in due {
        if Instant::now() >= deadline {
            return Ok(run);
        }
        // Claimed by moving `refreshed_at`: an overlapping run skips it, and
        // a failure waits for the next interval.
        let claimed = sqlx::query(
            "UPDATE tendlc_registrations SET refreshed_at = UTC_TIMESTAMP()
              WHERE id = ? AND refreshed_at <=> ?",
        )
        .bind(id)
        .bind(seen)
        .execute(pool)
        .await?
        .rows_affected()
            == 1;
        if !claimed {
            continue;
        }
        let Some(row) = tendlc::load(pool, company_id).await? else {
            continue;
        };
        match refresh(state, &row, true).await {
            Ok(_) => run.refreshed += 1,
            Err(RegistryError::Database(err)) => return Err(err),
            Err(err) => tracing::warn!(kind = err.kind(), registration = id, "10DLC not refreshed"),
        }
    }
    let pending: Vec<(i64, i32, String, Option<DateTime<Utc>>)> =
        sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT id, company_id, e164, refreshed_at FROM tendlc_numbers
              WHERE status IN ('PENDING_ASSIGNMENT', 'PENDING_UNASSIGNMENT')
                AND (refreshed_at IS NULL OR refreshed_at < UTC_TIMESTAMP() - INTERVAL 15 MINUTE)
              ORDER BY refreshed_at IS NOT NULL, refreshed_at, id LIMIT {JOB_NUMBER_BATCH}"
        )))
        .fetch_all(pool)
        .await?;
    for (id, company_id, e164, seen) in pending {
        if Instant::now() >= deadline {
            break;
        }
        let claimed = sqlx::query(
            "UPDATE tendlc_numbers SET refreshed_at = UTC_TIMESTAMP()
              WHERE id = ? AND refreshed_at <=> ?",
        )
        .bind(id)
        .bind(seen)
        .execute(pool)
        .await?
        .rows_affected()
            == 1;
        if !claimed {
            continue;
        }
        match state.telnyx.number_campaign(&e164).await {
            Ok(Some(assignment)) => {
                if let Some(status) = assignment.assignment_status.as_deref() {
                    number_status(
                        pool,
                        company_id,
                        &e164,
                        status,
                        assignment.failure_reasons.as_deref(),
                    )
                    .await?;
                }
                run.numbers_refreshed += 1;
            }
            Ok(None) => {
                number_status(
                    pool,
                    company_id,
                    &e164,
                    "FAILED_ASSIGNMENT",
                    Some("Telnyx has no assignment for this number. Assign the numbers again."),
                )
                .await?;
                run.numbers_refreshed += 1;
            }
            Err(err) => tracing::warn!(error = %err, "number assignment not refreshed"),
        }
    }
    Ok(run)
}

/// Telnyx's reasons in a 10DLC notification's payload (`reasons[]` with
/// `description` and `fields`, or the `description` of a review that
/// refused it), masked.
pub fn payload_feedback(payload: &Value) -> Vec<TenDlcFeedback> {
    let kind = payload["type"].as_str().unwrap_or_default();
    let status = payload["status"].as_str().unwrap_or_default();
    let mut items: Vec<TenDlcFeedback> = payload["reasons"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|reason| {
            Some(TenDlcFeedback {
                title: match kind {
                    "REGISTRATION" => "Registration".into(),
                    other => other.replace('_', " ").to_lowercase(),
                },
                description: crate::telnyx::tendlc::reason_text(&reason["description"])
                    .or_else(|| crate::telnyx::tendlc::reason_text(reason))?,
                fields: reason["fields"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|field| field.as_str().map(str::to_string))
                    .collect(),
            })
        })
        .collect();
    let refused = matches!(status, "REJECTED" | "failed" | "DORMANT");
    let title = match kind {
        "MNO_REVIEW" => Some("Carrier review"),
        "TELNYX_REVIEW" => Some("Telnyx review"),
        "TELNYX_EVENT" => Some("Telnyx"),
        _ => None,
    };
    if let (true, Some(title), Some(description)) = (
        refused,
        title,
        crate::telnyx::tendlc::reason_text(&payload["description"]),
    ) {
        items.push(TenDlcFeedback {
            title: title.into(),
            description,
            fields: Vec::new(),
        });
    }
    items
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notification_payloads_give_their_reasons() {
        let registration = json!({ "campaignId": "c-1", "status": "failed", "type": "REGISTRATION",
            "reasons": [{ "fields": ["sample1"], "description": "Sample lacks opt-out" }] });
        assert_eq!(
            payload_feedback(&registration),
            [TenDlcFeedback {
                title: "Registration".into(),
                description: "Sample lacks opt-out".into(),
                fields: vec!["sample1".into()],
            }]
        );
        let review = json!({ "campaignId": "c-1", "status": "REJECTED", "type": "MNO_REVIEW",
            "description": "Rejected: call +13175550100 is not valid" });
        assert_eq!(
            payload_feedback(&review),
            [TenDlcFeedback {
                title: "Carrier review".into(),
                description: "Rejected: call [number] is not valid".into(),
                fields: Vec::new(),
            }]
        );
        let accepted = json!({ "campaignId": "c-1", "status": "ACCEPTED", "type": "TELNYX_REVIEW",
            "description": "C6X6M95 approved by Telnyx" });
        assert!(payload_feedback(&accepted).is_empty());
    }
}
