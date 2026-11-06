//! "Already used" detection for guided tours.
//!
//! A tour can name a usage check (`usage_check` in the manifest): a question
//! like "did this user ever email a quote?". For a user who answers yes the
//! app never starts the tour by itself; it stays a 0-point replay in the
//! Onboarding widget, and a tour row created after an old use pays no points
//! (see `store`).
//!
//! Three kinds of check:
//! - **data** checks read the app's own tables (one grouped query per check,
//!   for one user or for the whole company);
//! - **event** checks cover work the server cannot see in its tables (a
//!   panel opened in the browser); the app reports them with
//!   `POST /v1/me/onboarding/usage`;
//! - **both**: read from the tables and also reported by the app, for a
//!   setting whose tables keep who made a row but not who changed one, or
//!   keep no one at all (a company switch).
//!
//! An admin setting the company already has counts for every user of the
//! company: nobody can tell which admin set it up before its tour shipped.
//! Each admin's own later changes still count for that admin.
//!
//! Positives are cached per user in `user_feature_usage` (a negative is
//! asked again, so the first real use is noticed). A user's
//! `user_onboarding.feature_tasks_reset_at` is a fence: a use at or before it
//! does not count, neither as data nor as a cached positive.
//!
//! All the times compared here are UTC (the tables hold UTC).

use super::store::Actor;
use chrono::{NaiveDateTime, Utc};
use sqlx::{MySql, MySqlPool, QueryBuilder};
use std::collections::{BTreeMap, HashMap};

/// Where a check gets its answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Read from the app's tables.
    Data,
    /// Reported by the app (`POST /v1/me/onboarding/usage`).
    Event,
    /// Read from the app's tables (what they record) and reported by the app
    /// (what they do not). A report is a cached positive like any other, so
    /// the tables are read only while there is none.
    Both,
}

/// One usage check.
#[derive(Debug)]
pub struct UsageCheck {
    pub id: &'static str,
    pub kind: Kind,
    /// Data and both checks: a `SELECT user_id, ts` of every use in the
    /// company, one row per use. `{c}` is the company id; `[[col]]` becomes
    /// `AND col = <user>` when asking for one user and nothing for the whole
    /// company, so the user's rows are found by index. The ids are numbers,
    /// so they go in as literals.
    rows: &'static str,
}

impl UsageCheck {
    /// The check has rows to read (a data or both check).
    fn reads_data(&self) -> bool {
        self.kind != Kind::Event
    }
}

