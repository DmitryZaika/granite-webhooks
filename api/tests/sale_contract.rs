mod common;

use api::app;
use api::state::AppState;
use axum::Router;
use axum::extract::Path;
use axum::http::StatusCode;
use axum::routing::get;
use axum_test::TestServer;
use common::{ALLOWED_ORIGIN, SECRET, WithSession, sessions};
use lopdf::{Dictionary, Document, Object, Stream, dictionary};
use sqlx::MySqlPool;
use std::collections::HashMap;

/// A one-page form with the contract's text fields and its seam dropdown.
fn template(text_fields: impl Iterator<Item = u32>, dropdown: &str) -> Vec<u8> {
    let mut doc = Document::with_version("1.7");
    let pages_id = doc.new_object_id();
    let page_id = doc.new_object_id();
    let mut fields = Vec::new();
    let mut widget = |doc: &mut Document, name: String, kind: &str, index: u32| {
        let y = 800 - i64::from(index % 60) * 12;
        let mut field = dictionary! {
            "Type" => "Annot",
            "Subtype" => "Widget",
            "FT" => Object::Name(kind.as_bytes().to_vec()),
            "T" => Object::string_literal(name),
            "Rect" => vec![20.into(), y.into(), 220.into(), (y + 14).into()],
            "P" => page_id,
            "DA" => Object::string_literal("/Helv 12 Tf 0 g"),
        };
        if kind == "Ch" {
            field.set("Ff", 131_072);
            field.set(
                "Opt",
                ["STD", "SPH", "EXT", "EU", "NONE", "N/A"]
                    .iter()
                    .map(|option| Object::string_literal(*option))
                    .collect::<Vec<_>>(),
            );
        }
        let id = doc.add_object(field);
        fields.push(Object::Reference(id));
    };
    for number in text_fields {
        widget(&mut doc, format!("Text{number}"), "Tx", number);
    }
    widget(&mut doc, dropdown.to_string(), "Ch", 0);
    let content = doc.add_object(Stream::new(Dictionary::new(), Vec::new()));
    doc.objects.insert(
        page_id,
        Object::Dictionary(dictionary! {
            "Type" => "Page",
            "Parent" => pages_id,
            "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
            "Contents" => content,
            "Annots" => fields.clone(),
        }),
    );
    doc.objects.insert(
        pages_id,
        Object::Dictionary(dictionary! {
            "Type" => "Pages",
            "Kids" => vec![page_id.into()],
            "Count" => 1,
        }),
    );
    let form = doc.add_object(dictionary! { "Fields" => fields });
    let catalog = doc.add_object(dictionary! {
        "Type" => "Catalog",
        "Pages" => pages_id,
        "AcroForm" => form,
    });
    doc.trailer.set("Root", catalog);
    let mut bytes = Vec::new();
    doc.save_to(&mut bytes).unwrap();
    bytes
}

