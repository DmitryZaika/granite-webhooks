//! `POST /v1/customers/{id}/activity-search`: answers a question about a
//! customer from their emails, texts, deal notes and activities (and, on
//! request, transcripts of their last recorded calls), citing the sources.
//!
//! Port of `customerActivitySearch.server.ts` (prompt unchanged).

use super::history::{activities, customer_emails, notes};
use crate::ai::{Caller, Chat, MINI_MODEL};
use crate::auth::EmployeeUser;
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::JsonBody;
use crate::openapi::ToSchema;
use crate::routes::calls::recording::transcribe_call;
use crate::routes::calls::{cloudtalk_calls, ringcentral, to_e164};
use crate::state::AppState;
use axum::Json;
use axum::extract::{Path, State};
use chrono::{DateTime, NaiveDateTime, Utc};
use chrono_tz::America::Indiana::Indianapolis;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashSet;

const MAX_EMAILS: usize = 60;
const MAX_SMS: i64 = 80;
const MAX_CALL_TRANSCRIPTS: usize = 2;
const MIN_CALL_SECONDS: f64 = 20.0;
const MAX_DOC_CHARS: usize = 3500;
const MAX_TOTAL_CONTEXT_CHARS: usize = 90_000;

const SYSTEM_PROMPT: &str = r#"You answer questions about a customer's history with a sales team at a countertop company.
You receive indexed sources from emails, SMS messages, phone call transcripts, deal notes, and CRM activities.

Rules:
- Answer ONLY using the provided sources.
- Be concise and direct (1-4 sentences).
- When a source contains the customer's own words about the thing asked, state the fact and then include that exact phrase in quotes. Do not paraphrase those words away. If the customer never said it, do not invent a quote.
- Example: asked which color the customer wants, and the customer wrote "I want the white one" → She would like white. She said: "I want the white one."
- Return JSON only: {"answer":"...","sourceKeys":["email:123"]}
- sourceKeys must use exact keys from the sources (e.g. email:42, sms:10, call:999, note:5, activity:7).
- Include every source that directly supports your answer.

Evidence rules:
- If a source mentions, offers, promises, or describes something the user asked about, that IS evidence. Answer yes or describe what was said.
- An email or SMS sent to this customer about a discount, sale, price, or offer counts as evidence that we communicated that to them.
- Do NOT say "could not find evidence" if you cite a source that mentions the topic. Your answer and sourceKeys must agree.
- If sourceKeys is non-empty, your answer must affirm what those sources show.
- Only say you could not find evidence when NO source mentions the topic. Then sourceKeys must be [].
- If the question asks whether something was applied to a quote or order, and sources only show it was mentioned in an email or message, say we communicated it to the customer but no record confirms it on a quote or order — still cite that email or message in sourceKeys.

Example:
- Question: "Did we promise a 5% discount?"
- Source email:42 mentions "5% off July 4th sale" sent to this customer
- Good: {"answer":"Yes. We emailed this customer about a 5% off July 4th sale on 6/27/2026.","sourceKeys":["email:42"]}
- Question: "Which color would the customer like?"
- Source sms:10 is from the customer: "I want the white one"
- Good: {"answer":"She would like white. She said: \"I want the white one.\"","sourceKeys":["sms:10"]}
- Bad: saying no evidence while listing email:42 in sourceKeys"#;

/// Body of `POST /v1/customers/{id}/activity-search`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct ActivitySearchInput {
    /// The question, 3 to 500 characters.
    pub question: String,
    /// Also transcribe the last two recorded calls (slower, costs more).
    #[serde(default)]
    pub include_calls: bool,
}

/// A source the answer cites.
#[derive(Debug, Serialize, ToSchema)]
pub struct ActivitySource {
    /// `email:42`, `sms:10`, `call:999`, `note:5` or `activity:7`.
    pub key: String,
    /// What it is and when.
    pub label: String,
    /// Where to open it in the CRM (emails and texts).
    pub href: Option<String>,
}

/// `POST /v1/customers/{id}/activity-search`.
#[derive(Debug, Serialize, ToSchema)]
pub struct ActivityAnswer {
    /// The answer.
    pub answer: String,
    /// The sources it rests on.
    pub sources: Vec<ActivitySource>,
}

struct Document {
    key: String,
    date: Option<DateTime<Utc>>,
    label: String,
    content: String,
    href: Option<String>,
}

fn truncate(text: &str, max: usize) -> String {
    let trimmed = text.trim();
    if trimmed.chars().count() <= max {
        return trimmed.to_string();
    }
    let mut cut: String = trimmed.chars().take(max).collect();
    cut.push('…');
    cut
}

