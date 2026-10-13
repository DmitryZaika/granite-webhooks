//! Assistant mode: questions about the company's own CRM records.
//!
//! The model answers by calling skills (look up a customer, list their
//! deals, search deal notes, draft an email), chaining them as the task
//! needs. Port of the CRM's `lib/assistant` (`runSkillLoop`, `runAssistant`,
//! the identity block and the four available skills; `query_database` is
//! switched off there and is not here).

use crate::ai::{AiError, Caller, MINI_FALLBACK_MODEL, MINI_MODEL};
use crate::followups::js::trim;
use crate::re;
use crate::routes::customers::fuzzy;
use crate::state::AppState;
use serde::Serialize;
use serde_json::{Map, Value, json};
use std::sync::LazyLock;
use std::time::Duration;

pub const SYSTEM_PREFIX: &str = "You are the AI assistant inside Granite Manager";
/// Histories saved before the skill system start with this.
pub const OLD_SYSTEM_PREFIX: &str = "You are a helpful CRM assistant";
const MAX_ROUNDS: usize = 8;
const MAX_TEAMMATES: usize = 40;

/// The tools as the CRM sends them (zod schemas → JSON Schema).
static TOOLS: LazyLock<Vec<Value>> = LazyLock::new(|| {
    serde_json::from_str(include_str!("assistant_tools.json"))
        .expect("assistant_tools.json is valid")
});

const CATALOG: &str = "customers:
- lookup_customer: Find a customer by name, phone number, or email. Returns name, company, phone numbers, emails, street address, postal code, ids, and a short deal summary.
deals:
- get_customer_deals: List the deals of one customer with stage, amount, title, outcome, and each deal recent activities and notes. Requires a customer id.
- search_deal_notes: Search deal notes across all customers by text, scoped to the current user by default. Use for \"which customers have a note about X\".
email:
- draft_email: Prepare an email draft for a customer and give the user a one-click link to open it. Does NOT send anything.";

/// Who is asking.
#[derive(Debug, Clone, Copy)]
pub struct Ctx {
    pub company_id: i32,
    pub user_id: i32,
}

/// A link the chat shows under the answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Action {
    #[serde(rename = "type")]
    pub kind: String,
    pub label: String,
    pub url: String,
}

/// What the assistant did, shown under the answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Trace {
    pub kind: String,
    pub detail: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub row_count: Option<i64>,
    pub skill: String,
}

/// The assistant's answer.
#[derive(Debug, Clone, Default)]
pub struct RunResult {
    pub answer: String,
    pub actions: Vec<Action>,
    pub traces: Vec<Trace>,
}

#[derive(sqlx::FromRow)]
struct UserRow {
    id: i32,
    name: Option<String>,
    email: String,
    is_admin: Option<i8>,
    is_superuser: Option<i8>,
    is_employee: Option<i8>,
}

struct Member {
    id: i32,
    name: Option<String>,
    email: String,
    role: &'static str,
}

fn role_of(row: &UserRow) -> &'static str {
    if row.is_superuser.unwrap_or(0) != 0 {
        "superuser"
    } else if row.is_admin.unwrap_or(0) != 0 {
        "admin"
    } else if row.is_employee.unwrap_or(0) != 0 {
        "employee"
    } else {
        "user"
    }
}

struct Identity {
    me: Member,
    company_id: i32,
    company_name: Option<String>,
    teammates: Vec<Member>,
    truncated: bool,
    today: String,
}

async fn identity(state: &AppState, ctx: Ctx) -> sqlx::Result<Identity> {
    let users: Vec<UserRow> = sqlx::query_as(
        "SELECT id, name, email, CAST(is_admin AS SIGNED) AS is_admin, CAST(is_superuser AS SIGNED) AS is_superuser,
                CAST(is_employee AS SIGNED) AS is_employee
           FROM users WHERE company_id = ? AND is_deleted = 0 ORDER BY id",
    )
    .bind(ctx.company_id)
    .fetch_all(&state.pool)
    .await?;
    let company_name: Option<String> = sqlx::query_scalar("SELECT name FROM company WHERE id = ?")
        .bind(ctx.company_id)
        .fetch_optional(&state.pool)
        .await?;
    let member = |row: &UserRow| Member {
        id: row.id,
        name: row.name.clone(),
        email: row.email.clone(),
        role: role_of(row),
    };
    let me = users.iter().find(|u| u.id == ctx.user_id).map_or_else(
        || Member {
            id: ctx.user_id,
            name: None,
            email: "unknown".into(),
            role: "user",
        },
        member,
    );
    let others: Vec<&UserRow> = users.iter().filter(|u| u.id != ctx.user_id).collect();
    Ok(Identity {
        me,
        company_id: ctx.company_id,
        company_name,
        teammates: others
            .iter()
            .take(MAX_TEAMMATES)
            .map(|u| member(u))
            .collect(),
        truncated: others.len() > MAX_TEAMMATES,
        today: chrono::Utc::now().format("%Y-%m-%d").to_string(),
    })
}

