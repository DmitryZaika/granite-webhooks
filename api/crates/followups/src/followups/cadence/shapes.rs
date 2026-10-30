//! The fixed sequence shapes: what each step of a cadence tries to do
//! (generated from the tables of `smsFlowAiCadence.ts`; the purposes go
//! into the AI prompt word for word).

use super::super::pressure::FollowupAngle;
use super::types::FollowupSituation;

/// A step's intent before it is pinned to a date or channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShapeDef {
    pub situation: FollowupSituation,
    pub goal: &'static str,
    pub angle: FollowupAngle,
    pub subangle: Option<&'static str>,
    pub purpose: &'static str,
}

/// Plain-language long-term nurture.
pub const NURTURE_PURPOSE: &str = "Very light long-term nurture in plain salesperson language. Short and concrete — like a real SMS. Ask one normal question about the project (still happening, still comparing colors, still want help). Never corporate or AI phrasing: no “leave the door open”, “on your radar”, “take a back seat”, “fresh conversation with no assumptions”, “restarting can be simple”, “one small first step”, “ideas or practical planning”. Never tell them to reply with a keyword.";

pub const POST_HOLD_SHAPE: &[ShapeDef] = &[
    ShapeDef {
        situation: FollowupSituation::StalledConversation,
        goal: "Reactivate after hold",
        angle: FollowupAngle::HoldReactivation,
        subangle: Some("original_timing"),
        purpose: "First touch after a CRM hold. If unfinished_next_step involves measurements, confirm the project is still active and ask whether measurements are now available — do not open with timing-only unless no measurement step exists. Otherwise test whether timing is better now. Acknowledge a reconnect reason ONLY if hold_reason_evidence supports it. Do not invent fall, home repairs, or money problems. Do not ask material/design first. Do not treat this as a brand-new lead. One low-pressure question. Do not include the stones/customer page URL.",
    },
    ShapeDef {
        situation: FollowupSituation::StalledConversation,
        goal: "Check project status",
        angle: FollowupAngle::ProjectStatus,
        subangle: Some("useful_next_step"),
        purpose: "New useful reason to reply: a concrete next step if they are ready. Do not say you texted recently or ask if they received the last message. Do not make them restate the known hold reason.",
    },
    ShapeDef {
        situation: FollowupSituation::StalledConversation,
        goal: "Clarify project scope",
        angle: FollowupAngle::ProjectScope,
        subangle: Some("scope_or_obstacle"),
        purpose: "Kitchen vs vanity, what is being replaced, or what still needs to happen — only if useful. Frame known hold context naturally (you had planned to revisit after home repairs) instead of asking them to explain it again.",
    },
    ShapeDef {
        situation: FollowupSituation::StalledConversation,
        goal: "Provide value",
        angle: FollowupAngle::HelpfulInformation,
        subangle: Some("quartz_vs_granite_maintenance"),
        purpose: "A different strategy than earlier messages: one useful fact. Do not repeat the same helpful_information hook.",
    },
    ShapeDef {
        situation: FollowupSituation::StalledConversation,
        goal: "Reconnect",
        angle: FollowupAngle::Reactivation,
        subangle: Some("light_checkin"),
        purpose: "Low-pressure restart after more silence. Interest confidence is lower. Do not use fresh-lead urgency.",
    },
    ShapeDef {
        situation: FollowupSituation::StalledConversation,
        goal: "Light nurture",
        angle: FollowupAngle::LongTermNurture,
        subangle: Some("project_still_relevant"),
        purpose: NURTURE_PURPOSE,
    },
    ShapeDef {
        situation: FollowupSituation::StalledConversation,
        goal: "Long-term nurture",
        angle: FollowupAngle::LongTermNurture,
        subangle: Some("easy_restart"),
        purpose: NURTURE_PURPOSE,
    },
];