async fn server(pool: MySqlPool) -> TestServer {
    let homeowner = template(1..=38, "Dropdown42");
    let commercial = template(123..=161, "Dropdown159");
    let router = Router::new().route(
        "/base-contracts/{name}",
        get(move |Path(name): Path<String>| {
            let homeowner = homeowner.clone();
            let commercial = commercial.clone();
            async move {
                match name.as_str() {
                    "Contract.pdf" => Ok(homeowner),
                    "Contract_commercial.pdf" => Ok(commercial),
                    _ => Err(StatusCode::NOT_FOUND),
                }
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let state = AppState::new(pool, SECRET, ALLOWED_ORIGIN)
        .with_contract_templates_base(&format!("http://{address}"));
    TestServer::new(app(state))
}

/// Field name → value, and whether every set field has an appearance.
fn values(pdf: &[u8]) -> HashMap<String, String> {
    let doc = Document::load_mem(pdf).unwrap();
    let mut found = HashMap::new();
    for object in doc.objects.values() {
        let Ok(field) = object.as_dict() else {
            continue;
        };
        let Ok(name) = field.get(b"T").and_then(Object::as_str) else {
            continue;
        };
        let name = String::from_utf8_lossy(name).into_owned();
        if let Ok(value) = field.get(b"V").and_then(Object::as_str) {
            assert!(field.has(b"AP"), "{name} has no appearance");
            found.insert(name, String::from_utf8_lossy(value).into_owned());
        }
    }
    found
}

async fn contract(pool: MySqlPool) -> (axum_test::TestResponse, HashMap<String, String>) {
    let res = server(pool)
        .await
        .get("/v1/sales/7010/contract")
        .with_session(sessions::REP)
        .await;
    res.assert_status_ok();
    let values = values(res.as_bytes());
    (res, values)
}

#[test_db::test(seed)]
async fn homeowner_contract_is_filled_in(pool: MySqlPool) {
    sqlx::raw_sql(
        "UPDATE slab_inventory SET room = 'KITCHEN', backsplash = '4 inch', tear_out = 'stone_t/o',
                stove = 's/i', waterfall = 'yes', corbels = 2, seam = 'Phantom', price = 1890.00,
                extras = '{\"edge_price\": {\"edge_type\": \"OGEE\"}, \"ten_year_sealer\": true}',
                room_uuid = UUID_TO_BIN('11111111-2222-3333-4444-555555555555')
          WHERE id = 6071;
         INSERT INTO faucets (id, faucet_type_id, is_deleted, slab_id)
              SELECT 6190, id, 0, 6071 FROM faucet_type WHERE company_id = 100 LIMIT 1;",
    )
    .execute(&pool)
    .await
    .unwrap();
    let faucet: String = sqlx::query_scalar(
        "SELECT faucet_type.name FROM faucets JOIN faucet_type ON faucet_type.id = faucets.faucet_type_id WHERE faucets.id = 6190",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    let (res, values) = contract(pool).await;
    assert_eq!(res.header("content-type"), "application/pdf");
    assert_eq!(
        res.header("content-disposition"),
        "inline; filename=\"Alice Lead.pdf\""
    );
    let expected = [
        ("Text1", "4/15/2026"),
        ("Text2", "Rita Rep"),
        ("Text3", "Alice Lead"),
        ("Text4", "1 Main St"),
        ("Text7", "Kitchen"),
        ("Text8", "Taj Mahal"),
        ("Text9", "Composite Black X 2, Bar Prep Sink"),
        ("Text10", faucet.as_str()),
        ("Text11", "Ogee"),
        ("Text12", "4 inch"),
        ("Text25", "30.25"),
        ("Text26", "1890"),
        ("Text31", "No"),
        ("Text32", "Yes"),
        ("Text33", "S/I"),
        ("Text34", "Yes"),
        ("Text35", "Yes"),
        ("Text36", "2"),
        ("Dropdown42", "SPH"),
        ("Text37", "4200"),
        ("Text38", "2100"),
    ];
    for (field, value) in expected {
        assert_eq!(
            values.get(field).map(String::as_str),
            Some(value),
            "{field}"
        );
    }
    // Only one room: the second room's fields stay empty.
    assert!(!values.contains_key("Text13"));
}

#[test_db::test(seed)]
async fn commercial_customers_get_the_commercial_contract(pool: MySqlPool) {
    sqlx::raw_sql(
        "UPDATE customers SET company_name = 'Lead Builders LLC', address = '9 Billing Rd' WHERE id = 1000;
         UPDATE sales SET price = 900 WHERE id = 7010;",
    )
    .execute(&pool)
    .await
    .unwrap();
    let (_, values) = contract(pool).await;
    for (field, value) in [
        ("Text125", "Alice Lead"),
        ("Text126", "Lead Builders LLC"),
        ("Text128", "9 Billing Rd"),
        ("Text132", "Taj Mahal"),
        ("Text134", "N/A"),
        ("Text135", "N/A"),
        ("Text146", "30.25"),
        ("Text154", "N/A"),
        ("Dropdown159", "N/A"),
        // At most $1000: the whole price is due up front.
        ("Text160", "900"),
        ("Text161", "900"),
    ] {
        assert_eq!(
            values.get(field).map(String::as_str),
            Some(value),
            "{field}"
        );
    }
    assert!(!values.contains_key("Text1"));
}

#[test_db::test(seed)]
async fn other_companies_sales_and_slabless_sales_are_not_found(pool: MySqlPool) {
    let server = server(pool).await;
    server
        .get("/v1/sales/7010/contract")
        .with_session(sessions::OTHER_COMPANY_REP)
        .await
        .assert_status(StatusCode::NOT_FOUND);
    server
        .get("/v1/sales/999999/contract")
        .with_session(sessions::REP)
        .await
        .assert_status(StatusCode::NOT_FOUND);
    server
        .get("/v1/sales/7010/contract")
        .await
        .assert_status(StatusCode::UNAUTHORIZED);
}

#[test_db::test(seed)]
async fn more_than_three_rooms_is_refused(pool: MySqlPool) {
    sqlx::raw_sql(
        "INSERT INTO slab_inventory (id, stone_id, bundle, sale_id, room_uuid) VALUES
           (6180, 3010, 'R1', 7010, UUID_TO_BIN(UUID())),
           (6181, 3010, 'R2', 7010, UUID_TO_BIN(UUID())),
           (6182, 3010, 'R3', 7010, UUID_TO_BIN(UUID()));",
    )
    .execute(&pool)
    .await
    .unwrap();
    let res = server(pool)
        .await
        .get("/v1/sales/7010/contract")
        .with_session(sessions::REP)
        .await;
    res.assert_status(StatusCode::BAD_REQUEST);
}
