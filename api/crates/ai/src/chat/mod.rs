//! The CRM's AI chat (port of the Remix `api.chat` loader).
//!
//! Instructions mode (company instructions and the CRM manual), price lists
//! mode (supplier price files, special orders) and assistant mode (skills
//! over the company's records). An answer is a stream of [`Event`]s, sent
//! to the browser as server-sent events by `routes/ai/chat.rs`.

pub mod assistant;
pub mod crm_manual;
pub mod instructions;
pub mod manual_feedback;
pub mod markers;
pub mod prompts;

use crate::ai::{AiError, Caller, Chat, MINI_FALLBACK_MODEL, MINI_MODEL};
use crate::auth::session::SessionUser;
use crate::followups::js::{js_slice, trim};
use crate::price_lists::{self, PriceListContext, SourceFileType};
use crate::state::AppState;
use instructions::Instruction;
use lambda_http::tracing;
use markers::SpecialOrderOffer;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::MySqlPool;
use std::time::Duration;
use tokio::sync::mpsc;

/// The chat's three modes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Instructions,
    PriceLists,
    Assistant,
}

/// One step of an answer, in the order the browser handles them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// Keeps proxies from buffering the start of the stream.
    Ping,
    /// `info`: "Connecting to AI...".
    Info(String),
    /// `status`: `{"state":"searching"|"answering"}`.
    Status(&'static str),
    /// A piece of the answer (the default `message` event).
    Text(String),
    /// `replace`: the answer so far is dropped (a second look is coming).
    Replace,
    /// A named JSON event: `instructions`, `sources`, `images`, `actions`,
    /// `trace`, `thicknessOptions`, `slabSizeOptions`, `priceListOptions`,
    /// `specialOrderOffer`, `clarify` (the choices of a question back).
    Json(&'static str, String),
    /// The answer is complete (`data: [DONE]`).
    Done,
}

/// What the user asked.
#[derive(Debug, Clone, Deserialize)]
pub struct Request {
    pub query: String,
    pub mode: Option<String>,
    pub is_new: bool,
    pub special_order: Option<SpecialOrderOffer>,
}

/// One saved message of the conversation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Message {
    pub role: String,
    #[serde(deserialize_with = "content_text")]
    pub content: String,
}

fn content_text<'de, D: serde::Deserializer<'de>>(d: D) -> Result<String, D::Error> {
    Ok(match Value::deserialize(d)? {
        Value::String(text) => text,
        Value::Null => String::new(),
        other => other.to_string(),
    })
}

impl Message {
    fn new(role: &str, content: impl Into<String>) -> Self {
        Self {
            role: role.into(),
            content: content.into(),
        }
    }

    fn is_system(&self) -> bool {
        self.role == "system"
    }
}

/// The DONE marker the browser waits for (`DONE_KEY` in `app/utils/constants.ts`).
pub const DONE: &str = "[DONE]035d8eba-9f8c-44c5-a1e0-290d1da033f7[/DONE]";

struct Sender {
    tx: mpsc::Sender<Event>,
}

impl Sender {
    async fn send(&self, event: Event) {
        // The browser went away; the answer is still saved.
        let _ = self.tx.send(event).await;
    }

    async fn json(&self, name: &'static str, value: &(impl Serialize + Sync)) {
        self.send(Event::Json(
            name,
            serde_json::to_string(value).unwrap_or_default(),
        ))
        .await;
    }

    /// `streamTextAnswer`: a ready answer in small pieces.
    async fn text_answer(&self, text: &str) {
        let units: Vec<u16> = text.encode_utf16().collect();
        let mut start = 0;
        while start < units.len() {
            let end = (start + 24).min(units.len());
            self.send(Event::Text(String::from_utf16_lossy(&units[start..end])))
                .await;
            start = end;
        }
    }
}

/// Whether `user` may use the assistant, whose tools read the company's
/// customers, deals and notes: employees only, like the CRM pages for them.
pub const fn may_use_assistant(user: &SessionUser) -> bool {
    user.is_employee || user.is_admin || user.is_superuser
}

/// The mode a question is answered in (`priceLists` and `assistant` are
/// asked for; a question about the company's records goes to the assistant
/// when `records` allows it, see [`may_use_assistant`], unless it asks how
/// to use the app: "can I delete a deal?" is the manual's).
pub fn mode_for(mode: Option<&str>, query: &str, records: bool) -> Mode {
    match mode {
        Some("priceLists") => Mode::PriceLists,
        Some("assistant") if records => Mode::Assistant,
        _ if records
            && prompts::looks_like_assistant_query(query)
            && !crm_manual::asks_how_to(query) =>
        {
            Mode::Assistant
        }
        _ => Mode::Instructions,
    }
}

async fn tax_rate(pool: &MySqlPool, company_id: i32) -> f64 {
    let raw: Option<String> =
        sqlx::query_scalar("SELECT CAST(state_taxes AS CHAR) FROM company WHERE id = ?")
            .bind(company_id)
            .fetch_optional(pool)
            .await
            .ok()
            .flatten()
            .flatten();
    let rate = raw.map_or(f64::NAN, |r| crate::followups::js::js_parse_float(&r));
    if rate.is_finite() && rate >= 0.0 {
        rate
    } else {
        0.0
    }
}

/// The user's saved conversation: `(id, messages)` of their first
/// `chat_history` row.
async fn saved_history(
    pool: &MySqlPool,
    user_id: i32,
) -> sqlx::Result<Option<(i32, Vec<Message>)>> {
    let row: Option<(i32, Option<String>)> =
        sqlx::query_as("SELECT id, CAST(history AS CHAR) FROM chat_history WHERE user_id = ?")
            .bind(user_id)
            .fetch_optional(pool)
            .await?;
    Ok(row.map(|(id, history)| {
        let messages = history
            .and_then(|h| serde_json::from_str::<Vec<Message>>(&h).ok())
            .unwrap_or_default();
        (id, messages)
    }))
}

