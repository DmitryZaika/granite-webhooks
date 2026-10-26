//! The `NEW_CONTEXT` cadence prompt.
//!
//! The Markdown instructions (`ai-followup.md`, the CRM's
//! `app/prompts/ai-followup.md`), the JSON contract, and the model context
//! (port of `aiFollowupRefactoredPrompt.ts` and `aiFollowupPrompt.server.ts`).

use super::js::{js_len, trim};
use super::model_context::{CadenceModelContext, format_cadence_model_context};

/// The follow-up instructions the model reads.
pub const AI_FOLLOWUP_PROMPT: &str = include_str!("ai-followup.md");

/// The JSON contract the cadence parser reads; not sales rules.
pub const REFACTORED_CADENCE_OUTPUT_CONTRACT: &str = r#"Return JSON only.

Write exactly one message per STEP in the context, in that order.

{"skip":false,"messages":[{"skip":false,"subject":"email subject or empty string for SMS","body":"the message to send","delayMinutes":0}]}

Rules for this JSON:
- subject is required on email steps and ignored on SMS
- body is the customer-facing text
- delayMinutes is required: minutes to wait after the previous message (0 = send as soon as allowed for the first); the instructions above say how to choose it
- a message may be {"skip":true,"reason":"..."} when that step has nothing useful; top-level {"skip":true,"reason":"..."} only if you cannot write any of the steps"#;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefactoredCadencePrompt {
    pub system_prompt: String,
    pub user_prompt: String,
    pub markdown_chars: usize,
    pub output_contract_chars: usize,
    pub model_context_chars: usize,
}

pub fn build_refactored_cadence_prompt(
    markdown_instructions: &str,
    context: &CadenceModelContext,
) -> RefactoredCadencePrompt {
    let markdown = trim(markdown_instructions);
    let user_prompt = format_cadence_model_context(context);
    RefactoredCadencePrompt {
        system_prompt: format!("{markdown}\n\n{REFACTORED_CADENCE_OUTPUT_CONTRACT}"),
        markdown_chars: js_len(markdown),
        output_contract_chars: js_len(REFACTORED_CADENCE_OUTPUT_CONTRACT),
        model_context_chars: js_len(&user_prompt),
        user_prompt,
    }
}

/// The repair prompt; `failed_step_numbers` (1-based) narrows it to the
/// messages that failed so the passing ones are not written again.
pub fn build_refactored_cadence_repair_user_prompt(
    base_user_prompt: &str,
    failure_reasons: &[String],
    failed_step_numbers: &[usize],
) -> String {
    let failures = if failure_reasons.is_empty() {
        "- (none listed)".to_string()
    } else {
        failure_reasons
            .iter()
            .map(|reason| format!("- {reason}"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    let scope = if failed_step_numbers.is_empty() {
        "REGENERATE the messages that failed validation. Keep accepted messages on the same job. Use only the structured context above.".to_string()
    } else {
        let count = failed_step_numbers.len();
        let steps = failed_step_numbers
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(", STEP ");
        format!(
            "REGENERATE ONLY the failed messages listed below. Return exactly {count} message{}, for STEP {steps} in that order, and nothing for any other STEP. Use only the structured context above.",
            if count == 1 { "" } else { "s" }
        )
    };
    format!("{base_user_prompt}\n\n{scope}\n\nValidation failures:\n{failures}")
}

/// `Math.ceil(chars / 4)`.
pub const fn estimate_tokens_from_chars(chars: usize) -> usize {
    chars.div_ceil(4)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repair_prompts_name_only_the_failed_steps() {
        let base = "BASE";
        assert_eq!(
            build_refactored_cadence_repair_user_prompt(base, &[], &[]),
            "BASE\n\nREGENERATE the messages that failed validation. Keep accepted messages on the same job. Use only the structured context above.\n\nValidation failures:\n- (none listed)"
        );
        let prompt = build_refactored_cadence_repair_user_prompt(
            base,
            &["Message 2: too long".into()],
            &[2, 3],
        );
        assert!(
            prompt.contains("Return exactly 2 messages, for STEP 2, STEP 3 in that order"),
            "{prompt}"
        );
        assert!(prompt.ends_with("Validation failures:\n- Message 2: too long"));
        assert!(AI_FOLLOWUP_PROMPT.len() > 1000);
        assert_eq!(estimate_tokens_from_chars(9), 3);
    }
}
