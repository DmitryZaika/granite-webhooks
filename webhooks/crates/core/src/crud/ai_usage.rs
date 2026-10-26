use crate::openai::Usage;
use sqlx::MySqlPool;

/// One row of `ai_usage_log` for a chat completion. Mirrors what the CRM's
/// `trackedChatCompletion` records, so the AI usage dashboard picks it up.
pub struct ChatUsageLog<'a> {
    pub feature: &'a str,
    pub model: &'a str,
    pub company_id: Option<i32>,
    pub usage: Option<Usage>,
    pub error_message: Option<&'a str>,
    pub duration_ms: u32,
}

struct ChatPricing {
    input: Option<f64>,
    cached_input: Option<f64>,
    output: Option<f64>,
}

/// Same formula as `computeCostUsd` in the CRM: cached tokens are billed at the
/// cached rate, the rest of the input at the normal rate; missing rates cost 0.
fn chat_cost_usd(usage: Usage, pricing: &ChatPricing) -> f64 {
    let input = f64::from(usage.input_tokens);
    let cached = f64::from(usage.cached_input_tokens).min(input);
    let output = f64::from(usage.output_tokens);
    let total = output.mul_add(
        pricing.output.unwrap_or(0.0),
        cached.mul_add(
            pricing.cached_input.unwrap_or(0.0),
            (input - cached) * pricing.input.unwrap_or(0.0),
        ),
    );
    total / 1_000_000.0
}

pub async fn log_chat_usage(pool: &MySqlPool, entry: &ChatUsageLog<'_>) -> Result<(), sqlx::Error> {
    let pricing = sqlx::query_as!(
        ChatPricing,
        r#"SELECT CAST(input_per_1m_usd AS DOUBLE) AS "input?: f64",
                  CAST(cached_input_per_1m_usd AS DOUBLE) AS "cached_input?: f64",
                  CAST(output_per_1m_usd AS DOUBLE) AS "output?: f64"
           FROM ai_model_pricing
           WHERE model = ? AND effective_from <= CURDATE()
           ORDER BY effective_from DESC
           LIMIT 1"#,
        entry.model
    )
    .fetch_optional(pool)
    .await?;

    let cost_usd = match (entry.usage, &pricing) {
        (Some(usage), Some(pricing)) => chat_cost_usd(usage, pricing),
        _ => 0.0,
    };
    let error_message = entry
        .error_message
        .map(|message| message.chars().take(500).collect::<String>());

    sqlx::query!(
        r#"INSERT INTO ai_usage_log
           (feature, api_kind, model, company_id, input_tokens, cached_input_tokens, output_tokens, cost_usd, success, error_message, duration_ms)
           VALUES (?, 'chat', ?, ?, ?, ?, ?, ?, ?, ?, ?)"#,
        entry.feature,
        entry.model,
        entry.company_id,
        entry.usage.map(|u| u.input_tokens),
        entry.usage.map(|u| u.cached_input_tokens),
        entry.usage.map(|u| u.output_tokens),
        cost_usd,
        entry.error_message.is_none(),
        error_message,
        entry.duration_ms,
    )
    .execute(pool)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_chat_cost_bills_cached_tokens_at_cached_rate() {
        let pricing = ChatPricing {
            input: Some(2.0),
            cached_input: Some(0.2),
            output: Some(10.0),
        };
        let usage = Usage {
            input_tokens: 1_000_000,
            cached_input_tokens: 500_000,
            output_tokens: 100_000,
        };
        let cost = chat_cost_usd(usage, &pricing);
        assert!((cost - 2.1).abs() < 1e-9, "cost was {cost}");
    }

    #[test]
    fn test_chat_cost_missing_rates_cost_nothing() {
        let pricing = ChatPricing {
            input: None,
            cached_input: None,
            output: None,
        };
        let usage = Usage {
            input_tokens: 10,
            cached_input_tokens: 0,
            output_tokens: 10,
        };
        assert!(chat_cost_usd(usage, &pricing).abs() < f64::EPSILON);
    }
}