fn strip_assistant_prompt(messages: Vec<Message>) -> Vec<Message> {
    messages
        .into_iter()
        .filter(|m| {
            !(m.is_system()
                && (m.content.starts_with(assistant::OLD_SYSTEM_PREFIX)
                    || m.content.starts_with(assistant::SYSTEM_PREFIX)))
        })
        .collect()
}

/// The conversation so far with the new question (`getContext`).
async fn continued(
    pool: &MySqlPool,
    user_id: i32,
    query: &str,
) -> sqlx::Result<(Option<i32>, Vec<Message>)> {
    let (id, messages) = saved_history(pool, user_id)
        .await?
        .map_or((None, Vec::new()), |(id, m)| (Some(id), m));
    let mut messages = strip_assistant_prompt(messages);
    messages.push(Message::new("user", query));
    Ok((id, messages))
}

/// Saves the conversation with its answer (`updateContext` / `insertContext`).
async fn save(
    pool: &MySqlPool,
    user_id: i32,
    id: Option<i32>,
    mut messages: Vec<Message>,
    answer: &str,
) {
    messages.push(Message::new("assistant", answer));
    let history = serde_json::to_string(&messages).unwrap_or_else(|_| "[]".into());
    let result = match id {
        Some(id) => {
            sqlx::query("UPDATE chat_history SET history = ? WHERE id = ? AND user_id = ?")
                .bind(history)
                .bind(id)
                .bind(user_id)
                .execute(pool)
                .await
        }
        None => {
            sqlx::query("INSERT INTO chat_history (history, user_id) VALUES (?, ?)")
                .bind(history)
                .bind(user_id)
                .execute(pool)
                .await
        }
    };
    if let Err(err) = result {
        tracing::error!(error = %err, user = user_id, "chat history not saved");
    }
}

/// `messages` with `context` as a system message: first ones removed by
/// `is_old`, the new one put at the index `place` picks.
fn with_system(
    messages: Vec<Message>,
    content: String,
    is_old: impl Fn(&Message) -> bool,
    place: Placement,
) -> Vec<Message> {
    let mut kept: Vec<Message> = messages
        .into_iter()
        .filter(|m| !(m.is_system() && is_old(m)))
        .collect();
    let message = Message::new("system", content);
    let index = match place {
        Placement::BeforeFirst => kept.iter().position(Message::is_system),
        Placement::AfterFirst => kept.iter().position(Message::is_system).map(|i| i + 1),
        Placement::AfterLast => kept.iter().rposition(Message::is_system).map(|i| i + 1),
    };
    match index {
        Some(i) => kept.insert(i, message),
        None => kept.insert(0, message),
    }
    kept
}

#[derive(Clone, Copy)]
enum Placement {
    BeforeFirst,
    AfterFirst,
    AfterLast,
}

fn with_instruction_context(
    messages: Vec<Message>,
    instructions: &[Instruction],
    query: &str,
) -> Vec<Message> {
    with_system(
        messages,
        format!(
            "{}\n\n{}",
            instructions::instruction_context(instructions, query),
            prompts::INSTRUCTION_RULES
        ),
        |m| m.content.contains("Here are the instructions you can use"),
        Placement::BeforeFirst,
    )
}

fn with_price_list_context(messages: Vec<Message>, context: &str) -> Vec<Message> {
    with_system(
        messages,
        format!("{context}\n\n{}", prompts::PRICE_LIST_RULES),
        |m| m.content.starts_with("SUPPLIER PRICE LISTS"),
        Placement::AfterFirst,
    )
}

fn with_crm_manual(messages: Vec<Message>, manual: &str, access_block: &str) -> Vec<Message> {
    with_system(
        messages,
        format!(
            "{access_block}\n\n{}\n\nCRM USER MANUAL:\n\n{manual}\n\n{}",
            crm_manual::ACCESS_RULES,
            prompts::CRM_MANUAL_RULES
        ),
        |m| m.content.contains("CRM USER MANUAL:"),
        Placement::AfterLast,
    )
}

/// `messages` without the system messages `is_old` picks.
fn without_system(messages: Vec<Message>, is_old: impl Fn(&Message) -> bool) -> Vec<Message> {
    messages
        .into_iter()
        .filter(|m| !(m.is_system() && is_old(m)))
        .collect()
}

/// What the reply to a question back picked.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Clarified {
    /// Granite Manager or the company's own process.
    Source(crm_manual::Source),
    /// One of the features offered, answered from its sections.
    Feature(crm_manual::Feature),
}

/// The question a clarification asked about and what the user's reply
/// picks, when `messages` ends with the clarifying question and that reply.
fn picked_clarification(messages: &[Message]) -> Option<(String, Clarified)> {
    let [.., asked, reply] = messages else {
        return None;
    };
    if asked.role != "assistant" || reply.role != "user" {
        return None;
    }
    let question = markers::parse_clarify_marker(&asked.content)?;
    let offered: Vec<crm_manual::Feature> = markers::parse_clarify_choices(&asked.content)
        .into_iter()
        .map(|(label, sections)| {
            if sections.is_empty() {
                crm_manual::Feature {
                    label,
                    ..crm_manual::company_choice()
                }
            } else {
                crm_manual::feature(&label, sections)
            }
        })
        .collect();
    let offers_company = offered.is_empty()
        || offered
            .iter()
            .any(|c| c.source == crm_manual::Source::Company);
    let picked = match crm_manual::feature_choice(&reply.content, &offered) {
        Some(choice) if choice.source == crm_manual::Source::Company => {
            Clarified::Source(crm_manual::Source::Company)
        }
        Some(choice) => Clarified::Feature(choice.clone()),
        // "app" or "company" typed to a question that offered the company's
        // process: where the answer comes from (the manual then asks which
        // feature, if it has to).
        None if offers_company => Clarified::Source(crm_manual::clarify_choice(&reply.content)?),
        None => return None,
    };
    Some((question, picked))
}