pub const EXISTING_OLD_LEAD_SHAPE: &[ShapeDef] = &[
    ShapeDef {
        situation: FollowupSituation::StalledConversation,
        goal: "Test current timing",
        angle: FollowupAngle::ProjectTiming,
        subangle: Some("current_readiness"),
        purpose: "CRM shows prior interaction but no SMS/email. This is not first_contact. Ask whether the project timing is still open. Do not open with material/design. Do not write as a brand-new inbound lead. Do not include the stones/customer page URL.",
    },
    ShapeDef {
        situation: FollowupSituation::StalledConversation,
        goal: "Check project status",
        angle: FollowupAngle::ProjectStatus,
        subangle: Some("useful_next_step"),
        purpose: "New useful next step. Do not say you texted recently or ask if they got a prior message.",
    },
    ShapeDef {
        situation: FollowupSituation::StalledConversation,
        goal: "Clarify project scope",
        angle: FollowupAngle::ProjectScope,
        subangle: Some("scope_or_obstacle"),
        purpose: "One new fact about scope or obstacle. Do not repeat the first question.",
    },
    ShapeDef {
        situation: FollowupSituation::StalledConversation,
        goal: "Provide value",
        angle: FollowupAngle::HelpfulInformation,
        subangle: Some("quartz_vs_granite_maintenance"),
        purpose: "A different helpful-information strategy. Do not repeat an earlier hook.",
    },
    ShapeDef {
        situation: FollowupSituation::StalledConversation,
        goal: "Reconnect",
        angle: FollowupAngle::Reactivation,
        subangle: Some("light_checkin"),
        purpose: "Low-pressure restart. Interest is unconfirmed.",
    },
    ShapeDef {
        situation: FollowupSituation::StalledConversation,
        goal: "Light nurture",
        angle: FollowupAngle::LongTermNurture,
        subangle: Some("project_still_relevant"),
        purpose: NURTURE_PURPOSE,
    },
    ShapeDef {
        situation: FollowupSituation::StalledConversation,
        goal: "Long-term nurture",
        angle: FollowupAngle::LongTermNurture,
        subangle: Some("easy_restart"),
        purpose: NURTURE_PURPOSE,
    },
];

pub const UNREACHED_SHAPE: &[ShapeDef] = &[
    ShapeDef {
        situation: FollowupSituation::StalledConversation,
        goal: "Check original timeline",
        angle: FollowupAngle::OriginalTimeline,
        subangle: None,
        purpose: "Use the strongest customer-specific signal — if they said ASAP / urgent / this month, ask whether that timeline still holds. One question only. Do not ask whether they are still interested. Do not offer an estimate if that was already asked. Do not ask about colors if that was already asked.",
    },
    ShapeDef {
        situation: FollowupSituation::StalledConversation,
        goal: "Clarify project scope",
        angle: FollowupAngle::ProjectScope,
        subangle: None,
        purpose: "One new fact: kitchen vs vanity, what is being replaced, or remove-and-dispose — only if useful and not already asked. One low-friction question. Do not repeat colors, interest, or estimate.",
    },
    ShapeDef {
        situation: FollowupSituation::StalledConversation,
        goal: "Uncover the real holdup",
        angle: FollowupAngle::DecisionObstacle,
        subangle: None,
        purpose: "Timing, budget, or choosing materials — one multiple-choice. Do not ask if the project is still active.",
    },
    ShapeDef {
        situation: FollowupSituation::StalledConversation,
        goal: "Provide value",
        angle: FollowupAngle::HelpfulInformation,
        subangle: None,
        purpose: "Useful resource without repeating the stones link if it was already sent. Do not lecture. Do not re-ask colors or interest.",
    },
    ShapeDef {
        situation: FollowupSituation::StalledConversation,
        goal: "Reconnect",
        angle: FollowupAngle::Reactivation,
        subangle: None,
        purpose: "Low-pressure restart after multichannel silence. New angle only. Do not ask whether to close the file.",
    },
    ShapeDef {
        situation: FollowupSituation::StalledConversation,
        goal: "Close the loop",
        angle: FollowupAngle::CloseLoop,
        subangle: None,
        purpose: "Use close_loop once only. Softly offer to pause outreach. Do not ask them to reply with a keyword. Later messages back off.",
    },
    ShapeDef {
        situation: FollowupSituation::StalledConversation,
        goal: "Light nurture",
        angle: FollowupAngle::LongTermNurture,
        subangle: Some("project_still_relevant"),
        purpose: NURTURE_PURPOSE,
    },
    ShapeDef {
        situation: FollowupSituation::StalledConversation,
        goal: "Light nurture",
        angle: FollowupAngle::LongTermNurture,
        subangle: Some("easy_restart"),
        purpose: NURTURE_PURPOSE,
    },
];