/// `toLocaleString()` of the CRM server, in Indianapolis time.
fn when(date: Option<DateTime<Utc>>) -> String {
    date.map_or_else(
        || "Invalid Date".into(),
        |date| {
            date.with_timezone(&Indianapolis)
                .format("%-m/%-d/%Y, %-I:%M:%S %p")
                .to_string()
        },
    )
}

fn strip_tags(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut in_tag = false;
    for ch in html.chars() {
        match ch {
            '<' => in_tag = true,
            '>' if in_tag => in_tag = false,
            _ if !in_tag => out.push(ch),
            _ => {}
        }
    }
    out
}

fn digits(phone: &str) -> String {
    phone.chars().filter(char::is_ascii_digit).collect()
}

fn last10(digits: &str) -> &str {
    &digits[digits.len().saturating_sub(10)..]
}

#[derive(sqlx::FromRow)]
struct SmsRow {
    id: i64,
    text: Option<String>,
    direction: String,
    created_date: Option<DateTime<Utc>>,
    sender: Option<String>,
    recipient: Option<String>,
}

async fn sms_documents(
    state: &AppState,
    company_id: i32,
    phones: &[Option<String>],
) -> Result<Vec<Document>, sqlx::Error> {
    let mut wanted: Vec<String> = Vec::new();
    for phone in phones.iter().flatten() {
        let all = digits(phone.trim());
        if all.len() < 10 {
            continue;
        }
        for candidate in [all.clone(), last10(&all).to_string()] {
            if !wanted.contains(&candidate) {
                wanted.push(candidate);
            }
        }
    }
    if wanted.is_empty() {
        return Ok(Vec::new());
    }
    let cleaned = |column: &str| {
        format!(
            "RIGHT(REPLACE(REPLACE(REPLACE(REPLACE(CAST({column} AS CHAR), ' ', ''), '-', ''), '(', ''), ')', ''), 10)"
        )
    };
    // Table, key prefix (RingCentral ids would collide with CloudTalk's), thread page.
    let mut documents = Vec::new();
    for (table, prefix, page) in [
        ("cloudtalk_sms", "sms:", "cloudtalk"),
        ("ringcentral_sms", "sms:rc", "ringcentral"),
    ] {
        let mut builder = sqlx::QueryBuilder::<sqlx::MySql>::new(format!(
            "SELECT CAST(id AS SIGNED) AS id, text, CAST(direction AS CHAR) AS direction, created_date,
                    CAST(sender AS CHAR) AS sender, CAST(recipient AS CHAR) AS recipient
               FROM {table} WHERE company_id = "
        ));
        // A copy made when its rep changed provider is its original's twin.
        builder.push_bind(company_id).push(format!(
            " AND {} AND status IN ('received', 'sent') AND (",
            crate::sms_provider::not_a_copy("")
        ));
        for (index, column) in ["sender", "recipient"].into_iter().enumerate() {
            if index > 0 {
                builder.push(" OR ");
            }
            builder.push(cleaned(column)).push(" IN (");
            let mut list = builder.separated(", ");
            for value in &wanted {
                list.push_bind(value.clone());
            }
            builder.push(")");
        }
        builder
            .push(") ORDER BY created_date DESC LIMIT ")
            .push_bind(MAX_SMS);
        let rows: Vec<SmsRow> = builder.build_query_as().fetch_all(&state.pool).await?;
        documents.extend(rows.into_iter().map(|row| {
            let inbound = row.direction == "inbound";
            let other = if inbound { &row.sender } else { &row.recipient };
            let other = digits(other.as_deref().unwrap_or(""));
            let date = row.created_date;
            Document {
                key: format!("{prefix}{}", row.id),
                date,
                label: format!("SMS · {}", when(date)),
                content: truncate(
                    &format!(
                        "[{}]: {}",
                        if inbound { "Customer" } else { "Sales Rep" },
                        row.text.unwrap_or_default()
                    ),
                    MAX_DOC_CHARS,
                ),
                href: (other.len() >= 10)
                    .then(|| format!("/employee/{page}/thread/{}", last10(&other))),
            }
        }));
    }
    documents.sort_by_key(|document| std::cmp::Reverse(document.date));
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    documents.truncate(MAX_SMS as usize);
    Ok(documents)
}

/// `CloudTalk`'s `2026-05-01 10:00:00` (UTC) or `RingCentral`'s RFC 3339.
fn call_started(at: &str) -> Option<DateTime<Utc>> {
    NaiveDateTime::parse_from_str(at, "%Y-%m-%d %H:%M:%S")
        .map(|at| at.and_utc())
        .or_else(|_| DateTime::parse_from_rfc3339(at).map(|at| at.with_timezone(&Utc)))
        .ok()
}