/// `messages` for answering a clarified question from what was picked
/// only: the clarification and the reply give way to the question with
/// what the user meant, and the other source's context goes.
fn clarified(mut messages: Vec<Message>, question: &str, picked: &Clarified) -> Vec<Message> {
    messages.truncate(messages.len().saturating_sub(2));
    let (meant, other): (String, &str) = match picked {
        Clarified::Source(crm_manual::Source::Manual) => (
            "I mean how to do this in Granite Manager.".into(),
            "Here are the instructions you can use",
        ),
        Clarified::Source(crm_manual::Source::Company) => (
            "I mean our company's own process, not how to use Granite Manager.".into(),
            "CRM USER MANUAL:",
        ),
        Clarified::Feature(feature) => (
            format!("I mean {} in Granite Manager.", feature.label),
            "Here are the instructions you can use",
        ),
    };
    let mut messages = without_system(messages, |m| m.content.contains(other));
    let content = prompts::instruction_user_message(&format!("{question} ({meant})"));
    // The question asked is the last user message before the clarification.
    match messages.last_mut() {
        Some(last) if last.role == "user" => last.content = content,
        _ => messages.push(Message::new("user", content)),
    }
    messages
}

/// The model's messages (a saved clarifying question without its marker).
fn request_messages(messages: &[Message]) -> Vec<Value> {
    messages
        .iter()
        .map(|m| {
            let content = if m.content.contains("[[CLARIFY:") {
                markers::strip_clarify_marker(&m.content)
            } else {
                m.content.clone()
            };
            json!({ "role": m.role, "content": content })
        })
        .collect()
}

/// Whether the answer before the last question came from the CRM manual:
/// that question went to the model as `prompts::manual_user_message`.
fn last_answer_from_manual(messages: &[Message]) -> bool {
    messages
        .iter()
        .filter(|m| m.role == "user")
        .rev()
        .nth(1)
        .is_some_and(|m| prompts::is_manual_question(&m.content))
}

/// The user's questions before the last message, as they asked them: not
/// the wording of `prompts::instruction_user_message` around them.
fn earlier_questions(messages: &[Message]) -> Vec<String> {
    let asked: Vec<&Message> = messages.iter().filter(|m| m.role == "user").collect();
    asked[..asked.len().saturating_sub(1)]
        .iter()
        .map(|m| prompts::asked_question(&m.content).to_string())
        .collect()
}

/// `completeChatAnswer`: the answer streamed as it is written, holding back
/// the last 64 characters so a marker is never shown half-written.
/// `manual`: a how-to answered from the CRM manual, with room and time to
/// reason: on low effort its 1024 tokens left step lists cut off or empty,
/// and it skipped steps and mixed up who may open a page (the manual eval,
/// `api/tests/crm_manual_eval.rs`).
async fn complete(
    state: &AppState,
    user: &SessionUser,
    messages: &[Message],
    out: &Sender,
    manual: bool,
) -> Result<String, AiError> {
    const HOLDBACK: usize = 64;
    let caller = Caller {
        feature: "chat",
        user_id: Some(user.id),
        company_id: Some(user.company_id),
    };
    let (tx, mut rx) = mpsc::unbounded_channel::<String>();
    let forward = async {
        let mut pending = String::new();
        while let Some(piece) = rx.recv().await {
            pending.push_str(&piece);
            let units = pending.encode_utf16().count();
            if units > HOLDBACK {
                let head = js_slice(&pending, 0, units - HOLDBACK);
                let tail = js_slice(&pending, units - HOLDBACK, units);
                out.send(Event::Text(head)).await;
                pending = tail;
            }
        }
        if !pending.is_empty() {
            out.send(Event::Text(pending)).await;
        }
    };
    let run = async {
        let mut chat = Chat::new(MINI_MODEL).max_tokens(Some(if manual { 4000 } else { 1024 }));
        chat.messages = request_messages(messages);
        chat.timeout = Duration::from_secs(120);
        if manual {
            chat.reasoning_effort = Some("medium");
        }
        let first = state.ai.chat_stream(&state.pool, caller, &chat, |t| {
            let _ = tx.send(t.to_string());
        });
        let answer = match first.await {
            Err(err) if err.is_model_permission() => {
                chat.model = MINI_FALLBACK_MODEL;
                state
                    .ai
                    .chat_stream(&state.pool, caller, &chat, |t| {
                        let _ = tx.send(t.to_string());
                    })
                    .await
            }
            other => other,
        };
        drop(tx);
        answer
    };
    let (answer, ()) = tokio::join!(run, forward);
    answer
}

/// Answers `request` for `user`, sending the answer's events to `tx`.
pub async fn answer(state: AppState, user: SessionUser, request: Request, tx: mpsc::Sender<Event>) {
    let out = Sender { tx };
    for _ in 0..30 {
        out.send(Event::Ping).await;
    }
    out.send(Event::Info("Connecting to AI...".into())).await;
    let mut mode = mode_for(
        request.mode.as_deref(),
        &request.query,
        may_use_assistant(&user),
    );
    // The rules send any question that names the company's records to the
    // assistant; the model tells how to do it in the app ("mark deal won")
    // from a question about the records ("which deals did we win", "did
    // the customer open my quote?" is the app's). A typed email or phone
    // number is the records'.
    let mut app_pick = None;
    if mode == Mode::Assistant
        && request.mode.as_deref() != Some("assistant")
        && !prompts::lookup_kind_is_contact(&request.query)
    {
        app_pick = app_question(&state, &user, &request).await;
        if app_pick.is_some() {
            mode = Mode::Instructions;
        }
    }
    if let Err(err) = run(&state, &user, &request, mode, app_pick, &out).await {
        tracing::error!(error = %err, ?mode, "AI chat request failed");
        let message = if err.is_model_permission() {
            "Chat is temporarily unavailable because the AI model is not enabled for this workspace. Please try again later.".to_string()
        } else {
            format!("Error: {err}")
        };
        out.send(Event::Text(message)).await;
    }
}