pub const EARLY_SHAPE: &[ShapeDef] = &[
    ShapeDef {
        situation: FollowupSituation::FirstContact,
        goal: "Initial outreach",
        angle: FollowupAngle::MaterialPreference,
        subangle: None,
        purpose: "First reach-out only when we have never emailed or texted them. Narrow the search with one leading choice from what we know (color vs style, simple vs bold veining, remnant vs full slab). If the stones/customer page URL is not already in the conversation, include the exact Customer page URL from the profile in this first message (EMAIL always; SMS if it stays short). Example: \"Are you hoping for a simpler look, or stronger black-and-brown veining? You can also browse options here:\" plus the URL.",
    },
    ShapeDef {
        situation: FollowupSituation::SecondFollowup,
        goal: "Re-engage",
        angle: FollowupAngle::DesignHelp,
        subangle: None,
        purpose: "Second touch — still unanswered. Give a new useful reason to reply (a leading choice or next step). Do not say you texted or emailed recently and do not ask if they received the last message. If the stones/customer page URL was not sent yet, include it here.",
    },
    ShapeDef {
        situation: FollowupSituation::StalledConversation,
        goal: "Build interest",
        angle: FollowupAngle::OriginalTimeline,
        subangle: None,
        purpose: "Create curiosity or remind them of something relevant to their project. Prefer a leading either/or question. If the stones link still has not been sent, include the exact Customer page URL now. Email can offer a simple choice. Example subject: \"Your kitchen countertops\". Example: \"Are you mainly trying to figure out the material, the price, or the timing?\"",
    },
    ShapeDef {
        situation: FollowupSituation::StalledConversation,
        goal: "Explore situation",
        angle: FollowupAngle::ProjectStatus,
        subangle: None,
        purpose: "Learn where they are with the project and what they are considering. Example: \"Is the kitchen project still active, or did the timeline get pushed back?\"",
    },
    ShapeDef {
        situation: FollowupSituation::StalledConversation,
        goal: "Change angle",
        angle: FollowupAngle::HelpfulInformation,
        subangle: None,
        purpose: "Approach from a different perspective or give a new reason to engage. Use a specific piece of relevant value from the lead (material comparison, sink options, installation timing). Include the customer page only if it was still not sent earlier. EMAIL: exact URL from the profile. SMS: include the URL if it stays short.",
    },
    ShapeDef {
        situation: FollowupSituation::StalledConversation,
        goal: "Uncover obstacles",
        angle: FollowupAngle::DecisionObstacle,
        subangle: None,
        purpose: "Identify what may be delaying, preventing, or complicating their decision. Example: \"What part of the countertop project are you still trying to figure out?\"",
    },
    ShapeDef {
        situation: FollowupSituation::StalledConversation,
        goal: "Provide value",
        angle: FollowupAngle::HelpfulInformation,
        subangle: None,
        purpose: "Useful resource, not a lecture. Prefer this only if the stones link was still not sent earlier — otherwise pick a different value angle without repeating the URL. Example: \"We have some new colors and slabs. You can take a look here when you have a chance:\" plus the exact customer-page URL from the profile.",
    },
    ShapeDef {
        situation: FollowupSituation::StalledConversation,
        goal: "Reconnect",
        angle: FollowupAngle::Reactivation,
        subangle: None,
        purpose: "A longer wait has passed with no reply. Low-pressure restart. Do not pressure and do not re-explain the old sales details. Do not ask whether to close the file.",
    },
    ShapeDef {
        situation: FollowupSituation::StalledConversation,
        goal: "Close the loop",
        angle: FollowupAngle::CloseLoop,
        subangle: None,
        purpose: "Use close_loop once only. Softly offer to pause outreach. Do not ask them to reply with a keyword. After this, later messages must back off — do not keep asking whether to stop contacting them.",
    },
    ShapeDef {
        situation: FollowupSituation::StalledConversation,
        goal: "Light nurture",
        angle: FollowupAngle::LongTermNurture,
        subangle: None,
        purpose: NURTURE_PURPOSE,
    },
    ShapeDef {
        situation: FollowupSituation::StalledConversation,
        goal: "Light nurture",
        angle: FollowupAngle::QuoteStatus,
        subangle: None,
        purpose: NURTURE_PURPOSE,
    },
];

