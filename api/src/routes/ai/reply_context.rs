//! What the AI Reply button knows about the customer's quotes beyond the
//! conversation: colors per area, totals, and whether each was sent,
//! opened, accepted or signed. The rep's own voice is in
//! [`crate::followups::rep_voice`].
//!
//! The block is capped so the prompt stays a sane size.

use crate::followups::js::{js_len, js_slice, trim};
use crate::quote_pricing::doc::QuoteDoc;
use serde::Deserialize;
use sqlx::MySqlPool;

const MAX_QUOTES: i64 = 5;
const MAX_OPTIONS_PER_QUOTE: usize = 6;
const MAX_QUOTES_CHARS: usize = 3_500;

/// One of the customer's quotes, as the prompt needs it.
#[derive(Debug, Clone, Default, sqlx::FromRow)]
pub struct QuoteRow {
    pub id: i32,
    pub quote_name: Option<String>,
    pub status: String,
    pub deal_id: Option<i64>,
    pub created: Option<String>,
    pub expires: Option<String>,
    pub signed_at: Option<String>,
    /// 0-based color option the customer accepted.
    pub accepted_option: Option<i32>,
    pub current_revision: i32,
    /// The sent revision's drawing (what the customer has), else the draft.
    pub drawing: Option<String>,
    /// The sent revision's `[{optionId, total, missing}]`.
    pub totals: Option<String>,
    pub emails_sent: i64,
    pub last_sent: Option<String>,
    pub views: i64,
    pub first_viewed: Option<String>,
    pub last_viewed: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct OptionTotal {
    option_id: String,
    total: Option<f64>,
    #[serde(default)]
    missing: bool,
}

/// The customer's newest quotes (the focus deal's first), newest first.
pub async fn load_quotes(
    pool: &MySqlPool,
    company_id: i32,
    customer_id: i32,
    deal_id: Option<i64>,
) -> Result<Vec<QuoteRow>, sqlx::Error> {
    sqlx::query_as(
        "SELECT q.id, q.quote_name, q.status, CAST(q.deal_id AS SIGNED) AS deal_id,
                DATE_FORMAT(q.created_date, '%Y-%m-%d') AS created,
                DATE_FORMAT(q.expiration_date, '%Y-%m-%d') AS expires,
                DATE_FORMAT(q.signed_at, '%Y-%m-%d %H:%i') AS signed_at,
                q.customer_accepted_option_id AS accepted_option, q.current_revision,
                CAST(COALESCE(r.drawing_json, q.draft_drawing_json) AS CHAR) AS drawing,
                CAST(r.totals_json AS CHAR) AS totals,
                (SELECT COUNT(*) FROM quote_emails e WHERE e.quote_id = q.id) AS emails_sent,
                (SELECT DATE_FORMAT(MAX(e.sent_at), '%Y-%m-%d %H:%i') FROM quote_emails e WHERE e.quote_id = q.id) AS last_sent,
                CAST((SELECT COALESCE(SUM(e.view_count), 0) FROM quote_emails e WHERE e.quote_id = q.id) AS SIGNED) AS views,
                (SELECT DATE_FORMAT(MIN(e.first_viewed_at), '%Y-%m-%d %H:%i') FROM quote_emails e WHERE e.quote_id = q.id) AS first_viewed,
                (SELECT DATE_FORMAT(MAX(e.last_viewed_at), '%Y-%m-%d %H:%i') FROM quote_emails e WHERE e.quote_id = q.id) AS last_viewed
           FROM quotes q
           LEFT JOIN quote_revisions r ON r.quote_id = q.id AND r.revision_number = q.current_revision
          WHERE q.company_id = ? AND q.customer_id = ? AND q.deleted_at IS NULL
          ORDER BY COALESCE(q.deal_id = ?, 0) DESC, q.created_date DESC, q.id DESC
          LIMIT ?",
    )
    .bind(company_id)
    .bind(customer_id)
    .bind(deal_id)
    .bind(MAX_QUOTES)
    .fetch_all(pool)
    .await
}

/// `$4,350.00`.
pub fn format_money(amount: f64) -> String {
    let cents = (amount * 100.0).round();
    let negative = cents < 0.0;
    #[allow(clippy::cast_possible_truncation)]
    let cents = cents.abs() as i64;
    let whole = (cents / 100).to_string();
    let mut grouped = String::new();
    for (i, digit) in whole.chars().enumerate() {
        if i > 0 && (whole.len() - i) % 3 == 0 {
            grouped.push(',');
        }
        grouped.push(digit);
    }
    format!(
        "{}${grouped}.{:02}",
        if negative { "-" } else { "" },
        cents % 100
    )
}

/// The colors of each option, area by area: `Kitchen: Quartz Calacatta (edge Eased)`.
fn option_lines(doc: &QuoteDoc, totals: &[OptionTotal], accepted: Option<usize>) -> Vec<String> {
    doc.color_options
        .iter()
        .enumerate()
        .take(MAX_OPTIONS_PER_QUOTE)
        .map(|(index, option)| {
            let areas: Vec<String> = doc
                .areas
                .iter()
                .filter_map(|area| {
                    let color = option.by_area.get(&area.id)?;
                    let stone = [trim(&color.product), trim(&color.color)]
                        .into_iter()
                        .filter(|s| !s.is_empty())
                        .collect::<Vec<_>>()
                        .join(" ");
                    if stone.is_empty() {
                        return None;
                    }
                    let mut edges: Vec<&str> = Vec::new();
                    for edge in color.edges.iter().map(|e| trim(e)) {
                        if !edge.is_empty() && !edges.contains(&edge) {
                            edges.push(edge);
                        }
                    }
                    let edges = if edges.is_empty() {
                        String::new()
                    } else {
                        format!(" (edge {})", edges.join(", "))
                    };
                    Some(format!("{}: {stone}{edges}", trim(&area.name)))
                })
                .collect();
            let total = totals
                .iter()
                .find(|t| t.option_id == option.id)
                .and_then(|t| {
                    t.total.filter(|v| v.is_finite() && *v > 0.0).map(|v| {
                        format!(
                            " · total {}{}",
                            format_money(v),
                            if t.missing {
                                " (some prices missing)"
                            } else {
                                ""
                            }
                        )
                    })
                })
                .unwrap_or_default();
            format!(
                "  Option {}{}: {}{total}",
                index + 1,
                if accepted == Some(index) {
                    " (ACCEPTED by the customer)"
                } else {
                    ""
                },
                if areas.is_empty() {
                    "(no colors picked)".to_string()
                } else {
                    areas.join("; ")
                }
            )
        })
        .collect()
}

/// The quotes block of the prompt; empty when there are none.
pub fn format_quotes(quotes: &[QuoteRow], focus_deal_id: Option<i64>) -> String {
    let mut blocks: Vec<String> = Vec::new();
    for quote in quotes {
        let name = quote
            .quote_name
            .as_deref()
            .map(trim)
            .filter(|n| !n.is_empty())
            .unwrap_or("Quote");
        let mut head = format!("Quote \"{name}\" (#{}, status {}", quote.id, quote.status);
        if let Some(created) = &quote.created {
            head.push_str(&format!(", made {created}"));
        }
        if quote.current_revision > 0 {
            head.push_str(&format!(", revision {}", quote.current_revision));
        }
        if let Some(deal) = quote.deal_id {
            head.push_str(&format!(", deal #{deal}"));
            if focus_deal_id == Some(deal) {
                head.push_str(" — the deal this message is about");
            }
        }
        head.push(')');
        let mut lines = vec![head];
        let doc = quote
            .drawing
            .as_deref()
            .and_then(|text| serde_json::from_str::<serde_json::Value>(text).ok())
            .and_then(|value| QuoteDoc::parse(&value));
        let totals: Vec<OptionTotal> = quote
            .totals
            .as_deref()
            .and_then(|text| serde_json::from_str(text).ok())
            .unwrap_or_default();
        let accepted = quote.accepted_option.and_then(|i| usize::try_from(i).ok());
        if let Some(doc) = &doc {
            let areas: Vec<&str> = doc
                .areas
                .iter()
                .map(|a| trim(&a.name))
                .filter(|n| !n.is_empty())
                .collect();
            if !areas.is_empty() {
                lines.push(format!("  Areas: {}", areas.join(", ")));
            }
            lines.extend(option_lines(doc, &totals, accepted));
        }
        if quote.current_revision == 0 {
            lines.push(
                "  Draft — never sent to the customer: do not mention its price or treat it as sent."
                    .into(),
            );
        }
        if quote.emails_sent > 0 {
            let mut sent = format!(
                "  Emailed to the customer {} time{}",
                quote.emails_sent,
                if quote.emails_sent == 1 { "" } else { "s" }
            );
            if let Some(last) = &quote.last_sent {
                sent.push_str(&format!(", last {last} UTC"));
            }
            if quote.views > 0 {
                sent.push_str(&format!(
                    " · customer opened it {} time{}",
                    quote.views,
                    if quote.views == 1 { "" } else { "s" }
                ));
                if let Some(first) = &quote.first_viewed {
                    sent.push_str(&format!(", first {first} UTC"));
                }
                if let Some(last) = &quote.last_viewed {
                    sent.push_str(&format!(", last {last} UTC"));
                }
            } else {
                sent.push_str(" · NOT opened yet");
            }
            lines.push(sent);
        } else if quote.current_revision > 0 {
            lines.push(
                "  Not emailed from the quote page (it may have been shared another way).".into(),
            );
        }
        if let Some(signed) = &quote.signed_at {
            lines.push(format!("  Signed by the customer {signed} UTC"));
        }
        if let Some(expires) = &quote.expires {
            lines.push(format!("  Expires {expires}"));
        }
        blocks.push(lines.join("\n"));
    }
    let text = blocks.join("\n");
    if js_len(&text) > MAX_QUOTES_CHARS {
        format!("{}…", js_slice(&text, 0, MAX_QUOTES_CHARS))
    } else {
        text
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DOC: &str = r#"{"version":2,"areas":[{"id":"a1","name":"Kitchen"},{"id":"a2","name":"Island"}],
        "colorOptions":[
          {"id":"o1","byArea":{"a1":{"product":"Quartz","color":"Calacatta Karmelo","edges":["Eased","Eased"]},"a2":{"product":"Quartz","color":"Calacatta Karmelo","edges":["Ogee"]}}},
          {"id":"o2","byArea":{"a1":{"product":"Granite","color":"Viraldi"}}}
        ]}"#;

    #[test]
    fn money_is_grouped() {
        assert_eq!(format_money(4350.0), "$4,350.00");
        assert_eq!(format_money(1_234_567.891), "$1,234,567.89");
        assert_eq!(format_money(99.5), "$99.50");
    }

    #[test]
    fn a_sent_quote_lists_colors_totals_and_views() {
        let quote = QuoteRow {
            id: 12,
            quote_name: Some("Smith kitchen".into()),
            status: "active".into(),
            deal_id: Some(7),
            created: Some("2026-10-01".into()),
            current_revision: 2,
            drawing: Some(DOC.into()),
            totals: Some(
                r#"[{"optionId":"o1","total":4350,"missing":false},{"optionId":"o2","total":3900.5,"missing":true}]"#
                    .into(),
            ),
            emails_sent: 1,
            last_sent: Some("2026-10-02 15:00".into()),
            views: 3,
            first_viewed: Some("2026-10-02 18:10".into()),
            last_viewed: Some("2026-10-04 09:00".into()),
            accepted_option: Some(1),
            ..QuoteRow::default()
        };
        let text = format_quotes(&[quote], Some(7));
        assert!(text.starts_with("Quote \"Smith kitchen\" (#12, status active, made 2026-10-01, revision 2, deal #7 — the deal this message is about)"));
        assert!(text.contains("  Areas: Kitchen, Island"));
        assert!(text.contains("  Option 1: Kitchen: Quartz Calacatta Karmelo (edge Eased); Island: Quartz Calacatta Karmelo (edge Ogee) · total $4,350.00"));
        assert!(text.contains("  Option 2 (ACCEPTED by the customer): Kitchen: Granite Viraldi · total $3,900.50 (some prices missing)"));
        assert!(text.contains("Emailed to the customer 1 time, last 2026-10-02 15:00 UTC · customer opened it 3 times, first 2026-10-02 18:10 UTC, last 2026-10-04 09:00 UTC"));
    }

    #[test]
    fn a_draft_is_flagged_and_unopened_emails_are_said() {
        let draft = QuoteRow {
            id: 3,
            status: "draft".into(),
            drawing: Some("not json".into()),
            ..QuoteRow::default()
        };
        let text = format_quotes(&[draft], None);
        assert!(text.starts_with("Quote \"Quote\" (#3, status draft)"));
        assert!(text.contains("Draft — never sent to the customer"));
        let unopened = QuoteRow {
            id: 4,
            status: "active".into(),
            current_revision: 1,
            emails_sent: 2,
            ..QuoteRow::default()
        };
        assert!(
            format_quotes(&[unopened], None)
                .contains("Emailed to the customer 2 times · NOT opened yet")
        );
        assert_eq!(format_quotes(&[], None), "");
    }
}