/// The model's pick for a question the rules send to the assistant, when it
/// finds the question is about using the app (its sections, or which
/// feature to ask about): the manual answers it instead.
async fn app_question(
    state: &AppState,
    user: &SessionUser,
    request: &Request,
) -> Option<crm_manual::Picked> {
    let earlier = if request.is_new {
        Vec::new()
    } else {
        continued(&state.pool, user.id, &request.query)
            .await
            .map(|(_, messages)| earlier_questions(&messages))
            .unwrap_or_default()
    };
    let caller = Caller {
        feature: "chat",
        user_id: Some(user.id),
        company_id: Some(user.company_id),
    };
    let asker = crm_manual::asker(user.is_employee, user.is_admin, user.is_superuser);
    match crm_manual::pick(state, caller, &request.query, &earlier, asker).await? {
        crm_manual::Picked::Sections(ids) if ids.is_empty() => None,
        crm_manual::Picked::Records => None,
        picked => Some(picked),
    }
}

/// Why an answer failed.
#[derive(Debug, thiserror::Error)]
pub enum ChatError {
    #[error("{0}")]
    Ai(#[from] AiError),
    #[error("{0}")]
    Database(#[from] sqlx::Error),
}

impl ChatError {
    fn is_model_permission(&self) -> bool {
        matches!(self, Self::Ai(err) if err.is_model_permission())
    }
}

/// A price-list search with the "searching" status first.
async fn price_list_context(
    state: &AppState,
    company_id: i32,
    query: &str,
    out: &Sender,
) -> sqlx::Result<PriceListContext> {
    out.send(Event::Status("searching")).await;
    price_lists::build_context(&state.pool, company_id, query).await
}

#[allow(clippy::too_many_lines)]
async fn run(
    state: &AppState,
    user: &SessionUser,
    request: &Request,
    mode: Mode,
    // The model's pick that took the question from the assistant.
    app_pick: Option<crm_manual::Picked>,
    out: &Sender,
) -> Result<(), ChatError> {
    let pool = &state.pool;
    let query = request.query.as_str();
    let (company_id, user_id) = (user.company_id, user.id);
    let tax = tax_rate(pool, company_id).await;
    let history_id: Option<i32>;
    let mut messages: Vec<Message>;
    let mut found: Option<PriceListContext> = None;
    let mut skip_rebuild = false;
    let mut loaded: Option<Vec<Instruction>> = None;
    // What is answered: the user's question, or the one a clarification asked
    // about once the reply picks where its answer comes from (`source`) or
    // which feature it is about (`feature`, from the manual).
    let mut question = query.to_string();
    let mut source: Option<crm_manual::Source> = None;
    let mut feature: Option<crm_manual::Feature> = None;

    if !request.is_new
        && mode == Mode::PriceLists
        && let Some(offer) = request.special_order
    {
        let (id, history) = continued(pool, user_id, query).await?;
        history_id = id;
        skip_rebuild = true;
        if markers::user_declined_special_order(query) {
            let answer = "No problem. Let me know if you need anything else.";
            finish_with(out, pool, user_id, history_id, history, answer).await;
            return Ok(());
        }
        let parsed = markers::parse_slabs_and_delivery(query);
        if let (Some(slabs), Some(delivery_cost), true) =
            (parsed.slabs, parsed.delivery_cost, offer.is_complete())
        {
            let answer = markers::special_order_answer(&markers::SpecialOrderInput {
                price_per_sqft: offer.price_per_sqft,
                length_inches: offer.length_inches.unwrap_or_default(),
                width_inches: offer.width_inches.unwrap_or_default(),
                slabs,
                delivery_cost,
                delivery_cost_type: parsed.delivery_cost_type,
                tax_rate: tax,
            });
            finish_with(out, pool, user_id, history_id, history, &answer).await;
            return Ok(());
        }
        messages = history;
        if markers::should_rebuild_price_list_context(query, Some(&offer)) {
            let context = price_list_context(state, company_id, query, out).await?;
            messages = with_price_list_context(messages, &context.context);
            found = Some(context);
        }
    } else if request.is_new {
        history_id = saved_history(pool, user_id).await?.map(|(id, _)| id);
        messages = match mode {
            Mode::PriceLists => {
                let context = price_list_context(state, company_id, query, out).await?;
                let messages = vec![
                    Message::new(
                        "system",
                        format!("{}\n\n{}", context.context, prompts::PRICE_LIST_RULES),
                    ),
                    Message::new(
                        "user",
                        format!(
                            "Answer the question using the supplier price lists.\n\nQuestion: {query}\n\nAnswer:"
                        ),
                    ),
                ];
                found = Some(context);
                messages
            }
            Mode::Assistant => vec![
                Message::new("system", assistant::system_prompt(state, ctx(user)).await),
                Message::new("user", query),
            ],
            Mode::Instructions => {
                let all = instructions::load_visible(pool, company_id, user_id).await?;
                let messages = vec![
                    Message::new(
                        "system",
                        format!(
                            "{}\n\n{}",
                            instructions::instruction_context(&all, query),
                            prompts::INSTRUCTION_RULES
                        ),
                    ),
                    Message::new("user", prompts::instruction_user_message(query)),
                ];
                loaded = Some(all);
                messages
            }
        };
    } else {
        let (id, history) = continued(pool, user_id, query).await?;
        history_id = id;
        messages = history;
        match mode {
            Mode::PriceLists => {
                if markers::should_rebuild_price_list_context(query, request.special_order.as_ref())
                {
                    let context = price_list_context(state, company_id, query, out).await?;
                    messages = with_price_list_context(messages, &context.context);
                    found = Some(context);
                } else {
                    skip_rebuild = true;
                }
            }
            Mode::Assistant => {
                let prompt = assistant::system_prompt(state, ctx(user)).await;
                let mut next = vec![Message::new("system", prompt)];
                next.extend(strip_assistant_prompt(messages));
                messages = next;
            }
            Mode::Instructions => {
                let all = instructions::load_visible(pool, company_id, user_id).await?;
                if let Some((asked, picked)) = picked_clarification(&messages) {
                    messages = clarified(messages, &asked, &picked);
                    question = asked;
                    source = Some(match picked {
                        Clarified::Source(picked) => picked,
                        Clarified::Feature(picked) => {
                            feature = Some(picked);
                            crm_manual::Source::Manual
                        }
                    });
                }
                if source != Some(crm_manual::Source::Manual) {
                    messages = with_instruction_context(messages, &all, &question);
                }
                loaded = Some(all);
            }
        }
    }

    // The company's inventory or customer quote link: its own, not the model's guess.
    if crm_manual::asks_customer_link(&question) {
        let domain: Option<Option<String>> =
            sqlx::query_scalar("SELECT domain FROM company WHERE id = ?")
                .bind(company_id)
                .fetch_optional(pool)
                .await?;
        if let Some(answer) =
            crm_manual::customer_link_answer(&question, company_id, domain.flatten().as_deref())
        {
            finish_with(out, pool, user_id, history_id, messages, &answer).await;
            return Ok(());
        }
    }

    // A follow-up to an answer from the manual ("what does it mean?", "there
    // is no such option") stays on the manual, unless it asks about the
    // company's own process (or the model finds it is not about the app).
    let follows_manual = mode == Mode::Instructions
        && source.is_none()
        && last_answer_from_manual(&messages)
        && !crm_manual::asks_about_company_process(&question);
    let mut how_to = source.map_or_else(
        || follows_manual || crm_manual::looks_like_how_to(&question),
        |picked| picked == crm_manual::Source::Manual,
    );
    if mode == Mode::Instructions {
        // The app's how-to or the company's own process: ask, don't guess.
        if source.is_none()
            && let Some(all) = &loaded
            && crm_manual::should_clarify(
                &question,
                how_to,
                instructions::has_title_match(all, &question),
            )
        {
            // A word of several features: the company's process is one more
            // choice of the same question.
            if let Some(crm_manual::Features::Ask(asked)) = crm_manual::features(&question)
                && let Some(text) = asked.with_company
            {
                let mut choices = asked.choices;
                choices.push(crm_manual::company_choice());
                let asked = crm_manual::Clarification {
                    question: text,
                    choices,
                    with_company: None,
                };
                ask_which_feature(out, pool, user_id, history_id, messages, &question, &asked)
                    .await;
            } else {
                ask_to_clarify(out, pool, user_id, history_id, messages, &question).await;
            }
            return Ok(());
        }
        if source != Some(crm_manual::Source::Company) {
            let access = crm_manual::access(pool, user).await?;
            let earlier = earlier_questions(&messages);
            let search: String = std::iter::once(question.as_str())
                .chain(earlier.iter().map(String::as_str))
                .collect::<Vec<_>>()
                .join(" ");
            let quick = crm_manual::quick_answer(&question, Some(&access));
            // Picked for the question asked, not for a reply to a question back.
            let app_pick = app_pick.filter(|_| source.is_none());
            let ids = if let Some(feature) = &feature {
                feature.sections.clone()
            } else if quick.is_none()
                && (how_to || app_pick.is_some() || crm_manual::asks_how_to(&question))
            {
                let caller = Caller {
                    feature: "chat",
                    user_id: Some(user_id),
                    company_id: Some(company_id),
                };
                match crm_manual::sections_for(
                    state,
                    caller,
                    &question,
                    &earlier,
                    crm_manual::asker(user.is_employee, user.is_admin, user.is_superuser),
                    &search,
                    app_pick,
                )
                .await
                {
                    // About using the app: short steps from the manual.
                    Ok(Some(ids)) => {
                        how_to = true;
                        ids
                    }
                    // Not about the app (the model says): a follow-up to the
                    // manual goes back to the company's instructions.
                    Ok(None) => {
                        if follows_manual {
                            how_to = false;
                        }
                        crm_manual::section_ids_for(&search)
                    }
                    Err(clarification) => {
                        ask_which_feature(
                            out,
                            pool,
                            user_id,
                            history_id,
                            messages,
                            &question,
                            &clarification,
                        )
                        .await;
                        return Ok(());
                    }
                }
            } else {
                crm_manual::section_ids_for(&search)
            };
            let manual = crm_manual::content_with(pool, &ids).await;
            messages = with_crm_manual(messages, &manual, &crm_manual::access_block(&access));
            if how_to {
                // The browser offers the manual's feedback on this answer.
                out.json("crmManual", &true).await;
                // Short steps from the manual, not the instructions' checklist.
                if let Some(last) = messages.iter_mut().rev().find(|m| m.role == "user") {
                    last.content =
                        prompts::manual_user_message(prompts::asked_question(&last.content));
                }
            }
            if let Some(quick) = quick {
                finish_with(out, pool, user_id, history_id, messages, &quick).await;
                return Ok(());
            }
        }
    }

    if mode == Mode::PriceLists
        && let Some(context) = &found
        && context.sources.is_empty()
        && let Some(readiness) = context.readiness
        && readiness.ready == 0
    {
        let answer = markers::price_list_not_ready_message(
            &context.supplier_name,
            readiness.total,
            readiness.processing,
            readiness.failed,
        );
        finish_with(out, pool, user_id, history_id, messages, &answer).await;
        return Ok(());
    }

    let (all_instructions, mut selected) = if mode == Mode::Instructions && !how_to {
        let all = match loaded {
            Some(all) => all,
            None => instructions::load_visible(pool, company_id, user_id).await?,
        };
        let selected = instructions::select_for_context(&all, &question);
        (all, selected)
    } else {
        (Vec::new(), Vec::new())
    };

    out.send(Event::Status("answering")).await;

    if mode == Mode::Assistant {
        let result = assistant::run(state, ctx(user), request_messages(&messages)).await?;
        out.text_answer(&result.answer).await;
        if !result.actions.is_empty() {
            out.json("actions", &result.actions).await;
        }
        if !result.traces.is_empty() {
            out.json("trace", &result.traces).await;
        }
        out.send(Event::Done).await;
        save(
            pool,
            user_id,
            history_id,
            strip_assistant_prompt(messages),
            &result.answer,
        )
        .await;
        return Ok(());
    }

    let manual = mode == Mode::Instructions && how_to;
    let mut answer = complete(state, user, &messages, out, manual).await?;

    if mode == Mode::Instructions
        && !how_to
        && markers::instruction_answer_needs_another_look(&answer)
    {
        let exclude = instructions::ids_to_exclude_for_retry(&selected, &answer, &question);
        let rest: Vec<Instruction> = all_instructions
            .iter()
            .filter(|i| !exclude.contains(&i.id))
            .cloned()
            .collect();
        let leftover = instructions::select_for_context(&rest, &question);
        if leftover
            .iter()
            .any(|i| instructions::is_related(i, &question))
        {
            out.send(Event::Replace).await;
            messages = with_instruction_context(messages, &leftover, &question);
            selected = leftover;
            answer = complete(state, user, &messages, out, false).await?;
        }
    }

    let mut clean = markers::strip_markers(&answer);
    let in_price_lists = mode == Mode::PriceLists;
    let thickness = if in_price_lists {
        markers::infer_thickness_options_from_answer(&answer)
    } else {
        None
    };
    let awaiting_thickness = thickness.as_ref().is_some_and(|t| t.len() >= 2);
    let slab_sizes = if in_price_lists && !awaiting_thickness {
        markers::infer_slab_size_options_from_answer(&answer)
    } else {
        None
    };
    let awaiting_size = slab_sizes.as_ref().is_some_and(|t| t.len() >= 2);
    let products = if in_price_lists && !awaiting_thickness && !awaiting_size {
        markers::infer_price_list_options_from_answer(&answer)
    } else {
        None
    };
    let awaiting_product = products.as_ref().is_some_and(|t| t.len() >= 2);
    let awaiting = awaiting_thickness || awaiting_size || awaiting_product;
    let offer = if in_price_lists && !awaiting {
        markers::parse_special_order_marker(&answer)
            .or_else(|| markers::infer_special_order_offer_from_answer(&answer))
    } else {
        None
    };
    if in_price_lists && awaiting {
        clean = markers::strip_ai_special_order_prompt(&clean);
    }
    if in_price_lists && offer.is_some() {
        clean = markers::append_special_order_prompt(&clean);
        let stripped = markers::strip_markers(&answer);
        let units = clean.encode_utf16().count();
        let tail = js_slice(&clean, stripped.encode_utf16().count(), units);
        if !tail.is_empty() {
            out.send(Event::Text(tail)).await;
        }
    }

    let has_info = markers::answer_has_usable_info(&clean);
    let mut images: Vec<String> = Vec::new();
    if has_info && mode == Mode::Instructions && !how_to {
        // Only an instruction the model cited whose text has what the answer
        // says: no guess when it cited none, and none when unsure.
        let cited: Vec<instructions::MatchedInstruction> =
            instructions::used_for_reply(&selected, &answer, &question)
                .into_iter()
                .filter(|m| {
                    selected
                        .iter()
                        .any(|i| i.id == m.id && instructions::supports_answer(i, &clean))
                })
                .collect();
        if !cited.is_empty() {
            out.json("instructions", &cited).await;
            images = instructions::images_for_used(&selected, &cited, &clean, &question);
        }
    }
    if in_price_lists && has_info {
        let choices = Choices {
            thickness: thickness.as_deref(),
            slab_sizes: slab_sizes.as_deref(),
            products: products.as_deref(),
            offer: offer.as_ref(),
        };
        price_list_events(
            out,
            &answer,
            query,
            if skip_rebuild { None } else { found.as_ref() },
            &choices,
        )
        .await;
    }
    if has_info && !images.is_empty() {
        out.json("images", &images).await;
    }
    out.send(Event::Done).await;
    save(pool, user_id, history_id, messages, &clean).await;
    Ok(())
}

struct Choices<'a> {
    thickness: Option<&'a [markers::ThicknessOption]>,
    slab_sizes: Option<&'a [markers::SlabSizeOption]>,
    products: Option<&'a [markers::PriceListOption]>,
    offer: Option<&'a SpecialOrderOffer>,
}

/// The price-list events after an answer: the sources it cites (`found`
/// is the search this answer used), the choices it offers and the special
/// order.
async fn price_list_events(
    out: &Sender,
    answer: &str,
    query: &str,
    found: Option<&PriceListContext>,
    choices: &Choices<'_>,
) {
    if let Some(context) = found {
        let cited = markers::sources_where_color_found(
            &markers::parse_source_ids(answer),
            &context.sources,
            |s| s.id,
            |s| s.supplier_name.as_str(),
            &context.context,
            query,
        );
        if !cited.is_empty() {
            let payload: Vec<Value> = cited
                .iter()
                .map(|s| json!({ "id": s.id, "name": s.name, "supplierName": s.supplier_name, "url": s.url }))
                .collect();
            out.json("sources", &payload).await;
        }
        if let Some(primary) = cited.first()
            && primary.file_type == SourceFileType::Image
        {
            out.json("images", &[&primary.url]).await;
        }
    }
    if let Some(options) = choices.thickness.filter(|o| o.len() >= 2) {
        out.json("thicknessOptions", &options).await;
    }
    if let Some(options) = choices.slab_sizes.filter(|o| o.len() >= 2) {
        out.json("slabSizeOptions", &options).await;
    }
    if let Some(options) = choices.products.filter(|o| o.len() >= 2) {
        out.json("priceListOptions", &options).await;
    }
    if let Some(offer) = choices.offer {
        out.json("specialOrderOffer", offer).await;
    }
}

/// A ready answer: status, the text, done, saved.
async fn finish_with(
    out: &Sender,
    pool: &MySqlPool,
    user_id: i32,
    id: Option<i32>,
    messages: Vec<Message>,
    answer: &str,
) {
    out.send(Event::Status("answering")).await;
    out.text_answer(answer).await;
    out.send(Event::Done).await;
    save(pool, user_id, id, messages, answer).await;
}

/// Asks which the user means instead of guessing: the question is plain
/// answer text (what a browser without the `clarify` event shows), and the
/// saved history keeps the question it asks about.
async fn ask_to_clarify(
    out: &Sender,
    pool: &MySqlPool,
    user_id: i32,
    id: Option<i32>,
    messages: Vec<Message>,
    question: &str,
) {
    out.send(Event::Status("answering")).await;
    out.text_answer(crm_manual::CLARIFY_QUESTION).await;
    out.json("clarify", &crm_manual::CLARIFY_CHOICES).await;
    out.send(Event::Done).await;
    let asked = markers::with_clarify_marker(crm_manual::CLARIFY_QUESTION, question);
    save(pool, user_id, id, messages, &asked).await;
}

/// Asks which of several features the user means, like [`ask_to_clarify`]:
/// the choices' labels are the `clarify` event (all answered from the
/// manual), and the saved history keeps each one's sections.
async fn ask_which_feature(
    out: &Sender,
    pool: &MySqlPool,
    user_id: i32,
    id: Option<i32>,
    messages: Vec<Message>,
    question: &str,
    clarification: &crm_manual::Clarification,
) {
    out.send(Event::Status("answering")).await;
    out.text_answer(&clarification.question).await;
    out.json("clarify", &clarification.choices).await;
    out.send(Event::Done).await;
    let choices: Vec<(&str, &[String])> = clarification
        .choices
        .iter()
        .map(|c| (c.label.as_str(), c.sections.as_slice()))
        .collect();
    let asked = markers::with_clarify_choices_marker(&clarification.question, question, &choices);
    save(pool, user_id, id, messages, &asked).await;
}

const fn ctx(user: &SessionUser) -> assistant::Ctx {
    assistant::Ctx {
        company_id: user.company_id,
        user_id: user.id,
    }
}

/// `query.slice(0, 200)` for logs.
pub fn short(query: &str) -> String {
    js_slice(trim(query), 0, 200)
}

#[cfg(test)]
mod tests {
    use super::crm_manual::{CLARIFY_QUESTION, Source};
    use super::{
        Clarified, DONE, Message, Mode, clarified, earlier_questions, markers, mode_for,
        picked_clarification, prompts, request_messages,
    };