pub const STALLED_SHAPE: &[ShapeDef] = &[
    ShapeDef {
        situation: FollowupSituation::StalledConversation,
        goal: "Re-engage",
        angle: FollowupAngle::MaterialPreference,
        subangle: None,
        purpose: "Multiple prior emails/texts already went unanswered. Prefer SMS if we have only emailed so far — they may not see email. Do NOT say \"I emailed you recently\" or ask if they received a prior message. New leading either/or question only. If the stones/customer page URL was not sent yet, include it in this first message. Do not rephrase the last unanswered message.",
    },
    ShapeDef {
        situation: FollowupSituation::StalledConversation,
        goal: "Build interest",
        angle: FollowupAngle::DesignHelp,
        subangle: None,
        purpose: "Create interest with a leading either/or question. Email can offer a simple choice. If the stones link still has not been sent, include it now. Do not ask the same question as the last outbound.",
    },
    ShapeDef {
        situation: FollowupSituation::StalledConversation,
        goal: "Uncover obstacles",
        angle: FollowupAngle::DecisionObstacle,
        subangle: None,
        purpose: "Low-pressure. What may be delaying the decision. Do not turn this into a questionnaire.",
    },
    ShapeDef {
        situation: FollowupSituation::StalledConversation,
        goal: "Change angle",
        angle: FollowupAngle::HelpfulInformation,
        subangle: None,
        purpose: "Different hook. Use a specific detail from the lead. Include the customer page only if it was still not sent earlier. EMAIL: exact URL from the profile. SMS: include the URL if it stays short.",
    },
    ShapeDef {
        situation: FollowupSituation::StalledConversation,
        goal: "Explore situation",
        angle: FollowupAngle::OriginalTimeline,
        subangle: None,
        purpose: "Find out whether the project is still active at all. Example: \"Is the kitchen still happening, or did the timeline get pushed back?\"",
    },
    ShapeDef {
        situation: FollowupSituation::StalledConversation,
        goal: "Provide value",
        angle: FollowupAngle::HelpfulInformation,
        subangle: None,
        purpose: "Useful resource, not a lecture. Prefer this only if the stones link was still not sent earlier — otherwise a different value angle without repeating the URL. Example: \"We have some new colors and slabs. You can take a look here when you have a chance:\" plus the exact customer-page URL from the profile.",
    },
    ShapeDef {
        situation: FollowupSituation::StalledConversation,
        goal: "Close the loop",
        angle: FollowupAngle::CloseLoop,
        subangle: None,
        purpose: "Use close_loop once only. Softly offer to pause outreach. Do not ask them to reply with a keyword. Later messages must back off — do not keep asking whether to stop contacting them.",
    },
    ShapeDef {
        situation: FollowupSituation::StalledConversation,
        goal: "Light nurture",
        angle: FollowupAngle::LongTermNurture,
        subangle: None,
        purpose: NURTURE_PURPOSE,
    },
];

pub const MONTHLY_SHAPE: &[ShapeDef] = &[
    ShapeDef {
        situation: FollowupSituation::StalledConversation,
        goal: "Reconnect",
        angle: FollowupAngle::Reactivation,
        subangle: None,
        purpose: "Reactivation after many unanswered touches. Do NOT say \"I emailed you recently\" or ask if they got a prior message. Open with a fresh leading question. If the stones/customer page URL was not sent yet, include it here with a short reason to reply — do not send only the link. Example: \"Are you still exploring options for the kitchen?\" Do not assume old notes are still true. Prefer newer customer messages over older internal notes.",
    },
    ShapeDef {
        situation: FollowupSituation::StalledConversation,
        goal: "Create interest",
        angle: FollowupAngle::ProjectStatus,
        subangle: None,
        purpose: "Open and easy. Example: \"Are you still planning the kitchen, or has the project changed?\" If one known fact is solid, \"Did you end up moving forward with the cabinets?\" If the stones link still has not been sent, include it now. Do not assume they are still considering both cabinets and countertops. Do not ask a consulting \"which first?\" question.",
    },
    ShapeDef {
        situation: FollowupSituation::StalledConversation,
        goal: "Understand",
        angle: FollowupAngle::DecisionObstacle,
        subangle: None,
        purpose: "Soft multiple choice. Example: \"Has anything been holding up the kitchen project — timing, budget, or choosing materials?\" Do not say \"holdup\" as if they are stuck.",
    },
    ShapeDef {
        situation: FollowupSituation::StalledConversation,
        goal: "Provide value",
        angle: FollowupAngle::HelpfulInformation,
        subangle: None,
        purpose: "Useful resource, not a lecture. Prefer this only if the stones link was still not sent earlier — otherwise a different value angle without repeating the URL. Example: \"We have some new colors and slabs. You can take a look here when you have a chance:\" plus the exact customer-page URL from the profile. Do not explain why comparing materials early helps.",
    },
    ShapeDef {
        situation: FollowupSituation::StalledConversation,
        goal: "Close the loop",
        angle: FollowupAngle::CloseLoop,
        subangle: None,
        purpose: "Use close_loop once only. Softly offer to pause outreach. Keep it low-pressure and easy to answer even if they are no longer interested. Do not ask them to reply with a keyword. After this, later messages must back off — do not keep asking whether to stop contacting them.",
    },
    ShapeDef {
        situation: FollowupSituation::StalledConversation,
        goal: "Light nurture",
        angle: FollowupAngle::HelpfulInformation,
        subangle: None,
        purpose: NURTURE_PURPOSE,
    },
    ShapeDef {
        situation: FollowupSituation::StalledConversation,
        goal: "Light nurture",
        angle: FollowupAngle::OriginalTimeline,
        subangle: None,
        purpose: NURTURE_PURPOSE,
    },
    ShapeDef {
        situation: FollowupSituation::StalledConversation,
        goal: "Light nurture",
        angle: FollowupAngle::QuoteStatus,
        subangle: None,
        purpose: NURTURE_PURPOSE,
    },
];