/// Every check, by id.
static CHECKS: [UsageCheck; 20] = [
    // A quote emailed to a customer. Quotes are scoped by their company.
    UsageCheck {
        id: "quote_emailed",
        kind: Kind::Data,
        rows: "SELECT qe.sent_by AS user_id, qe.sent_at AS ts
                 FROM quote_emails qe
                 JOIN quotes q ON q.id = qe.quote_id AND q.company_id = {c}
                WHERE qe.sent_by IS NOT NULL [[qe.sent_by]]",
    },
    // A schedule event the user created. `events` has no company column:
    // the creator's home company scopes it. Unscheduled jobs are left out:
    // sold quotes add those on their own.
    UsageCheck {
        id: "schedule_event_created",
        kind: Kind::Data,
        rows: "SELECT e.created_user_id AS user_id, e.created_date AS ts
                 FROM events e
                 JOIN users u ON u.id = e.created_user_id AND u.company_id = {c}
                WHERE e.unscheduled = 0 [[e.created_user_id]]",
    },
    // An email or a text scheduled to go out later.
    UsageCheck {
        id: "send_later_used",
        kind: Kind::Data,
        rows: "SELECT de.user_id AS user_id, de.created_at AS ts
                 FROM delayed_emails de WHERE de.company_id = {c} [[de.user_id]]
               UNION ALL
               SELECT ds.user_id AS user_id, ds.created_at AS ts
                 FROM delayed_sms ds WHERE ds.company_id = {c} [[ds.user_id]]",
    },
    // A template used (`user_template_usage` keeps only the last use of each
    // template, so this is the earliest of those) or one the user made.
    // `user_template_usage` has no company column: the user's home company
    // scopes it. Deleted templates do not count.
    UsageCheck {
        id: "templates_used",
        kind: Kind::Data,
        rows: "SELECT tu.user_id AS user_id, tu.last_used_at AS ts
                 FROM user_template_usage tu
                 JOIN users u ON u.id = tu.user_id AND u.company_id = {c}
                WHERE 1 = 1 [[tu.user_id]]
               UNION ALL
               SELECT et.user_id AS user_id, et.created_at AS ts
                 FROM email_templates et
                WHERE et.company_id = {c} AND et.user_id IS NOT NULL
                  AND et.deleted_at IS NULL [[et.user_id]]
               UNION ALL
               SELECT st.user_id AS user_id, st.created_at AS ts
                 FROM cloudtalk_sms_templates st
                WHERE st.company_id = {c} AND st.user_id IS NOT NULL
                  AND st.deleted_at IS NULL [[st.user_id]]",
    },
    // A document sent out for signature (a draft never went out).
    UsageCheck {
        id: "signature_sent",
        kind: Kind::Data,
        rows: "SELECT sr.created_by_user_id AS user_id, sr.created_at AS ts
                 FROM signature_requests sr
                WHERE sr.company_id = {c} AND sr.status <> 'draft'
                  [[sr.created_by_user_id]]",
    },
    // A question asked in Chat: every question from 2026-10-28 is in
    // `chat_log`, and `chat_history` (the user's saved conversation, only
    // Chat writes it) dates from their first answer. Neither is sure to have
    // a company: the asker's home company scopes them. The AI cost log holds
    // older questions as `chat`, but from 2026-10-07 the Schedule estimate
    // (run as the dialog opens) and price offers logged as `chat` too, with
    // the same model and nothing else to tell them apart: only its rows from
    // before they shipped count (a day early, whatever the session's zone).
    UsageCheck {
        id: "chat_asked",
        kind: Kind::Data,
        rows: "SELECT cl.user_id AS user_id, cl.created_at AS ts
                 FROM chat_log cl
                 JOIN users u ON u.id = cl.user_id AND u.company_id = {c}
                WHERE 1 = 1 [[cl.user_id]]
               UNION ALL
               SELECT ch.user_id AS user_id, ch.created_date AS ts
                 FROM chat_history ch
                 JOIN users u ON u.id = ch.user_id AND u.company_id = {c}
                WHERE ch.created_date IS NOT NULL [[ch.user_id]]
               UNION ALL
               SELECT al.user_id AS user_id, al.created_at AS ts
                 FROM ai_usage_log al
                WHERE al.company_id = {c} AND al.feature = 'chat' AND al.success = 1
                  AND al.created_at < '2026-10-06'
                  AND al.user_id IS NOT NULL [[al.user_id]]",
    },
    // A draft from the AI reply button. Until 2026-11-05 it was logged as
    // `quick_reply` with the composer suggestions; only AI reply asked the
    // large model.
    UsageCheck {
        id: "ai_reply_used",
        kind: Kind::Data,
        rows: "SELECT al.user_id AS user_id, al.created_at AS ts
                 FROM ai_usage_log al
                WHERE al.company_id = {c} AND al.success = 1 AND al.user_id IS NOT NULL
                  AND (al.feature = 'ai_reply'
                       OR (al.feature = 'quick_reply' AND al.model LIKE 'gpt-6.1-sol%'))
                  [[al.user_id]]",
    },
    // A call the user placed: CloudTalk and RingCentral calls from the points
    // sync, Granite Phone calls from its own table.
    UsageCheck {
        id: "customer_called",
        kind: Kind::Data,
        rows: "SELECT pp.user_id AS user_id, pp.started_at AS ts
                 FROM phone_call_points pp
                WHERE pp.company_id = {c} AND pp.user_id IS NOT NULL
                  AND pp.started_at IS NOT NULL [[pp.user_id]]
               UNION ALL
               SELECT pc.user_id AS user_id, pc.started_at AS ts
                 FROM phone_calls pc
                WHERE pc.company_id = {c} AND pc.user_id IS NOT NULL
                  AND pc.direction = 'outbound' [[pc.user_id]]",
    },
    // A call turned into a note or activities by AI: only the call's Create
    // note and Create activities buttons log these. A transcript alone does
    // not count: the call's show-text action and the Call coach log
    // `call_transcription` too.
    UsageCheck {
        id: "call_followed_up",
        kind: Kind::Data,
        rows: "SELECT al.user_id AS user_id, al.created_at AS ts
                 FROM ai_usage_log al
                WHERE al.company_id = {c} AND al.success = 1 AND al.user_id IS NOT NULL
                  AND al.feature IN ('call_note_summary', 'call_activity_extract')
                  [[al.user_id]]",
    },
    // A quote drawing the user saved: each save of the drawing is a revision.
    UsageCheck {
        id: "quote_drawn",
        kind: Kind::Data,
        rows: "SELECT qr.created_by AS user_id, qr.created_at AS ts
                 FROM quote_revisions qr
                 JOIN quotes q ON q.id = qr.quote_id AND q.company_id = {c}
                WHERE qr.created_by IS NOT NULL [[qr.created_by]]",
    },
    // A saved quote drawing with a sink, cooktop or other cutout on a counter.
    UsageCheck {
        id: "quote_cutout_added",
        kind: Kind::Data,
        rows: "SELECT qr.created_by AS user_id, qr.created_at AS ts
                 FROM quote_revisions qr
                 JOIN quotes q ON q.id = qr.quote_id AND q.company_id = {c}
                WHERE qr.created_by IS NOT NULL
                  AND JSON_EXTRACT(qr.drawing_json, '$.counters[*].runs[*].cutouts[0]') IS NOT NULL
                  [[qr.created_by]]",
    },
    // A company automation the admin set up: an email template tied to a list
    // (it sends by itself after its delay), or a company follow-up flow.
    // Templates from before 2026-08-10 keep no creator, so a company that has
    // a live automation counts for each of its users, from its earliest one.
    UsageCheck {
        id: "automation_set_up",
        kind: Kind::Data,
        rows: "SELECT et.created_by AS user_id, et.created_at AS ts
                 FROM email_templates et
                WHERE et.company_id = {c} AND et.user_id IS NULL AND et.created_by IS NOT NULL
                  AND (et.lead_group_id IS NOT NULL OR et.lead_list_id IS NOT NULL)
                  [[et.created_by]]
               UNION ALL
               SELECT sf.created_by AS user_id, sf.created_at AS ts
                 FROM sms_flows sf
                WHERE sf.company_id = {c} AND sf.user_id IS NULL AND sf.created_by IS NOT NULL
                  [[sf.created_by]]
               UNION ALL
               SELECT u.id AS user_id, a.ts
                 FROM users u
                 JOIN (SELECT MIN(x.ts) AS ts
                         FROM (SELECT et.created_at AS ts
                                 FROM email_templates et
                                WHERE et.company_id = {c} AND et.user_id IS NULL
                                  AND et.deleted_at IS NULL
                                  AND (et.lead_group_id IS NOT NULL OR et.lead_list_id IS NOT NULL)
                               UNION ALL
                               SELECT sf.created_at AS ts
                                 FROM sms_flows sf
                                WHERE sf.company_id = {c} AND sf.user_id IS NULL
                                  AND sf.deleted_at IS NULL) x) a ON a.ts IS NOT NULL
                WHERE u.company_id = {c} AND u.is_deleted = 0 [[u.id]]",
    },
    UsageCheck {
        id: "quote_views_opened",
        kind: Kind::Event,
        rows: "",
    },
    UsageCheck {
        id: "unscheduled_planned",
        kind: Kind::Event,
        rows: "",
    },
    // A Chat question sent with "Share this page" on.
    UsageCheck {
        id: "chat_page_shared",
        kind: Kind::Event,
        rows: "",
    },
    // An AI reply draft changed with an instruction (Update draft). The AI
    // cost log cannot tell it from a first draft.
    UsageCheck {
        id: "ai_reply_revised",
        kind: Kind::Event,
        rows: "",
    },
    // A quote automation switch changed and saved (Admin, Quotes, Automation).
    // The switches are company columns: nothing keeps who set them or when.
    // The app reports each change; a company with a switch on counts for
    // each of its users as set up at the tours' release (2026-11-06, UTC). A
    // tour reset after that drops it, as for any use from before the reset.
    UsageCheck {
        id: "quote_automation_changed",
        kind: Kind::Both,
        rows: "SELECT u.id AS user_id, CAST('2026-11-06 00:00:00' AS DATETIME) AS ts
                 FROM users u
                 JOIN company co ON co.id = u.company_id
                WHERE u.company_id = {c} AND u.is_deleted = 0
                  AND (co.auto_sale = 1 OR co.auto_unscheduled_jobs = 1 OR co.auto_invoice = 1)
                  [[u.id]]",
    },
    // Schedule's customer texts saved (Admin, Schedule): a template or a flow
    // the user made (deleted ones too), and what the app reports, as an edit
    // of someone else's template or the AI texts settings (company columns).
    UsageCheck {
        id: "schedule_texts_saved",
        kind: Kind::Both,
        rows: "SELECT t.created_by AS user_id, t.created_at AS ts
                 FROM schedule_notification_templates t
                WHERE t.company_id = {c} AND t.created_by IS NOT NULL [[t.created_by]]
               UNION ALL
               SELECT f.created_by AS user_id, f.created_at AS ts
                 FROM schedule_notification_flows f
                WHERE f.company_id = {c} AND f.created_by IS NOT NULL [[f.created_by]]",
    },
    // An exact length typed and committed in the quote drawing's dimension
    // dialog.
    UsageCheck {
        id: "quote_size_typed",
        kind: Kind::Event,
        rows: "",
    },
    // An edit applied in the stone photo editor (Use edited image).
    UsageCheck {
        id: "stone_image_edited",
        kind: Kind::Event,
        rows: "",
    },
];

