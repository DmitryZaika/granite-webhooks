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
    /// `specialOrderOffer`.
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
/// when `records` allows it, see [`may_use_assistant`]).
pub fn mode_for(mode: Option<&str>, query: &str, records: bool) -> Mode {
    match mode {
        Some("priceLists") => Mode::PriceLists,
        Some("assistant") if records => Mode::Assistant,
        _ if records && prompts::looks_like_assistant_query(query) => Mode::Assistant,
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

/// The model's messages.
fn request_messages(messages: &[Message]) -> Vec<Value> {
    messages
        .iter()
        .map(|m| json!({ "role": m.role, "content": m.content }))
        .collect()
}

/// `completeChatAnswer`: the answer streamed as it is written, holding back
/// the last 64 characters so a marker is never shown half-written.
async fn complete(
    state: &AppState,
    user: &SessionUser,
    messages: &[Message],
    out: &Sender,
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
        let mut chat = Chat::new(MINI_MODEL).max_tokens(Some(1024));
        chat.messages = request_messages(messages);
        chat.timeout = Duration::from_secs(120);
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
    let mode = mode_for(
        request.mode.as_deref(),
        &request.query,
        may_use_assistant(&user),
    );
    if let Err(err) = run(&state, &user, &request, mode, &out).await {
        tracing::error!(error = %err, ?mode, "AI chat request failed");
        let message = if err.is_model_permission() {
            "Chat is temporarily unavailable because the AI model is not enabled for this workspace. Please try again later.".to_string()
        } else {
            format!("Error: {err}")
        };
        out.send(Event::Text(message)).await;
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
                messages = with_instruction_context(messages, &all, query);
                loaded = Some(all);
            }
        }
    }

    let how_to = crm_manual::looks_like_how_to(query);
    if mode == Mode::Instructions {
        let access = crm_manual::access(pool, user).await?;
        let search: String = std::iter::once(query)
            .chain(
                messages
                    .iter()
                    .filter(|m| m.role == "user")
                    .map(|m| m.content.as_str()),
            )
            .collect::<Vec<_>>()
            .join(" ");
        let manual = crm_manual::content_for(pool, &search).await;
        messages = with_crm_manual(messages, &manual, &crm_manual::access_block(&access));
        if let Some(quick) = crm_manual::quick_answer(query, Some(&access)) {
            finish_with(out, pool, user_id, history_id, messages, &quick).await;
            return Ok(());
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
        let selected = instructions::select_for_context(&all, query);
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

    let mut answer = complete(state, user, &messages, out).await?;

    if mode == Mode::Instructions
        && !how_to
        && markers::instruction_answer_needs_another_look(&answer)
    {
        let exclude = instructions::ids_to_exclude_for_retry(&selected, &answer, query);
        let rest: Vec<Instruction> = all_instructions
            .iter()
            .filter(|i| !exclude.contains(&i.id))
            .cloned()
            .collect();
        let leftover = instructions::select_for_context(&rest, query);
        if leftover.iter().any(|i| instructions::is_related(i, query)) {
            out.send(Event::Replace).await;
            messages = with_instruction_context(messages, &leftover, query);
            selected = leftover;
            answer = complete(state, user, &messages, out).await?;
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
        let mut cited = instructions::used_for_reply(&selected, &answer, query);
        if cited.is_empty()
            && markers::parse_instruction_indices(&answer) != Some(None)
            && let Some(best) = instructions::best_matching(&selected, query)
        {
            cited = vec![best];
        }
        if let Some(content) = instructions::matching_answer(&selected, &clean, query)
            && !cited.iter().any(|u| u.id == content.id)
        {
            cited = vec![content];
        }
        if !cited.is_empty() {
            out.json("instructions", &cited).await;
            images = instructions::images_for_used(&selected, &cited, &clean, query);
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
    use super::{DONE, Mode, mode_for, prompts};

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