fn display_name(member: &Member) -> String {
    member
        .name
        .as_deref()
        .map(trim)
        .filter(|n| !n.is_empty())
        .map_or_else(|| member.email.clone(), str::to_string)
}

/// `buildIdentityBlock`.
fn identity_block(identity: &Identity) -> String {
    let me = display_name(&identity.me);
    let workspace = identity.company_name.as_ref().map_or_else(
        || format!("company_id {}", identity.company_id),
        |name| format!("{name} (company_id {})", identity.company_id),
    );
    let roster = if identity.teammates.is_empty() {
        "TEAM: no other employees in this workspace.".to_string()
    } else {
        let mut lines = vec![format!(
            "TEAM (employees of this workspace — the `users` table{}):",
            if identity.truncated {
                ", truncated; query `users` for the rest"
            } else {
                ", complete"
            }
        )];
        lines.extend(identity.teammates.iter().map(|m| {
            format!(
                "  id {} — {} <{}> ({})",
                m.id,
                display_name(m),
                m.email,
                m.role
            )
        }));
        lines.join("\n")
    };
    format!(
        "WHO YOU ARE TALKING TO
  {me} <{email}> — {role}, users.id {id}.
  Workspace: {workspace}. Today is {today}.

{roster}

RESOLVING PEOPLE — these rules are absolute:
- \"I\", \"me\", \"my\", \"mine\" always mean {me}, users.id {id}.
- In SQL write the placeholder :user_id for that id and :company_id for the workspace; the server substitutes both before the query runs. Never write either id as a literal, and never ask the user for them.
- A question about a person NEVER gets answered by matching their name against a text column (sender_email, display_name, subject, body, ...). Resolve the person to an id first — from the roster above for employees, with lookup_customer for customers — then filter on that id.
- If a name matches nobody in the roster and no customer, say so instead of guessing.

VOCABULARY — these four are different things and are a common source of wrong answers:
- `users` = EMPLOYEES of this workspace (the people listed above). Your colleagues.
- `customers` = CLIENTS. A question about \"clients\"/\"customers\" is NEVER answered from `users`.
- `company` = the CRM tenant, i.e. this workspace itself. Fixed for the whole conversation; the user never means this when they say \"company\" casually.
- `customers.company_name` = the business a client works for. This IS what \"group my customers by company\" means.",
        email = identity.me.email,
        role = identity.me.role,
        id = identity.me.id,
        today = identity.today,
    )
}

fn build_system_prompt(identity_block: Option<&str>) -> String {
    format!(
        "{SYSTEM_PREFIX}, a CRM for the countertop industry.
You complete the user's request by calling skills (tools), chaining as many as the task needs.
{}
SKILL CATALOG (every skill you can use — name: purpose):
{CATALOG}

How to work:
1. Read the request and pick the skills from the catalog that the task needs. Multi-step tasks are normal: gather each fact with the right skill, then combine the results. Example: \"email Clark Kent the price of a stone\" means look up the price with a pricing skill, find Clark Kent with a customer skill, then draft the email with an email skill using both results.
2. Skills from the catalog that are not attached as tools yet must be activated first: call activate_skills with their exact names, then call them normally. Activate everything you expect to need in one call.
3. Only state facts that come from skill results. Never invent customer names, phone numbers, emails, prices, or deal details.
4. When a skill returns several candidates (for example multiple matching customers), do not guess: list the top matches with one short distinguishing detail each and ask the user which one they mean.
5. When a required fact is missing and no skill can provide it, ask the user instead of assuming. Never ask the user for technical details you can look up yourself (database schema, table or column names, ids) — find them with the skills. Before telling the user something cannot be answered, check the catalog again for a skill that could.
6. Be concise and direct. Plain text only (no markdown).",
        identity_block.map_or_else(String::new, |b| format!("\n{b}\n"))
    )
}