/// The check with this id.
pub fn check(id: &str) -> Option<&'static UsageCheck> {
    CHECKS.iter().find(|check| check.id == id)
}

/// `id` is a check the app may report itself (an event or a both check).
pub fn is_event_check(id: &str) -> bool {
    check(id).is_some_and(|check| check.kind != Kind::Data)
}

/// ISO 8601 UTC, as the rest of the onboarding payloads.
fn iso(time: NaiveDateTime) -> String {
    time.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string()
}

/// `rows` of a check for the company, and for one user when given.
fn expand(rows: &str, company: i32, user: Option<i32>) -> String {
    let mut sql = rows.replace("{c}", &company.to_string());
    while let Some(start) = sql.find("[[") {
        let Some(len) = sql[start..].find("]]") else {
            break;
        };
        let column = sql[start + 2..start + len].to_string();
        let filter = user.map_or_else(String::new, |user| format!("AND {column} = {user}"));
        sql.replace_range(start..start + len + 2, &filter);
    }
    sql
}

/// The first use after the user's fence of a data check, per user: for one
/// user, or for everyone in the company.
async fn first_uses(
    pool: &MySqlPool,
    check: &UsageCheck,
    company: i32,
    user: Option<i32>,
) -> sqlx::Result<Vec<(i32, NaiveDateTime)>> {
    let sql = format!(
        "SELECT r.user_id, CAST(MIN(r.ts) AS DATETIME(3))
           FROM ({rows}) r
           LEFT JOIN user_onboarding uo ON uo.user_id = r.user_id AND uo.company_id = {company}
          WHERE r.ts > COALESCE(uo.feature_tasks_reset_at, '1000-01-01')
          GROUP BY r.user_id",
        rows = expand(check.rows, company, user),
    );
    sqlx::query_as(sqlx::AssertSqlSafe(sql))
        .fetch_all(pool)
        .await
}