fn number(value: &Value) -> f64 {
    match value {
        Value::Number(number) => number.as_f64().unwrap_or(0.0),
        Value::String(text) => text.trim().parse().unwrap_or(0.0),
        _ => 0.0,
    }
}

fn truthy(value: &Value) -> bool {
    match value {
        Value::Bool(flag) => *flag,
        Value::Number(number) => number.as_f64().is_some_and(|n| n != 0.0),
        Value::String(text) => !text.is_empty(),
        _ => false,
    }
}

async fn call_documents(
    state: &AppState,
    company_id: i32,
    user_id: i32,
    phones: &[Option<String>],
) -> Vec<Document> {
    let phones: Vec<String> = phones
        .iter()
        .flatten()
        .filter_map(|phone| to_e164(phone))
        .collect();
    if phones.is_empty() {
        return Vec::new();
    }
    let ringcentral = ringcentral::Fetch {
        pool: &state.pool,
        client: &state.telephony_http,
        company_id,
    };
    // A company without one of the providers just has no calls there.
    let (cloudtalk, ringcentral) = tokio::join!(
        cloudtalk_calls(state, company_id, &phones, None, None),
        ringcentral.calls_for_phones(&phones, None, None),
    );
    let mut calls: Vec<Value> = cloudtalk
        .unwrap_or_default()
        .into_iter()
        .chain(ringcentral.unwrap_or_default())
        .collect();
    calls.retain(|call| {
        truthy(&call["Cdr"]["recorded"]) && number(&call["Cdr"]["billsec"]) >= MIN_CALL_SECONDS
    });
    let started = |call: &Value| call_started(call["Cdr"]["started_at"].as_str().unwrap_or(""));
    calls.sort_by_key(|call| std::cmp::Reverse(started(call)));
    calls.truncate(MAX_CALL_TRANSCRIPTS);
    let caller = Caller {
        feature: "customer_activity_search",
        user_id: Some(user_id),
        company_id: Some(company_id),
    };
    let mut documents = Vec::new();
    for call in calls {
        let cdr = &call["Cdr"];
        let id = match &cdr["id"] {
            Value::String(text) => text.clone(),
            Value::Number(number) => number.to_string(),
            _ => continue,
        };
        let link = cdr["recording_link"].as_str();
        let Ok(transcript) = transcribe_call(state, caller, &id, link, false).await else {
            continue;
        };
        let direction = if cdr["type"] == "incoming" {
            "Incoming"
        } else {
            "Outgoing"
        };
        #[allow(clippy::cast_possible_truncation)]
        let minutes = ((number(&cdr["billsec"]) / 60.0).round() as i64).max(1);
        let date = started(&call);
        documents.push(Document {
            key: format!("call:{id}"),
            date,
            label: format!("{direction} call · {}", when(date)),
            content: truncate(
                &format!("[{direction} call, {minutes} min]\n{}", transcript.text),
                MAX_DOC_CHARS,
            ),
            href: None,
        });
    }
    documents
}

#[derive(sqlx::FromRow)]
struct CustomerPhones {
    phone: Option<String>,
    phone_2: Option<String>,
}

