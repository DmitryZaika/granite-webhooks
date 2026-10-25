//! What a follow-up is written from (`followups::gather`), on the seed plus
//! `fixtures/followups/gather.sql`. Expected values are the CRM's
//! `gatherFollowupContext` on the same data (server in UTC).

use api::followups::dates::parse_iso;
use api::followups::gather::{FollowupRequest, gather_followup_context};
use api::followups::send_window::FollowupChannel;
use serde_json::{Value, json};
use sqlx::MySqlPool;

const FIXTURE: &str = include_str!("fixtures/followups/gather.sql");

async fn setup(pool: &MySqlPool) {
    sqlx::raw_sql(FIXTURE).execute(pool).await.unwrap();
}

fn request(company_id: i32, user_id: i32, customer_id: Option<i32>) -> FollowupRequest {
    let mut request = FollowupRequest::new(company_id, user_id, FollowupChannel::Sms);
    request.customer_id = customer_id;
    request
}

fn clock() -> api::followups::dates::Instant {
    parse_iso("2026-10-13T15:00:00Z").unwrap()
}

#[test_db::test(seed)]
async fn the_profile_lists_the_customer_their_deals_and_the_customer_page(pool: MySqlPool) {
    setup(&pool).await;
    let g = gather_followup_context(&pool, &request(100, 100, Some(1000)), clock())
        .await
        .unwrap();
    assert_eq!(
        g.profile,
        "Name: Alice Lead\nPhone: 317-555-0100\nPhone 2: 317-555-0101\nAddress: 1 Main St\nSource: leads\n\
         You (the salesperson sending this — write as I, never use this name): Rita Rep\nCity: Indianapolis\nState: IN\n\
         Project type: Kitchen\nProject size: 45\nBest time to contact them: Evenings\nSink: Undermount\nBacksplash: 4 inch\n\
         Lead details: Looking at quartz countertops for kitchen island\nCustomer message: Please call me after 5pm, we want white quartz\n\
         Notes: Met at home show\nBecame a lead: 2026-03-01 10:00\n\
         Deal: Target #8004 · Quoted · amount 9999.00 · Closed/Won\nDeal: Other #8000 · Quoted · amount 1000.00 · Closed/Won\n\
         Customer page URL for this company (id 100 — keep /customer/100/ exactly): https://granite-manager.com/customer/100/stones"
    );
    assert_eq!(g.company_instructions, "Tone\nBe friendly.");
    assert_eq!(g.admin_instruction, "send messages with 2 day gap");
    assert_eq!(
        serde_json::to_value(&g.style_profile).unwrap(),
        json!({ "smsLength": "SHORT", "emailLength": null, "questionCount": "ONE_MAX", "exclamation": null,
                "customerName": null, "greeting": "ADD", "link": null })
    );
    let templates: Vec<Value> = g
        .templates
        .iter()
        .map(|t| json!([t.name, t.channel, t.scope]))
        .collect();
    assert_eq!(
        templates,
        [
            json!(["My check-in", "sms", "personal"]),
            json!(["My quote follow-up", "email", "personal"]),
            json!(["Quoted sms", "sms", "company"]),
            json!(["Quoted drip", "email", "company"]),
        ]
    );
    assert_eq!(
        serde_json::to_value(&g.generation_decision).unwrap(),
        json!({ "decision": "DO_NOT_GENERATE", "reason": "TARGET_DEAL_CLOSED_WON" })
    );
}