    const ASKED: &str = "how do I make a special order?";

    /// A new chat that was asked back, and the user's reply.
    fn asked_back(reply: &str) -> Vec<Message> {
        vec![
            Message::new(
                "system",
                "Here are the instructions you can use, each TITLE is linked to its own BODY:",
            ),
            Message::new("system", "CRM USER MANUAL:\n\n## Quotes"),
            Message::new("user", prompts::instruction_user_message(ASKED)),
            Message::new(
                "assistant",
                markers::with_clarify_marker(CLARIFY_QUESTION, ASKED),
            ),
            Message::new("user", reply),
        ]
    }

    #[test]
    fn picking_a_choice_answers_the_question_asked_from_that_source() {
        for (reply, source, gone) in [
            (
                "In Granite Manager",
                Source::Manual,
                "Here are the instructions",
            ),
            ("app", Source::Manual, "Here are the instructions"),
            ("Our company's process", Source::Company, "CRM USER MANUAL:"),
            ("company", Source::Company, "CRM USER MANUAL:"),
        ] {
            let messages = asked_back(reply);
            let (question, picked) = picked_clarification(&messages).expect(reply);
            assert_eq!(
                (question.as_str(), &picked),
                (ASKED, &Clarified::Source(source)),
                "{reply}"
            );

            let next = clarified(messages, &question, &picked);
            let last = next.last().expect("a question");
            assert_eq!(last.role, "user");
            assert!(
                last.content
                    .contains("Question: how do I make a special order? (I mean")
            );
            assert!(next.iter().all(|m| !m.content.contains("[[CLARIFY:")));
            assert!(next.iter().all(|m| !m.content.contains(gone)), "{reply}");
            assert_eq!(next.iter().filter(|m| m.role == "user").count(), 1);
        }
    }