/// The cached positives of `users` past their fences: user -> check -> time.
async fn cached(
    pool: &MySqlPool,
    company: i32,
    users: &[i32],
) -> sqlx::Result<HashMap<i32, BTreeMap<String, NaiveDateTime>>> {
    let mut found: HashMap<i32, BTreeMap<String, NaiveDateTime>> = HashMap::new();
    if users.is_empty() {
        return Ok(found);
    }
    let mut query: QueryBuilder<MySql> = QueryBuilder::new(
        "SELECT f.user_id, f.check_id, f.first_used_at
           FROM user_feature_usage f
           LEFT JOIN user_onboarding uo ON uo.user_id = f.user_id AND uo.company_id = f.company_id
          WHERE f.company_id = ",
    );
    query.push_bind(company);
    query.push(" AND f.user_id IN (");
    let mut list = query.separated(", ");
    for user in users {
        list.push_bind(*user);
    }
    query.push(") AND f.first_used_at > COALESCE(uo.feature_tasks_reset_at, '1000-01-01')");
    let rows: Vec<(i32, String, NaiveDateTime)> = query.build_query_as().fetch_all(pool).await?;
    for (user, check_id, first_used) in rows {
        // A retired check's rows stay in the table but are not served.
        if check(&check_id).is_some() {
            found.entry(user).or_default().insert(check_id, first_used);
        }
    }
    Ok(found)
}