/// The system prompt for a given asker and roster (for checks against the
/// CRM's prompt).
#[doc(hidden)]
#[allow(clippy::too_many_arguments)]
pub fn render_system_prompt(
    me: (i32, Option<&str>, &str, &'static str),
    company_id: i32,
    company_name: Option<&str>,
    teammates: &[(i32, Option<&str>, &str, &'static str)],
    truncated: bool,
    today: &str,
) -> String {
    let member = |m: &(i32, Option<&str>, &str, &'static str)| Member {
        id: m.0,
        name: m.1.map(str::to_string),
        email: m.2.to_string(),
        role: m.3,
    };
    build_system_prompt(Some(&identity_block(&Identity {
        me: member(&me),
        company_id,
        company_name: company_name.map(str::to_string),
        teammates: teammates.iter().map(member).collect(),
        truncated,
        today: today.to_string(),
    })))
}

/// `getAssistantSystemPrompt`: without the roster when it cannot be read.
pub async fn system_prompt(state: &AppState, ctx: Ctx) -> String {
    match identity(state, ctx).await {
        Ok(identity) => build_system_prompt(Some(&identity_block(&identity))),
        Err(err) => {
            lambda_http::tracing::warn!(error = %err, "assistant identity load failed");
            build_system_prompt(None)
        }
    }
}

// ---------- skills ----------

/// A skill's result: what the model reads, and links for the user.
struct SkillOutput {
    data: Value,
    actions: Vec<Action>,
}

const fn data(value: Value) -> SkillOutput {
    SkillOutput {
        data: value,
        actions: Vec::new(),
    }
}

/// zod-like argument errors the model can fix.
fn invalid(name: &str, issues: &[(&str, String)]) -> String {
    json!({
        "error": format!("Invalid arguments for {name}."),
        "issues": issues.iter().map(|(path, message)| json!({ "path": path, "message": message })).collect::<Vec<_>>(),
    })
    .to_string()
}

/// A trimmed string argument of `min..=max` UTF-16 units.
fn string_arg(
    args: &Value,
    key: &str,
    min: usize,
    max: usize,
    required: bool,
) -> Result<Option<String>, (String, String)> {
    match args.get(key) {
        None | Some(Value::Null) if !required => Ok(None),
        Some(Value::String(s)) => {
            let s = trim(s).to_string();
            let len = s.encode_utf16().count();
            if len < min {
                Err((
                    key.into(),
                    format!("Too small: expected string to have >={min} characters"),
                ))
            } else if len > max {
                Err((
                    key.into(),
                    format!("Too big: expected string to have <={max} characters"),
                ))
            } else {
                Ok(Some(s))
            }
        }
        None | Some(Value::Null) => Err((
            key.into(),
            "Invalid input: expected string, received undefined".into(),
        )),
        Some(_) => Err((key.into(), "Invalid input: expected string".into())),
    }
}

fn int_arg(args: &Value, key: &str) -> Option<f64> {
    args.get(key)
        .and_then(Value::as_f64)
        .filter(|n| n.fract() == 0.0)
}

#[allow(clippy::too_many_lines)]
async fn lookup_customer(state: &AppState, ctx: Ctx, query: &str) -> sqlx::Result<Value> {
    let trimmed = trim(query).to_string();
    let digits: String = trimmed.chars().filter(char::is_ascii_digit).collect();
    let kind = if trimmed.contains('@') {
        "email"
    } else if digits.len() >= 7 && re!(r"^[\d\s()+.-]*$").is_match(&trimmed) {
        "phone"
    } else {
        "name"
    };
    let pool = &state.pool;
    let ids: Vec<i64> = match kind {
        "name" => fuzzy::find_by_name(pool, ctx.company_id, &trimmed, 20)
            .await?
            .into_iter()
            .map(|c| i64::from(c.id))
            .collect(),
        "email" => sqlx::query_scalar(
            "SELECT DISTINCT CAST(c.id AS SIGNED), c.name FROM customers c
               JOIN customers_emails ce ON ce.customer_id = c.id
              WHERE c.company_id = ? AND c.deleted_at IS NULL AND ce.email LIKE ?
              ORDER BY c.name LIMIT 20",
        )
        .bind(ctx.company_id)
        .bind(format!("%{trimmed}%"))
        .fetch_all(pool)
        .await?,
        _ => sqlx::query_scalar(
            "SELECT DISTINCT CAST(c.id AS SIGNED), c.name FROM customers c
              WHERE c.company_id = ? AND c.deleted_at IS NULL AND (
                REPLACE(REPLACE(REPLACE(REPLACE(c.phone, ' ', ''), '(', ''), ')', ''), '-', '') LIKE ?
                OR REPLACE(REPLACE(REPLACE(REPLACE(c.phone_2, ' ', ''), '(', ''), ')', ''), '-', '') LIKE ?)
              ORDER BY c.name LIMIT 20",
        )
        .bind(ctx.company_id)
        .bind(format!("%{digits}%"))
        .bind(format!("%{digits}%"))
        .fetch_all(pool)
        .await?,
    };
    if ids.is_empty() {
        return Ok(
            json!({ "query": trimmed, "kind": kind, "total": 0, "selected": null, "candidates": [] }),
        );
    }
    let marks = vec!["?"; ids.len()].join(", ");
    let bind_ids = |sql: String| {
        let mut q = sqlx::query_as::<_, CandidateRow>(sqlx::AssertSqlSafe(sql));
        for id in &ids {
            q = q.bind(id);
        }
        q
    };
    let rows: Vec<CandidateRow> = bind_ids(format!(
        "SELECT CAST(c.id AS SIGNED) AS id, COALESCE(c.name, '') AS name, c.company_name, c.phone, c.phone_2,
                c.address, c.postal_code, pe.email AS email, CAST(c.sales_rep AS SIGNED) AS sales_rep
           FROM customers c
           LEFT JOIN customers_emails pe ON pe.id = c.email_id AND pe.customer_id = c.id
          WHERE c.id IN ({marks})"
    ))
    .fetch_all(pool)
    .await?;
    let mut emails_q = sqlx::query_as::<_, (i64, String)>(sqlx::AssertSqlSafe(format!(
        "SELECT CAST(customer_id AS SIGNED), email FROM customers_emails WHERE customer_id IN ({marks}) ORDER BY id ASC"
    )));
    for id in &ids {
        emails_q = emails_q.bind(id);
    }
    let emails = emails_q.fetch_all(pool).await?;
    let mut deals_q = sqlx::query_as::<_, DealRow>(sqlx::AssertSqlSafe(format!(
        "SELECT CAST(d.customer_id AS SIGNED) AS customer_id, CAST(d.id AS SIGNED) AS id, CAST(d.amount AS CHAR) AS amount,
                d.title, CAST(d.is_won AS SIGNED) AS is_won, CAST(d.user_id AS SIGNED) AS user_id,
                DATE_FORMAT(d.created_at, '%Y-%m-%dT%H:%i:%sZ') AS created_at, dl.name AS stage_name
           FROM deals d
           LEFT JOIN deals_list dl ON dl.id = d.list_id AND dl.deleted_at IS NULL
          WHERE d.customer_id IN ({marks}) AND d.deleted_at IS NULL
          ORDER BY d.created_at DESC, d.id DESC"
    )));
    for id in &ids {
        deals_q = deals_q.bind(id);
    }
    let deals = deals_q.fetch_all(pool).await?;
    let mut candidates: Vec<Candidate> = ids
        .iter()
        .filter_map(|id| rows.iter().find(|r| r.id == *id))
        .map(|row| {
            let mine: Vec<String> = emails.iter().filter(|e| e.0 == row.id).take(6).map(|e| e.1.clone()).collect();
            let mut open = None;
            let (mut won_count, mut lost_count, mut last): (i64, i64, Option<String>) = (0, 0, None);
            for deal in deals.iter().filter(|d| d.customer_id == row.id) {
                if last.is_none() {
                    last.clone_from(&deal.created_at);
                }
                match deal.is_won {
                    Some(1) => won_count += 1,
                    Some(0) => lost_count += 1,
                    _ if open.is_none() => {
                        open = Some(json!({
                            "id": deal.id, "stage": deal.stage_name, "amount": deal.amount, "title": deal.title,
                            "assigned_to_you": deal.user_id == Some(i64::from(ctx.user_id)),
                        }));
                    }
                    _ => {}
                }
            }
            Candidate {
                value: json!({
                    "id": row.id, "name": row.name, "company_name": row.company_name, "phone": row.phone,
                    "phone_2": row.phone_2, "email": row.email.clone().or_else(|| mine.first().cloned()), "emails": mine,
                    "address": row.address, "postal_code": row.postal_code, "open_deal": open,
                    "won_count": won_count, "lost_count": lost_count,
                    "assigned_to_you": row.sales_rep == Some(i64::from(ctx.user_id)),
                    "last_deal_at": last,
                }),
            }
        })
        .collect();
    candidates.sort_by(|a, b| {
        a.priority()
            .cmp(&b.priority())
            .then_with(|| {
                b.value["assigned_to_you"]
                    .as_bool()
                    .cmp(&a.value["assigned_to_you"].as_bool())
            })
            .then_with(|| b.last().cmp(a.last()))
            .then_with(|| a.name().to_lowercase().cmp(&b.name().to_lowercase()))
    });
    let total = candidates.len();
    let ranked: Vec<Value> = candidates.into_iter().map(|c| c.value).collect();
    Ok(json!({
        "query": trimmed,
        "kind": kind,
        "total": total,
        "selected": if total == 1 { ranked[0].clone() } else { Value::Null },
        "candidates": if total == 1 { Vec::new() } else { ranked.into_iter().take(6).collect() },
    }))
}

#[derive(sqlx::FromRow)]
struct CandidateRow {
    id: i64,
    name: String,
    company_name: Option<String>,
    phone: Option<String>,
    phone_2: Option<String>,
    address: Option<String>,
    postal_code: Option<String>,
    email: Option<String>,
    sales_rep: Option<i64>,
}

#[derive(sqlx::FromRow)]
struct DealRow {
    customer_id: i64,
    id: i64,
    amount: Option<String>,
    title: Option<String>,
    is_won: Option<i64>,
    user_id: Option<i64>,
    created_at: Option<String>,
    stage_name: Option<String>,
}

struct Candidate {
    value: Value,
}

impl Candidate {
    /// `rankCustomerCandidates` tiers.
    fn priority(&self) -> u8 {
        let open = &self.value["open_deal"];
        if open["assigned_to_you"].as_bool() == Some(true) {
            0
        } else if !open.is_null() {
            1
        } else if self.value["won_count"].as_i64() > Some(0) {
            2
        } else if self.value["lost_count"].as_i64() > Some(0) {
            3
        } else {
            4
        }
    }

    fn last(&self) -> &str {
        self.value["last_deal_at"].as_str().unwrap_or_default()
    }

    fn name(&self) -> &str {
        self.value["name"].as_str().unwrap_or_default()
    }
}

async fn customer_deals(state: &AppState, ctx: Ctx, customer_id: i64) -> sqlx::Result<Value> {
    #[derive(sqlx::FromRow)]
    struct Deal {
        id: i64,
        title: Option<String>,
        amount: Option<String>,
        is_won: Option<i64>,
        stage_name: Option<String>,
        created_at: Option<String>,
        assigned_user_id: Option<i64>,
    }
    let deals: Vec<Deal> = sqlx::query_as(
        "SELECT CAST(d.id AS SIGNED) AS id, d.title, CAST(d.amount AS CHAR) AS amount, CAST(d.is_won AS SIGNED) AS is_won,
                dl.name AS stage_name, DATE_FORMAT(d.created_at, '%Y-%m-%dT%H:%i:%sZ') AS created_at,
                CAST(d.user_id AS SIGNED) AS assigned_user_id
           FROM deals d
           JOIN customers c ON c.id = d.customer_id AND c.company_id = ? AND c.deleted_at IS NULL
           LEFT JOIN deals_list dl ON dl.id = d.list_id AND dl.deleted_at IS NULL
          WHERE d.customer_id = ? AND d.deleted_at IS NULL
          ORDER BY d.created_at DESC, d.id DESC
          LIMIT 20",
    )
    .bind(ctx.company_id)
    .bind(customer_id)
    .fetch_all(&state.pool)
    .await?;
    if deals.is_empty() {
        return Ok(json!({ "customer_id": customer_id, "total_returned": 0, "deals": [] }));
    }
    let marks = vec!["?"; deals.len()].join(", ");
    let mut activities_q = sqlx::query_as::<_, (i64, i64, String, Option<String>, Option<String>, i64, Option<String>)>(
        sqlx::AssertSqlSafe(format!(
            "SELECT CAST(id AS SIGNED), CAST(deal_id AS SIGNED), name,
                    DATE_FORMAT(deadline, '%Y-%m-%dT%H:%i:%s') AS deadline, priority, CAST(is_completed AS SIGNED),
                    DATE_FORMAT(created_at, '%Y-%m-%dT%H:%i:%sZ') AS created_at
               FROM deal_activities
              WHERE deal_id IN ({marks}) AND company_id = ? AND deleted_at IS NULL
              ORDER BY is_completed ASC, deadline IS NULL ASC, deadline ASC, created_at DESC"
        )),
    );
    let mut notes_q =
        sqlx::query_as::<_, (i64, i64, String, i64, Option<String>)>(sqlx::AssertSqlSafe(format!(
        "SELECT CAST(id AS SIGNED), CAST(deal_id AS SIGNED), content, CAST(is_pinned AS SIGNED),
                DATE_FORMAT(created_at, '%Y-%m-%dT%H:%i:%sZ') AS created_at
           FROM deal_notes
          WHERE deal_id IN ({marks}) AND company_id = ? AND deleted_at IS NULL
          ORDER BY is_pinned DESC, created_at DESC"
    )));
    for deal in &deals {
        activities_q = activities_q.bind(deal.id);
        notes_q = notes_q.bind(deal.id);
    }
    let activities = activities_q
        .bind(ctx.company_id)
        .fetch_all(&state.pool)
        .await?;
    let notes = notes_q.bind(ctx.company_id).fetch_all(&state.pool).await?;
    let deals: Vec<Value> = deals
        .iter()
        .map(|deal| {
            let mine: Vec<_> = activities.iter().filter(|a| a.1 == deal.id).collect();
            let deal_notes: Vec<_> = notes.iter().filter(|n| n.1 == deal.id).collect();
            json!({
                "id": deal.id, "title": deal.title, "amount": deal.amount, "is_won": deal.is_won,
                "stage_name": deal.stage_name, "created_at": deal.created_at, "assigned_user_id": deal.assigned_user_id,
                "assigned_to_you": deal.assigned_user_id == Some(i64::from(ctx.user_id)),
                "activity_count": mine.len(),
                "open_activity_count": mine.iter().filter(|a| a.5 == 0).count(),
                "activities": mine.iter().take(5).map(|a| json!({
                    "id": a.0, "name": a.2, "deadline": a.3, "priority": a.4, "is_completed": a.5, "created_at": a.6,
                })).collect::<Vec<_>>(),
                "note_count": deal_notes.len(),
                "notes": deal_notes.iter().take(5).map(|n| json!({
                    "id": n.0, "content": n.2, "is_pinned": n.3, "created_at": n.4,
                })).collect::<Vec<_>>(),
            })
        })
        .collect();
    Ok(json!({ "customer_id": customer_id, "total_returned": deals.len(), "deals": deals }))
}

fn escape_like(term: &str) -> String {
    let mut out = String::with_capacity(term.len());
    for c in term.chars() {
        if matches!(c, '\\' | '%' | '_') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

#[allow(clippy::items_after_statements, clippy::too_many_lines)]
async fn search_notes(
    state: &AppState,
    ctx: Ctx,
    search: Option<String>,
    scope: &str,
    limit: i64,
) -> sqlx::Result<Value> {
    let mut conditions = vec![
        "n.deleted_at IS NULL",
        "d.deleted_at IS NULL",
        "c.deleted_at IS NULL",
        "c.company_id = ?",
        "n.company_id = ?",
    ];
    let mut params: Vec<Value> = vec![json!(ctx.company_id), json!(ctx.company_id)];
    if let Some(search) = &search {
        conditions.push("n.content LIKE ? ESCAPE '\\\\'");
        params.push(json!(format!("%{}%", escape_like(search))));
    }
    let mut author: Option<String> = None;
    if scope == "my_deals" {
        conditions.push("d.user_id = ?");
        params.push(json!(ctx.user_id));
    } else if scope == "written_by_me" {
        let name = identity(state, ctx)
            .await?
            .me
            .name
            .map(|n| trim(&n).to_string())
            .filter(|n| !n.is_empty());
        let Some(name) = name else {
            return Ok(json!({
                "scope": scope, "search": search, "total_matched": 0, "notes": [],
                "note": "Your user record has no name, and note authorship is stored as a name, so no note can be attributed to you. Retry with scope \"my_deals\".",
            }));
        };
        conditions.push("LOWER(TRIM(n.created_by)) = LOWER(?)");
        params.push(json!(name));
        author = Some(name);
    }
    let filter = conditions.join("\n            AND ");
    let from = "FROM deal_notes n JOIN deals d ON d.id = n.deal_id JOIN customers c ON c.id = d.customer_id";
    type NoteRow = (
        i64,
        i64,
        String,
        i64,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<i64>,
        i64,
        Option<String>,
    );
    let mut rows_q = sqlx::query_as::<_, NoteRow>(sqlx::AssertSqlSafe(format!(
        "SELECT CAST(n.id AS SIGNED), CAST(n.deal_id AS SIGNED), n.content, CAST(n.is_pinned AS SIGNED),
                DATE_FORMAT(n.created_at, '%Y-%m-%dT%H:%i:%sZ'), n.created_by, d.title, CAST(d.user_id AS SIGNED),
                CAST(c.id AS SIGNED), c.name
           {from}
          WHERE {filter}
          ORDER BY n.is_pinned DESC, n.created_at DESC, n.id DESC
          LIMIT {limit}"
    )));
    let mut total_q = sqlx::query_scalar::<_, i64>(sqlx::AssertSqlSafe(format!(
        "SELECT COUNT(*) {from} WHERE {filter}"
    )));
    for p in &params {
        match p {
            Value::String(s) => {
                rows_q = rows_q.bind(s.clone());
                total_q = total_q.bind(s.clone());
            }
            other => {
                let n = other.as_i64().unwrap_or_default();
                rows_q = rows_q.bind(n);
                total_q = total_q.bind(n);
            }
        }
    }
    let rows = rows_q.fetch_all(&state.pool).await?;
    let total = total_q
        .fetch_optional(&state.pool)
        .await?
        .unwrap_or_else(|| i64::try_from(rows.len()).unwrap_or_default());
    let returned = i64::try_from(rows.len()).unwrap_or_default();
    let mut out = Map::new();
    out.insert("scope".into(), json!(scope));
    out.insert("search".into(), json!(search));
    if let Some(author) = author {
        out.insert("matched_author_name".into(), json!(author));
    }
    out.insert("total_matched".into(), json!(total));
    out.insert("returned".into(), json!(returned));
    if total > returned {
        out.insert("truncated".into(), json!(true));
        out.insert(
            "note".into(),
            json!(format!(
                "Showing the {returned} most recent of {total} matching notes."
            )),
        );
    }
    out.insert(
        "notes".into(),
        Value::Array(
            rows.into_iter()
                .map(|r| {
                    let units: Vec<u16> = r.2.encode_utf16().collect();
                    let content = if units.len() > 600 {
                        format!("{}…", String::from_utf16_lossy(&units[..600]))
                    } else {
                        r.2.clone()
                    };
                    json!({
                        "note_id": r.0, "content": content, "is_pinned": r.3 == 1, "created_at": r.4,
                        "created_by": r.5.as_deref().map(trim).filter(|s| !s.is_empty()),
                        "deal_id": r.1, "deal_title": r.6, "customer_id": r.8, "customer_name": r.9,
                        "assigned_to_you": r.7 == Some(i64::from(ctx.user_id)),
                    })
                })
                .collect(),
        ),
    );
    Ok(Value::Object(out))
}

/// `encodeURIComponent`.
fn encode_uri_component(text: &str) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    for byte in text.as_bytes() {
        match byte {
            b'A'..=b'Z'
            | b'a'..=b'z'
            | b'0'..=b'9'
            | b'-'
            | b'_'
            | b'.'
            | b'!'
            | b'~'
            | b'*'
            | b'\''
            | b'('
            | b')' => {
                out.push(char::from(*byte));
            }
            _ => {
                let _ = write!(out, "%{byte:02X}");
            }
        }
    }
    out
}

fn is_email(text: &str) -> bool {
    fancy_regex::Regex::new(r"^(?!\.)(?!.*\.\.)([A-Za-z0-9_'+\-\.]*)[A-Za-z0-9_+-]@([A-Za-z0-9][A-Za-z0-9\-]*\.)+[A-Za-z]{2,}$")
        .ok()
        .and_then(|re| re.is_match(text).ok())
        .unwrap_or(false)
}

/// Runs one skill call; the JSON the model reads back.
#[allow(clippy::too_many_lines)]
async fn run_skill(
    state: &AppState,
    ctx: Ctx,
    name: &str,
    args: &Value,
    result: &mut RunResult,
) -> String {
    let outcome: Result<SkillOutput, String> = match name {
        "lookup_customer" => match string_arg(args, "query", 1, 200, true) {
            Ok(Some(query)) => lookup_customer(state, ctx, &query)
                .await
                .map(data)
                .map_err(|e| e.to_string()),
            Ok(None) => unreachable!("required"),
            Err((path, message)) => return invalid(name, &[(&path, message)]),
        },
        "get_customer_deals" => match int_arg(args, "customer_id")
            .filter(|n| *n > 0.0 && *n <= 9_007_199_254_740_991.0)
        {
            #[allow(clippy::cast_possible_truncation)]
            Some(id) => customer_deals(state, ctx, id as i64)
                .await
                .map(data)
                .map_err(|e| e.to_string()),
            None => {
                return invalid(
                    name,
                    &[(
                        "customer_id",
                        "Invalid input: expected a positive integer".into(),
                    )],
                );
            }
        },
        "search_deal_notes" => {
            let search = match string_arg(args, "search", 2, 200, false) {
                Ok(s) => s,
                Err((path, message)) => return invalid(name, &[(&path, message)]),
            };
            let scope = match args.get("scope") {
                None | Some(Value::Null) => "my_deals",
                Some(Value::String(s))
                    if matches!(s.as_str(), "my_deals" | "written_by_me" | "everyone") =>
                {
                    s.as_str()
                }
                Some(_) => {
                    return invalid(
                        name,
                        &[("scope", "Invalid option: expected one of \"my_deals\"|\"written_by_me\"|\"everyone\"".into())],
                    );
                }
            };
            let limit = match args.get("limit") {
                None | Some(Value::Null) => Some(25.0),
                Some(_) => int_arg(args, "limit").filter(|n| (1.0..=100.0).contains(n)),
            };
            let Some(limit) = limit else {
                return invalid(
                    name,
                    &[(
                        "limit",
                        "Invalid input: expected an integer from 1 to 100".into(),
                    )],
                );
            };
            #[allow(clippy::cast_possible_truncation)]
            search_notes(state, ctx, search, scope, limit as i64)
                .await
                .map(data)
                .map_err(|e| e.to_string())
        }
        "draft_email" => {
            let to = match args.get("to_email") {
                Some(Value::String(s)) if is_email(s) => s.clone(),
                _ => return invalid(name, &[("to_email", "Invalid email address".into())]),
            };
            let subject = string_arg(args, "subject", 1, 200, true);
            let body = string_arg(args, "body", 1, 5000, true);
            let mut issues = Vec::new();
            if let Err((p, m)) = &subject {
                issues.push((p.as_str(), m.clone()));
            }
            if let Err((p, m)) = &body {
                issues.push((p.as_str(), m.clone()));
            }
            if !issues.is_empty() {
                return invalid(name, &issues);
            }
            let (subject, body) = (
                subject.ok().flatten().unwrap_or_default(),
                body.ok().flatten().unwrap_or_default(),
            );
            let mailto = format!(
                "mailto:{}?subject={}&body={}",
                encode_uri_component(&to),
                encode_uri_component(&subject),
                encode_uri_component(&body)
            );
            Ok(SkillOutput {
                data: json!({
                    "status": "draft_ready", "to": to, "subject": subject, "body": body,
                    "note": "Draft prepared but NOT sent. The user sees a link that opens it in their email client.",
                }),
                actions: vec![Action {
                    kind: "link".into(),
                    label: format!("Open email draft for {to}"),
                    url: mailto,
                }],
            })
        }
        other => return json!({ "error": format!("Unknown skill: {other}") }).to_string(),
    };
    match outcome {
        Ok(output) => {
            result.actions.extend(output.actions);
            output.data.to_string()
        }
        Err(message) => {
            lambda_http::tracing::warn!(skill = name, error = %message, "assistant skill failed");
            json!({ "error": format!("Skill {name} failed: {message}") }).to_string()
        }
    }
}

/// One round: the model's message, falling back to [`MINI_FALLBACK_MODEL`]
/// (for the rest of the run) when the key may not use the model.
async fn complete(
    state: &AppState,
    caller: Caller<'_>,
    model: &mut &'static str,
    messages: &[Value],
    tools: bool,
) -> Result<Value, AiError> {
    let body = |model: &str| {
        let mut body = json!({ "model": model, "reasoning_effort": "none", "messages": messages });
        if tools {
            body["tools"] = Value::Array(TOOLS.clone());
            body["tool_choice"] = json!("auto");
        }
        body
    };
    match state
        .ai
        .complete(
            &state.pool,
            caller,
            model,
            &body(model),
            Duration::from_secs(60),
        )
        .await
    {
        Err(err) if err.is_model_permission() => {
            *model = MINI_FALLBACK_MODEL;
            state
                .ai
                .complete(
                    &state.pool,
                    caller,
                    model,
                    &body(model),
                    Duration::from_secs(60),
                )
                .await
        }
        other => other,
    }
}

/// `runAssistant`: the skill loop over `messages` (system prompt first).
pub async fn run(
    state: &AppState,
    ctx: Ctx,
    mut messages: Vec<Value>,
) -> Result<RunResult, AiError> {
    let caller = Caller {
        feature: "chat",
        user_id: Some(ctx.user_id),
        company_id: Some(ctx.company_id),
    };
    let mut model = MINI_MODEL;
    let mut result = RunResult::default();
    for _ in 0..MAX_ROUNDS {
        let message = complete(state, caller, &mut model, &messages, true).await?;
        if message.is_null() {
            result.answer = "No response from the model.".into();
            return Ok(result);
        }
        let tool_calls = message["tool_calls"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        if tool_calls.is_empty() {
            let answer = message["content"].as_str().map(trim).unwrap_or_default();
            result.answer = if answer.is_empty() {
                "No answer generated.".into()
            } else {
                answer.to_string()
            };
            return Ok(result);
        }
        messages.push(
            json!({ "role": "assistant", "content": message["content"], "tool_calls": tool_calls }),
        );
        for call in &tool_calls {
            let content = if call["type"] == "function" {
                let name = call["function"]["name"].as_str().unwrap_or_default();
                let raw = call["function"]["arguments"]
                    .as_str()
                    .filter(|a| !a.is_empty())
                    .unwrap_or("{}");
                match serde_json::from_str::<Value>(raw) {
                    Ok(args) => run_skill(state, ctx, name, &args, &mut result).await,
                    Err(_) => json!({ "error": "Tool arguments were not valid JSON." }).to_string(),
                }
            } else {
                json!({ "error": "Unsupported tool call type." }).to_string()
            };
            messages
                .push(json!({ "role": "tool", "tool_call_id": call["id"], "content": content }));
        }
    }
    let mut last = messages.clone();
    last.push(json!({ "role": "user", "content": "Stop calling tools. Give the best final answer from the skill results so far." }));
    let message = complete(state, caller, &mut model, &last, false).await?;
    let answer = message["content"].as_str().map(trim).unwrap_or_default();
    result.answer = if answer.is_empty() {
        "Could not finish with the available skills.".into()
    } else {
        answer.to_string()
    };
    Ok(result)
}