    /// "How to turn off notifications" was asked back with the kinds there are.
    fn asked_which(reply: &str) -> Vec<Message> {
        let asked = "How to turn off notifications";
        let bell = vec!["notifications".to_string()];
        let telegram = vec![
            "notifications".to_string(),
            "my-account-telegram".to_string(),
        ];
        vec![
            Message::new(
                "system",
                "Here are the instructions you can use, each TITLE is linked to its own BODY:",
            ),
            Message::new("user", prompts::instruction_user_message(asked)),
            Message::new(
                "assistant",
                markers::with_clarify_choices_marker(
                    "Which notifications do you mean?",
                    asked,
                    &[
                        ("Bell in the header", bell.as_slice()),
                        ("Telegram alerts", telegram.as_slice()),
                    ],
                ),
            ),
            Message::new("user", reply),
        ]
    }

    #[test]
    fn picking_a_feature_answers_the_question_from_its_sections() {
        for reply in ["Telegram alerts", "telegram", "2"] {
            let messages = asked_which(reply);
            let (question, picked) = picked_clarification(&messages).expect(reply);
            assert_eq!(question, "How to turn off notifications");
            let Clarified::Feature(feature) = &picked else {
                panic!("{reply}: {picked:?}")
            };
            assert_eq!(feature.label, "Telegram alerts");
            assert_eq!(feature.sections, ["notifications", "my-account-telegram"]);

            let next = clarified(messages, &question, &picked);
            assert!(next.last().unwrap().content.contains(
                "Question: How to turn off notifications (I mean Telegram alerts in Granite Manager.)"
            ));
            assert!(next.iter().all(|m| !m.content.contains("[[CLARIFY:")));
            assert!(
                next.iter()
                    .all(|m| !m.content.contains("Here are the instructions"))
            );
        }
        // Not one of the choices: a new question.
        assert!(picked_clarification(&asked_which("how do I add a customer?")).is_none());
        assert!(picked_clarification(&asked_which("company")).is_none());
    }