async fn gather(
    state: &AppState,
    company_id: i32,
    user_id: i32,
    customer_id: i32,
    include_calls: bool,
) -> ApiResult<Vec<Document>> {
    let pool = &state.pool;
    let customer: CustomerPhones = sqlx::query_as(
        "SELECT phone, phone_2 FROM customers WHERE id = ? AND company_id = ? AND deleted_at IS NULL",
    )
    .bind(customer_id)
    .bind(company_id)
    .fetch_optional(pool)
    .await?
    .ok_or(ApiError::NotFound)?;
    let mut documents = Vec::new();
    for email in customer_emails(pool, company_id, customer_id)
        .await?
        .into_iter()
        .take(MAX_EMAILS)
    {
        let subject = email
            .subject
            .clone()
            .filter(|subject| !subject.is_empty())
            .unwrap_or_else(|| "No subject".into());
        let sender = if email.sender_user_id.is_some() {
            "Sales Rep"
        } else {
            "Customer"
        };
        documents.push(Document {
            key: format!("email:{}", email.id),
            date: email.sent_at,
            label: format!("Email · {subject} · {}", when(email.sent_at)),
            content: truncate(
                &format!(
                    "Subject: {subject}\nFrom: {sender}\n\n{}",
                    strip_tags(&email.body)
                ),
                MAX_DOC_CHARS,
            ),
            href: Some(format!(
                "/employee/customers/info/{customer_id}/chat/{}?messageId={}",
                email.thread_id, email.id
            )),
        });
    }
    for note in notes(pool, company_id, customer_id).await? {
        documents.push(Document {
            key: format!("note:{}", note.id),
            date: note.created_at,
            label: format!("Deal note · {}", when(note.created_at)),
            content: truncate(&note.content, MAX_DOC_CHARS),
            href: None,
        });
    }
    for activity in activities(pool, company_id, customer_id).await? {
        documents.push(Document {
            key: format!("activity:{}", activity.id),
            date: activity.created_at,
            label: format!(
                "Activity · {} · {}",
                activity.name,
                when(activity.created_at)
            ),
            content: truncate(
                &format!(
                    "Task: {}\nStage: {}\nCompleted: {}",
                    activity.name,
                    activity.deal_list_name.as_deref().unwrap_or("Unknown"),
                    if activity.is_completed == 0 {
                        "No"
                    } else {
                        "Yes"
                    }
                ),
                MAX_DOC_CHARS,
            ),
            href: None,
        });
    }
    let phones = [customer.phone, customer.phone_2];
    documents.extend(sms_documents(state, company_id, &phones).await?);
    if include_calls {
        documents.extend(call_documents(state, company_id, user_id, &phones).await);
    }
    documents.sort_by_key(|document| std::cmp::Reverse(document.date));
    let mut total = 0;
    Ok(documents
        .into_iter()
        .take_while(|document| {
            total += document.content.chars().count();
            total <= MAX_TOTAL_CONTEXT_CHARS
        })
        .collect())
}

fn context(documents: &[Document]) -> String {
    if documents.is_empty() {
        return "No customer activity records were found.".into();
    }
    documents
        .iter()
        .map(|document| {
            format!(
                "[{}] {}\n{}",
                document.key, document.label, document.content
            )
        })
        .collect::<Vec<_>>()
        .join("\n\n---\n\n")
}

/// Ask about a customer's history
///
/// Answers a question from the customer's emails, texts, deal notes and
/// activities (and, with `include_calls`, transcripts of their last two
/// recorded calls), with links to the sources.
#[crate::openapi::path(
    post,
    path = "/v1/customers/{id}/activity-search",
    operation_id = "search_customer_activity",
    tag = "customers",
    params(("id" = i32, Path, description = "Customer id")),
    request_body = ActivitySearchInput,
    responses(
        (status = 200, description = "Answer", body = ActivityAnswer),
        (status = 400, description = "Question too short or too long", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 404, description = "No such customer in the caller's company", body = ErrorBody),
        (status = 502, description = "OpenAI failed", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn activity_search(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path(customer_id): Path<i32>,
    JsonBody(input): JsonBody<ActivitySearchInput>,
) -> ApiResult<Json<ActivityAnswer>> {
    let question = input.question.trim();
    if !(3..=500).contains(&question.chars().count()) {
        return Err(ApiError::BadRequest("Invalid request data".into()));
    }
    let documents = gather(
        &state,
        user.company_id,
        user.id,
        customer_id,
        input.include_calls,
    )
    .await?;
    let mut chat = Chat::new(MINI_MODEL)
        .message("system", SYSTEM_PROMPT)
        .message(
            "user",
            &format!("QUESTION:\n{question}\n\nSOURCES:\n{}", context(&documents)),
        );
    chat.json = true;
    let caller = Caller {
        feature: "customer_activity_search",
        user_id: Some(user.id),
        company_id: Some(user.company_id),
    };
    let reply = state
        .ai
        .chat(&state.pool, caller, &chat)
        .await
        .map_err(|err| ApiError::Upstream(err.to_string()))?;
    let parsed: Value = serde_json::from_str(&reply).unwrap_or_else(
        |_| serde_json::json!({ "answer": "Could not parse AI response.", "sourceKeys": [] }),
    );
    let answer = parsed["answer"]
        .as_str()
        .map(str::trim)
        .filter(|answer| !answer.is_empty())
        .unwrap_or("No answer was generated.")
        .to_string();
    let mut seen = HashSet::new();
    let sources = parsed["sourceKeys"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .filter_map(|key| documents.iter().find(|document| document.key == key))
        .filter(|document| seen.insert(document.key.clone()))
        .map(|document| ActivitySource {
            key: document.key.clone(),
            label: document.label.clone(),
            href: document.href.clone(),
        })
        .collect();
    Ok(Json(ActivityAnswer { answer, sources }))
}