/// Saves a positive. An existing row stays as it is unless its use is at or
/// before the user's fence (a row from before a reset): then it is replaced.
/// ON DUPLICATE KEY UPDATE assigns left to right, so `first_used_at`, which
/// the staleness condition reads, is assigned LAST.
async fn store(
    pool: &MySqlPool,
    actor: Actor,
    check_id: &str,
    first_used: NaiveDateTime,
    source: &str,
    detected: NaiveDateTime,
) -> sqlx::Result<()> {
    let stale = "user_feature_usage.first_used_at <= COALESCE(
                   (SELECT uo.feature_tasks_reset_at FROM user_onboarding uo
                     WHERE uo.user_id = new.user_id AND uo.company_id = new.company_id),
                   '1000-01-01')";
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "INSERT INTO user_feature_usage
           (user_id, company_id, check_id, first_used_at, source, detected_at)
         VALUES (?, ?, ?, ?, ?, ?) AS new
         ON DUPLICATE KEY UPDATE
           source = IF({stale}, new.source, user_feature_usage.source),
           detected_at = IF({stale}, new.detected_at, user_feature_usage.detected_at),
           first_used_at = IF({stale}, new.first_used_at, user_feature_usage.first_used_at)"
    )))
    .bind(actor.user_id)
    .bind(actor.company_id)
    .bind(check_id)
    .bind(first_used)
    .bind(source)
    .bind(detected)
    .execute(pool)
    .await?;
    Ok(())
}

/// What the actor has used: check id -> first use (ISO 8601 UTC).
///
/// Holds every cached positive (any kind) and each data or both check in
/// `wanted` that is not cached and is positive now (stored for next time).
/// Negatives are never cached.
pub async fn evaluate(
    pool: &MySqlPool,
    actor: Actor,
    wanted: &[&str],
) -> sqlx::Result<BTreeMap<String, String>> {
    let mut known = cached(pool, actor.company_id, &[actor.user_id])
        .await?
        .remove(&actor.user_id)
        .unwrap_or_default();
    let now = Utc::now().naive_utc();
    for id in wanted {
        let Some(check) = check(id).filter(|check| check.reads_data()) else {
            continue;
        };
        if known.contains_key(check.id) {
            continue;
        }
        let uses = first_uses(pool, check, actor.company_id, Some(actor.user_id)).await?;
        if let Some((_, first_used)) = uses.into_iter().find(|(user, _)| *user == actor.user_id) {
            store(pool, actor, check.id, first_used, "data", now).await?;
            known.insert(check.id.to_string(), first_used);
        }
    }
    Ok(known.into_iter().map(|(id, at)| (id, iso(at))).collect())
}

/// [`evaluate`] for many users of one company: each wanted data or both
/// check that some user lacks runs once for the whole company.
pub async fn evaluate_team(
    pool: &MySqlPool,
    company: i32,
    users: &[i32],
    wanted: &[&str],
) -> sqlx::Result<HashMap<i32, BTreeMap<String, String>>> {
    let mut known = cached(pool, company, users).await?;
    let now = Utc::now().naive_utc();
    for id in wanted {
        let Some(check) = check(id).filter(|check| check.reads_data()) else {
            continue;
        };
        let missing = users.iter().any(|user| {
            !known
                .get(user)
                .is_some_and(|got| got.contains_key(check.id))
        });
        if !missing {
            continue;
        }
        for (user, first_used) in first_uses(pool, check, company, None).await? {
            if !users.contains(&user)
                || known
                    .get(&user)
                    .is_some_and(|got| got.contains_key(check.id))
            {
                continue;
            }
            let actor = Actor {
                user_id: user,
                company_id: company,
            };
            store(pool, actor, check.id, first_used, "data", now).await?;
            known
                .entry(user)
                .or_default()
                .insert(check.id.to_string(), first_used);
        }
    }
    Ok(users
        .iter()
        .map(|user| {
            let found = known
                .remove(user)
                .unwrap_or_default()
                .into_iter()
                .map(|(id, at)| (id, iso(at)))
                .collect();
            (*user, found)
        })
        .collect())
}