    /// One question of three: two features and the company's own process.
    #[test]
    fn the_company_s_process_is_a_choice_of_the_same_question() {
        let asked = "how do I make a special order?";
        let quote = vec!["quote-drawing-color-slab-layout".to_string()];
        let calculator = vec!["samples-special-order".to_string()];
        let company: Vec<String> = Vec::new();
        let marker = markers::with_clarify_choices_marker(
            "Which one?",
            asked,
            &[
                ("Special-order slab in a quote", quote.as_slice()),
                ("Special Order calculator", calculator.as_slice()),
                ("Our company's process", company.as_slice()),
            ],
        );
        assert!(marker.ends_with(
            "[[CLARIFY:how do I make a special order?|Special-order slab in a quote=quote-drawing-color-slab-layout|Special Order calculator=samples-special-order|Our company's process]]"
        ));
        let with_reply = |reply: &str| {
            vec![
                Message::new("user", prompts::instruction_user_message(asked)),
                Message::new("assistant", marker.clone()),
                Message::new("user", reply),
            ]
        };
        for (reply, picked) in [
            ("Our company's process", Clarified::Source(Source::Company)),
            ("company", Clarified::Source(Source::Company)),
            ("In Granite Manager", Clarified::Source(Source::Manual)),
            (
                "Special Order calculator",
                Clarified::Feature(super::crm_manual::feature(
                    "Special Order calculator",
                    calculator.clone(),
                )),
            ),
        ] {
            assert_eq!(
                picked_clarification(&with_reply(reply)).map(|(_, p)| p),
                Some(picked),
                "{reply}"
            );
        }
    }