#[test_db::test(seed)]
async fn the_timeline_merges_both_sms_providers_and_email_oldest_first(pool: MySqlPool) {
    setup(&pool).await;
    let g = gather_followup_context(&pool, &request(100, 100, Some(1000)), clock())
        .await
        .unwrap();
    // The fixture's messages (the seed's own texts are dated this month).
    let fixture: Vec<Value> = g
        .timeline
        .iter()
        .filter(|m| m.at.as_str() < "2026-09-30")
        .map(|m| json!([m.at, m.channel, m.direction, m.text]))
        .collect();
    assert_eq!(
        fixture,
        [
            json!([
                "2026-06-01 12:00",
                "sms",
                "inbound",
                "Old RingCentral text from the second phone"
            ]),
            json!([
                "2026-09-10 10:00",
                "email",
                "inbound",
                "Subject: Kitchen quote\nCould you quote the island?"
            ]),
            json!([
                "2026-09-11 10:00",
                "email",
                "outbound",
                "Subject: Re: Kitchen quote\nAttached is the quote."
            ]),
            json!([
                "2026-09-20 15:00",
                "sms",
                "inbound",
                "Hi, we are still thinking about the white quartz. Can you send the price?"
            ]),
            json!([
                "2026-09-20 16:00",
                "sms",
                "outbound",
                "Sure, I will send the quote today"
            ]),
            json!([
                "2026-09-22 15:30",
                "sms",
                "inbound",
                "Thanks! we will decide next month"
            ]),
        ]
    );
    assert_eq!(g.timeline[3].from_phone.as_deref(), Some("3175550100"));
    // Found by phone: the same customer.
    let mut by_phone = request(100, 100, None);
    by_phone.phone_digits = Some("3175550100".into());
    let found = gather_followup_context(&pool, &by_phone, clock())
        .await
        .unwrap();
    assert_eq!(found.customer_id, Some(1000));
    assert_eq!(found.timeline.len(), g.timeline.len());
}

#[test_db::test(seed)]
async fn deal_notes_and_activities_feed_the_profile_and_the_hold(pool: MySqlPool) {
    setup(&pool).await;
    let g = gather_followup_context(&pool, &request(100, 100, Some(8695)), clock())
        .await
        .unwrap();
    assert!(g.profile.contains(
        "Deal note (2026-04-01 13:00): Prefers white quartz\nDeal note (2026-04-01 14:00): Second note\n\
         Activity: Send quote · stage New · open · created 2026-04-01 12:00 · deadline 2099-01-01 00:00 — do not contact the customer before this date\n\
         Activity: Call back · stage New · open · created 2026-04-01 11:00 · deadline 281d overdue"
    ), "{}", g.profile);
    assert_eq!(
        serde_json::to_value(&g.crm_hold).unwrap(),
        json!({ "until": "2099-01-01T14:00:00Z", "source": "activity",
                "reason": "Open activity \"Send quote\" is due 2099-01-01 — wait until then before contacting" })
    );
    assert!(g.crm_note_hold.is_none());
    let mut preferred = request(100, 107, Some(8695));
    preferred.preferred_deal_id = Some(8600);
    let g = gather_followup_context(&pool, &preferred, clock())
        .await
        .unwrap();
    assert!(g.profile.contains(
        "Deal: Other #8607 · Measure · amount 30.00\nDeal: Target #8600 · New · amount 1500.25"
    ));
}

#[test_db::test(seed)]
async fn an_unknown_phone_is_a_first_contact_to_write(pool: MySqlPool) {
    setup(&pool).await;
    let mut unknown = request(100, 100, None);
    unknown.phone_digits = Some("3175559876".into());
    let g = gather_followup_context(&pool, &unknown, clock())
        .await
        .unwrap();
    assert_eq!(g.customer_id, None);
    assert!(
        g.profile
            .starts_with("Phone: 3175559876\nCustomer page URL")
    );
    assert_eq!(
        serde_json::to_value(&g.generation_decision).unwrap(),
        json!({ "decision": "GENERATE" })
    );
    assert_eq!(
        serde_json::to_value(g.situation).unwrap(),
        json!("first_contact")
    );
}