/// The app reports a use of an event check: now is its first use. A repeat
/// changes nothing.
pub async fn record_event(pool: &MySqlPool, actor: Actor, check_id: &str) -> sqlx::Result<()> {
    let now = Utc::now().naive_utc();
    store(pool, actor, check_id, now, "event", now).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn data_and_event_checks_are_split() {
        for id in [
            "quote_emailed",
            "schedule_event_created",
            "send_later_used",
            "templates_used",
            "signature_sent",
            "chat_asked",
            "ai_reply_used",
            "customer_called",
            "call_followed_up",
            "quote_drawn",
            "quote_cutout_added",
            "automation_set_up",
        ] {
            let check = check(id).unwrap();
            assert_eq!(check.kind, Kind::Data, "{id}");
            assert!(!is_event_check(id), "{id}");
            assert!(check.rows.contains("{c}"), "{id}");
        }
        for id in [
            "quote_views_opened",
            "unscheduled_planned",
            "chat_page_shared",
            "ai_reply_revised",
            "quote_size_typed",
            "stone_image_edited",
        ] {
            assert_eq!(check(id).unwrap().kind, Kind::Event, "{id}");
            assert!(is_event_check(id), "{id}");
            assert!(!check(id).unwrap().reads_data(), "{id}");
        }
        // Read from the tables, and the app may report it too.
        for id in ["schedule_texts_saved", "quote_automation_changed"] {
            let both = check(id).unwrap();
            assert_eq!(both.kind, Kind::Both, "{id}");
            assert!(is_event_check(both.id) && both.reads_data(), "{id}");
            assert!(both.rows.contains("{c}"), "{id}");
        }
        assert!(check("nope").is_none());
        assert!(!is_event_check("nope"));
    }

    #[test]
    fn ids_are_unique_and_fit_the_column() {
        let mut seen = std::collections::HashSet::new();
        for check in &CHECKS {
            assert!(seen.insert(check.id), "{}", check.id);
            assert!(check.id.len() <= 64);
        }
    }

    #[test]
    fn other_ai_calls_do_not_count_as_their_feature() {
        // The Schedule estimate and price offers logged as `chat` until they
        // got their own names: the cost log counts only before they shipped.
        let chat = check("chat_asked").unwrap().rows;
        assert!(chat.contains("al.feature = 'chat'"));
        assert!(chat.contains("al.created_at < '2026-10-06'"));
        assert!(!chat.contains("schedule_estimate") && !chat.contains("price_offers"));
        let call = check("call_followed_up").unwrap().rows;
        assert!(call.contains("'call_note_summary'") && call.contains("'call_activity_extract'"));
        assert!(!call.contains("call_transcription"));
    }

    #[test]
    fn a_user_filter_lands_on_every_branch() {
        let all = expand(check("send_later_used").unwrap().rows, 7, None);
        assert!(all.contains("company_id = 7"));
        assert!(!all.contains("[[") && !all.contains("user_id = 3"));
        let one = expand(check("send_later_used").unwrap().rows, 7, Some(3));
        assert!(one.contains("AND de.user_id = 3") && one.contains("AND ds.user_id = 3"));
        assert!(!one.contains("[[") && !one.contains("{c}"));
        let chat = expand(check("chat_asked").unwrap().rows, 7, Some(3));
        for column in ["cl.user_id", "ch.user_id", "al.user_id"] {
            assert!(chat.contains(&format!("AND {column} = 3")), "{column}");
        }
    }

    #[test]
    fn a_company_setup_counts_for_each_user_of_the_company() {
        // Each user's own rows, and the company's for every user: one user is
        // asked for by id, the whole company by its users.
        let automation = check("automation_set_up").unwrap().rows;
        let one = expand(automation, 7, Some(3));
        for column in ["et.created_by", "sf.created_by", "u.id"] {
            assert!(one.contains(&format!("AND {column} = 3")), "{column}");
        }
        assert!(automation.contains("MIN(x.ts)") && automation.contains("u.is_deleted = 0"));
        let all = expand(automation, 7, None);
        assert!(all.contains("u.company_id = 7") && !all.contains("= 3"));
        // The switches keep no time: set up at the tours' release.
        let quote = check("quote_automation_changed").unwrap().rows;
        for column in [
            "co.auto_sale",
            "co.auto_unscheduled_jobs",
            "co.auto_invoice",
        ] {
            assert!(quote.contains(column), "{column}");
        }
        assert!(quote.contains("'2026-11-06 00:00:00'"));
        assert!(expand(quote, 7, Some(3)).contains("AND u.id = 3"));
    }
}
