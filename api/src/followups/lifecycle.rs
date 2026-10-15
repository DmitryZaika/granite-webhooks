//! Whether the follow-up sequence still runs, and whether to write the next
//! message at all. Port of `smsFlowAiSequenceLifecycle.ts`.
//!
//! The checks run in a fixed order on purpose: a terminal customer state
//! outranks every other signal, including CRM activity dates and an explicit
//! re-enroll.

use super::contact_resume::{AutomationContactStatus, ChannelPermission, ContactResumeMode};
use super::conversation::RepOwnedNextStep;
use super::dates::Instant;
use super::facts::LatestCustomerState;
use super::intent::{
    MeaningfulCustomerIntent, intent_stops_automated_pursuit, moved_forward_elsewhere,
};
use crate::str_enum;

/// The customer ended the pursuit: only a manual re-enable reopens it.
pub fn is_terminal_customer_state(state: Option<LatestCustomerState>) -> bool {
    state == Some(LatestCustomerState::Declined)
}

pub fn terminal_customer_stop_reason(intent: Option<&MeaningfulCustomerIntent>) -> &'static str {
    if moved_forward_elsewhere(intent.map_or("", |i| i.evidence.as_str())) {
        "CUSTOMER_MOVED_FORWARD_ELSEWHERE"
    } else {
        "CUSTOMER_DECLINED"
    }
}

str_enum! { pub enum FollowupSequenceStatus {
    Active = "ACTIVE", StoppedCustomerReplied = "STOPPED_CUSTOMER_REPLIED", StoppedOptOut = "STOPPED_OPT_OUT",
    StoppedTerminalDeal = "STOPPED_TERMINAL_DEAL", Paused = "PAUSED", Completed = "COMPLETED",
} }

str_enum! { pub enum TargetDealTerminal { Won = "won", Lost = "lost" } }

/// What [`infer_followup_sequence_status`] reads.
#[derive(Debug, Clone, Copy, Default)]
#[allow(clippy::struct_excessive_bools)]
pub struct SequenceStatusInputs {
    pub genuine_customer_reply_at: Option<Instant>,
    pub target_deal_terminal: Option<TargetDealTerminal>,
    pub opted_out: bool,
    pub explicit_reenroll: bool,
    pub customer_reply_handled: bool,
    pub crm_hold_until: Option<Instant>,
    pub now: Option<Instant>,
    pub last_outbound_at: Option<Instant>,
    pub last_conversational_rep_reply_at: Option<Instant>,
}

pub fn infer_followup_sequence_status(params: &SequenceStatusInputs) -> FollowupSequenceStatus {
    if params.opted_out {
        return FollowupSequenceStatus::StoppedOptOut;
    }
    if params.target_deal_terminal.is_some() {
        return FollowupSequenceStatus::StoppedTerminalDeal;
    }
    if let Some(reply_at) = params.genuine_customer_reply_at {
        let historical_reply_already_passed = params.explicit_reenroll
            && (params
                .last_conversational_rep_reply_at
                .is_some_and(|at| reply_at < at)
                || params.last_outbound_at.is_some_and(|at| reply_at < at));
        let new_sequence_after_handled_reply = params.explicit_reenroll
            && (params.customer_reply_handled || historical_reply_already_passed);
        if !new_sequence_after_handled_reply {
            return FollowupSequenceStatus::StoppedCustomerReplied;
        }
    }
    let now = params.now.unwrap_or_else(chrono::Utc::now);
    if params.crm_hold_until.is_some_and(|until| until > now) {
        return FollowupSequenceStatus::Paused;
    }
    FollowupSequenceStatus::Active
}

/// Write the next message, or not and why.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(tag = "decision", rename_all = "SCREAMING_SNAKE_CASE")]
pub enum FollowupGenerationDecision {
    Generate,
    DoNotGenerate { reason: String },
}

impl FollowupGenerationDecision {
    fn stop(reason: impl Into<String>) -> Self {
        Self::DoNotGenerate {
            reason: reason.into(),
        }
    }
}