#[test_db::test(seed)]
async fn the_email_timeline_never_holds_another_companys_mail(pool: MySqlPool) {
    // Nina (8695, company 100) was given the address of company 101's
    // customer, and the follow-up was asked about that address too.
    sqlx::raw_sql(
        "INSERT INTO customers_emails (customer_id, email, created_at, created_by)
           VALUES (8695, 'buyer@other-customer.test', '2026-04-01 08:00:00', 100);
         INSERT INTO emails (id, subject, body, sent_at, message_id, thread_id, sender_user_id,
           sender_email, receiver_email, company_id, is_draft) VALUES
         (98400, 'Ours', 'Nina asks about quartz', '2026-04-04 10:00:00', '<m98400@x.test>',
          'dddddddd-7777-4000-8000-000000000001', NULL, 'nina@example.com', 'rep@acme.test', 100, 0),
         (98401, 'Unstamped', 'Unstamped secret', '2026-04-04 11:00:00', '<m98401@x.test>',
          'dddddddd-7777-4000-8000-000000000002', NULL, 'nina@example.com', 'nobody@x.test', NULL, 0),
         (98402, 'Theirs', 'Company 101 secret', '2026-04-04 12:00:00', '<m98402@x.test>',
          'dddddddd-7777-4000-8000-000000000003', NULL, 'someone@x.test', 'rep@other.test', 101, 0);
         INSERT INTO email_participants (email_id, type, email, customer_id, position)
           VALUES (98402, 'cc', 'nina@example.com', 8695, 0);",
    )
    .execute(&pool)
    .await
    .unwrap();
    let mut request = request(100, 100, Some(8695));
    request.customer_email = Some("pat@prospect.test".into());
    let g = gather_followup_context(&pool, &request, clock())
        .await
        .unwrap();
    let emails: Vec<&str> = g
        .timeline
        .iter()
        .filter(|m| m.channel == FollowupChannel::Email)
        .map(|m| m.text.as_str())
        .collect();
    assert!(
        emails.iter().any(|t| t.contains("Nina asks about quartz")),
        "{emails:?}"
    );
    for leaked in [
        "Unstamped secret",
        "Company 101 secret",
        "Can you quote my kitchen?",
        "Sure, attached.",
        "Thanks! See my sketch.",
        "Do you carry Black Pearl?",
        "Just checking in.",
    ] {
        assert!(
            !emails.iter().any(|t| t.contains(leaked)),
            "{leaked}: {emails:?}"
        );
    }
}

/// A customer id of another company (it comes from request bodies) is no
/// customer at all: nothing of theirs reaches the writer, neither their
/// profile, deals, notes nor messages.
#[test_db::test(seed)]
async fn another_companys_customer_id_gathers_nothing_of_theirs(pool: MySqlPool) {
    setup(&pool).await;
    sqlx::raw_sql(
        "INSERT INTO deals (id, customer_id, amount, title, list_id, position, user_id, created_at, updated_at)
           VALUES (9901, 2000, 777.00, 'Their secret deal', 100, 0, 106, NOW(), NOW());
         INSERT INTO emails (id, thread_id, subject, body, sender_email, receiver_email, sent_at, company_id, message_id)
           VALUES (9902, 'theirs', 'Their quote', 'their secret body', 'rep@other.test', 'buyer@other-customer.test', NOW(), 101, '<t1@x>');
         INSERT INTO email_participants (email_id, email, customer_id, type) VALUES (9902, 'buyer@other-customer.test', 2000, 'to');",
    )
    .execute(&pool)
    .await
    .unwrap();
    for channel in [FollowupChannel::Sms, FollowupChannel::Email] {
        let mut request = request(100, 100, Some(2000));
        request.channel = channel;
        let g = gather_followup_context(&pool, &request, clock())
            .await
            .unwrap();
        assert_eq!(g.customer_id, None);
        let everything = format!("{}\n{:?}", g.profile, g.timeline);
        for secret in [
            "Other Co",
            "463-555-0200",
            "Their secret deal",
            "777",
            "their secret body",
            "#9901",
        ] {
            assert!(
                !everything.contains(secret),
                "{secret} leaked: {everything}"
            );
        }
    }
    // The company's own customer still resolves.
    let own = gather_followup_context(&pool, &request(101, 106, Some(2000)), clock())
        .await
        .unwrap();
    assert_eq!(own.customer_id, Some(2000));
    assert!(own.profile.contains("Other Co Customer"), "{}", own.profile);
}