    #[test]
    fn a_follow_up_knows_the_answer_before_came_from_the_manual() {
        let manual = vec![
            Message::new("user", prompts::manual_user_message("how do I add a stone")),
            Message::new("assistant", "Open Inventory > Stones."),
            Message::new("user", "there is no such button"),
        ];
        assert!(super::last_answer_from_manual(&manual));
        let company = vec![
            Message::new(
                "user",
                prompts::instruction_user_message("what is our deposit"),
            ),
            Message::new("assistant", "50%."),
            Message::new("user", "and for quartz?"),
        ];
        assert!(!super::last_answer_from_manual(&company));
        assert!(!super::last_answer_from_manual(&manual[..1]));
    }

    #[test]
    fn earlier_questions_are_what_the_user_typed() {
        let messages = vec![
            Message::new("system", "Here are the instructions you can use"),
            Message::new(
                "user",
                prompts::instruction_user_message("how do I add a stone"),
            ),
            Message::new("assistant", "Open Inventory > Stones."),
            Message::new("user", "and delete one?"),
        ];
        assert_eq!(earlier_questions(&messages), ["how do I add a stone"]);
        assert!(earlier_questions(&messages[..2]).is_empty());
    }

    #[test]
    fn how_to_questions_about_records_still_go_to_the_manual() {
        for query in [
            "can i delete a deal?",
            "is there a way to export customers to excel",
            "turn off email notifications",
            "where are call recordings",
            "cant hear the customer on calls",
            "can I send pictures by text",
        ] {
            assert_eq!(
                mode_for(Some("instructions"), query, true),
                Mode::Instructions,
                "{query}"
            );
        }
        for query in [
            "how many deals did we win this month",
            "find customer Alice Lead",
        ] {
            assert_eq!(
                mode_for(Some("instructions"), query, true),
                Mode::Assistant,
                "{query}"
            );
        }
    }

    #[test]
    fn a_new_question_after_the_clarification_is_answered_as_itself() {
        assert!(picked_clarification(&asked_back("how do I add a customer?")).is_none());
        let mut no_clarification = asked_back("app");
        no_clarification[3] = Message::new("assistant", "Open Quotes.");
        assert!(picked_clarification(&no_clarification).is_none());
    }

    #[test]
    fn the_model_never_sees_the_clarify_marker() {
        let sent = request_messages(&asked_back("app"));
        assert_eq!(sent[3]["content"], CLARIFY_QUESTION);
    }

    #[test]
    fn only_employees_get_the_assistant() {
        let query = "find customer Alice Lead";
        assert!(prompts::looks_like_assistant_query(query));
        assert_eq!(mode_for(None, query, true), Mode::Assistant);
        assert_eq!(mode_for(None, query, false), Mode::Instructions);
        assert_eq!(mode_for(Some("assistant"), "hi", false), Mode::Instructions);
        assert_eq!(mode_for(Some("priceLists"), query, false), Mode::PriceLists);
    }

    /// The browser stops streaming only on this exact string (`DONE_KEY` in
    /// `app/utils/constants.ts`). Any other marker shows up in the answer text.
    #[test]
    fn done_marker_is_the_browser_key() {
        assert_eq!(DONE, "[DONE]035d8eba-9f8c-44c5-a1e0-290d1da033f7[/DONE]");
    }
}