/// What [`evaluate_followup_generation_decision`] reads.
#[derive(Debug, Clone, Default)]
#[allow(clippy::struct_excessive_bools)]
pub struct GenerationDecisionInputs<'a> {
    pub sequence_status: Option<FollowupSequenceStatus>,
    pub latest_intent: Option<&'a MeaningfulCustomerIntent>,
    pub target_deal_terminal: Option<TargetDealTerminal>,
    pub latest_customer_state: Option<LatestCustomerState>,
    pub explicit_reenroll: bool,
    pub customer_reply_handled: bool,
    pub validation_errors: Vec<String>,
    pub contact_resume_mode: Option<ContactResumeMode>,
    pub automation_contact_status: Option<AutomationContactStatus>,
    pub sms_available: Option<bool>,
    pub email_available: Option<bool>,
    pub customer_waiting_for_human_reply: bool,
    pub rep_owned_next_step: Option<RepOwnedNextStep>,
    pub sms_permission: Option<ChannelPermission>,
}

pub fn evaluate_followup_generation_decision(
    params: &GenerationDecisionInputs<'_>,
) -> FollowupGenerationDecision {
    use FollowupGenerationDecision as D;
    if is_terminal_customer_state(params.latest_customer_state) {
        return D::stop(terminal_customer_stop_reason(params.latest_intent));
    }
    if params.contact_resume_mode == Some(ContactResumeMode::CustomerInitiates)
        || params.automation_contact_status == Some(AutomationContactStatus::WaitForCustomer)
    {
        let no_text_wait = params.sms_permission == Some(ChannelPermission::Restricted)
            && params.email_available == Some(false);
        return D::stop(if no_text_wait {
            "CUSTOMER_REQUESTED_NO_TEXT_WAITING_FOR_CUSTOMER"
        } else {
            "CUSTOMER_WILL_RECONTACT"
        });
    }
    // An answered reply or a later promotion does not make a "not interested"
    // or "project completed" customer contactable again.
    if intent_stops_automated_pursuit(params.latest_intent) {
        return D::stop(format!(
            "CUSTOMER_INTENT_{}",
            params
                .latest_intent
                .map_or("UNKNOWN", |intent| intent.kind.as_str())
        ));
    }
    match params.target_deal_terminal {
        Some(TargetDealTerminal::Won) => return D::stop("TARGET_DEAL_CLOSED_WON"),
        Some(TargetDealTerminal::Lost) => return D::stop("TARGET_DEAL_CLOSED_LOST"),
        None => {}
    }
    let status = params.sequence_status;
    if status == Some(FollowupSequenceStatus::StoppedTerminalDeal) {
        return D::stop("TARGET_DEAL_CLOSED_WON");
    }
    if params.customer_waiting_for_human_reply {
        return D::stop("CUSTOMER_WAITING_FOR_HUMAN_REPLY");
    }
    // The next step belongs to a person; a sales sequence here reads as if
    // nobody is paying attention.
    if let Some(step) = params.rep_owned_next_step {
        return D::stop(format!("REP_OWNS_NEXT_STEP_{step}"));
    }
    if params.sms_available == Some(false) && params.email_available == Some(false) {
        return D::stop("NO_ALLOWED_AUTOMATED_CONTACT");
    }
    if status == Some(FollowupSequenceStatus::StoppedOptOut) {
        return D::stop("STOPPED_OPT_OUT");
    }
    let replied = status == Some(FollowupSequenceStatus::StoppedCustomerReplied);
    let new_sequence_after_handled_reply =
        params.explicit_reenroll && (params.customer_reply_handled || !replied);
    if replied && !new_sequence_after_handled_reply {
        return D::stop("STOPPED_CUSTOMER_REPLIED");
    }
    if !params.validation_errors.is_empty() {
        return D::stop(format!(
            "VALIDATION_FAILED: {}",
            params.validation_errors.join("; ")
        ));
    }
    D::Generate
}
