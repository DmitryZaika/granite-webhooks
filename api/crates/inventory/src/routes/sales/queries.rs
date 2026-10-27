//! Port of `app/orm/contract.ts` and `app/utils/contractsBackend.server.ts`.
//!
//! Every write runs in one transaction: a failure (e.g. no free sink of a
//! model) leaves nothing half saved, where Remix kept the rows it had already
//! written.

use super::schemas::{RoomInput, SaleContract, SaleFixture, SaleInput, SaleRoom, SaleSlab};
use crate::error::ApiError;
use crate::sql::push_in;
use serde_json::Value;
use sqlx::{AssertSqlSafe, FromRow, MySql, MySqlConnection, MySqlPool, QueryBuilder};
use std::collections::HashSet;

/// Sales are scoped through their customer: old sales have no `company_id`.
const SALE_IN_COMPANY: &str = "SELECT sales.id FROM sales
     JOIN customers ON customers.id = sales.customer_id
     WHERE sales.id = ? AND customers.company_id = ?";

pub async fn sale_exists(
    conn: &mut MySqlConnection,
    company_id: i32,
    sale_id: i32,
) -> Result<bool, sqlx::Error> {
    let found: Option<i32> = sqlx::query_scalar(SALE_IN_COMPANY)
        .bind(sale_id)
        .bind(company_id)
        .fetch_optional(conn)
        .await?;
    Ok(found.is_some())
}

#[derive(FromRow)]
struct SaleRow {
    id: i32,
    customer_id: i32,
    deal_id: Option<i64>,
    seller_id: Option<i32>,
    project_address: Option<String>,
    notes: Option<String>,
    price: Option<String>,
    company_name: Option<String>,
    extras: Option<String>,
}

#[derive(FromRow)]
struct SlabRow {
    id: i64,
    room_id: Option<String>,
    room: Option<String>,
    backsplash: Option<String>,
    square_feet: Option<String>,
    retail_price: Option<String>,
    tear_out: Option<String>,
    stove: Option<String>,
    waterfall: Option<String>,
    corbels: Option<i32>,
    seam: Option<String>,
    ten_year_sealer: bool,
    extras: Option<String>,
    has_children: bool,
}

#[derive(FromRow)]
struct FixtureRow {
    slab_id: i64,
    id: i32,
    type_id: i32,
    price: Option<String>,
}

fn parse_json(raw: Option<String>) -> Option<Value> {
    raw.and_then(|text| serde_json::from_str(&text).ok())
}

/// Remix: `getCustomerSchemaFromSaleId`. `None` when the sale is not the
/// company's or has no slabs left.
pub async fn find_contract(
    pool: &MySqlPool,
    company_id: i32,
    sale_id: i32,
) -> Result<Option<SaleContract>, sqlx::Error> {
    let sale = sqlx::query_as::<_, SaleRow>(
        "SELECT sales.id, sales.customer_id, CAST(sales.deal_id AS SIGNED) AS deal_id,
            sales.seller_id, sales.project_address,
            sales.notes, CAST(sales.price AS CHAR) AS price, customers.company_name,
            CAST(sales.extras AS CHAR) AS extras
         FROM sales
         JOIN customers ON customers.id = sales.customer_id
         WHERE sales.id = ? AND customers.company_id = ?",
    )
    .bind(sale_id)
    .bind(company_id)
    .fetch_optional(pool)
    .await?;
    let Some(sale) = sale else {
        return Ok(None);
    };

    let slabs = sqlx::query_as::<_, SlabRow>(
        "SELECT CAST(slab.id AS SIGNED) AS id, BIN_TO_UUID(slab.room_uuid) AS room_id,
            slab.room, slab.backsplash, CAST(slab.square_feet AS CHAR) AS square_feet,
            CAST(slab.price AS CHAR) AS retail_price, slab.tear_out, slab.stove,
            slab.waterfall, slab.corbels, slab.seam,
            COALESCE(slab.ten_year_sealer, 0) AS ten_year_sealer,
            CAST(slab.extras AS CHAR) AS extras,
            EXISTS (SELECT 1 FROM slab_inventory child WHERE child.parent_id = slab.id)
              AS has_children
         FROM slab_inventory slab
         WHERE slab.sale_id = ?
         ORDER BY slab.id",
    )
    .bind(sale_id)
    .fetch_all(pool)
    .await?;
    if slabs.is_empty() {
        return Ok(None);
    }

    let slab_ids: Vec<i64> = slabs.iter().map(|slab| slab.id).collect();
    let sinks = fixtures(pool, "sinks", "sink_type_id", &slab_ids).await?;
    let faucets = fixtures(pool, "faucets", "faucet_type_id", &slab_ids).await?;
    let on_slab = |rows: &[FixtureRow], slab_id: i64| -> Vec<SaleFixture> {
        rows.iter()
            .filter(|row| row.slab_id == slab_id)
            .map(|row| SaleFixture {
                id: row.id,
                type_id: row.type_id,
                price: row.price.clone(),
            })
            .collect()
    };

    let mut rooms: Vec<SaleRoom> = Vec::new();
    for slab in slabs {
        let slab_ref = SaleSlab {
            id: slab.id,
            is_full: !slab.has_children,
        };
        if let Some(room) = rooms.iter_mut().find(|room| room.room_id == slab.room_id) {
            room.slabs.push(slab_ref);
            continue;
        }
        rooms.push(SaleRoom {
            sinks: on_slab(&sinks, slab.id),
            faucets: on_slab(&faucets, slab.id),
            room_id: slab.room_id,
            room: slab.room,
            backsplash: slab.backsplash,
            square_feet: slab.square_feet,
            retail_price: slab.retail_price,
            tear_out: slab.tear_out,
            stove: slab.stove,
            waterfall: slab.waterfall,
            corbels: slab.corbels,
            seam: slab.seam,
            ten_year_sealer: slab.ten_year_sealer,
            extras: parse_json(slab.extras),
            slabs: vec![slab_ref],
        });
    }

    Ok(Some(SaleContract {
        id: sale.id,
        customer_id: sale.customer_id,
        deal_id: sale.deal_id,
        seller_id: sale.seller_id,
        project_address: sale.project_address,
        notes: sale.notes,
        price: sale.price,
        company_name: sale.company_name,
        extras: parse_json(sale.extras),
        rooms,
    }))
}

async fn fixtures(
    pool: &MySqlPool,
    table: &str,
    type_column: &str,
    slab_ids: &[i64],
) -> Result<Vec<FixtureRow>, sqlx::Error> {
    let mut query = QueryBuilder::<MySql>::new(format!(
        "SELECT CAST(slab_id AS SIGNED) AS slab_id, id, {type_column} AS type_id,
            CAST(price AS CHAR) AS price
         FROM {table} WHERE is_deleted = 0"
    ));
    push_in(&mut query, "slab_id", slab_ids);
    query.push(" ORDER BY id");
    query.build_query_as::<FixtureRow>().fetch_all(pool).await
}

// ---- Validation shared by sell and edit

/// Checks the body against the company's data before anything is written.
/// `sale_id` is the sale being edited, whose own slabs may be reused.
pub async fn validate(
    conn: &mut MySqlConnection,
    company_id: i32,
    input: &SaleInput,
    sale_id: Option<i32>,
) -> Result<(), ApiError> {
    validate_rooms(&input.rooms)?;

    let customer: Option<i32> = sqlx::query_scalar(
        "SELECT id FROM customers WHERE id = ? AND company_id = ? AND deleted_at IS NULL",
    )
    .bind(input.customer_id)
    .bind(company_id)
    .fetch_optional(&mut *conn)
    .await?;
    if customer.is_none() {
        return Err(ApiError::BadRequest(format!(
            "customer {} not found",
            input.customer_id
        )));
    }

    if let Some(deal_id) = input.deal_id.filter(|id| *id > 0) {
        let deal: Option<i64> = sqlx::query_scalar(
            "SELECT CAST(id AS SIGNED) FROM deals
              WHERE id = ? AND customer_id = ? AND deleted_at IS NULL",
        )
        .bind(deal_id)
        .bind(input.customer_id)
        .fetch_optional(&mut *conn)
        .await?;
        if deal.is_none() {
            return Err(ApiError::BadRequest(
                "That deal was deleted or belongs to another customer. Pick another deal.".into(),
            ));
        }
    }

    if let Some(seller_id) = input.seller_id {
        let seller: Option<i32> =
            sqlx::query_scalar("SELECT id FROM users WHERE id = ? AND is_deleted = 0")
                .bind(seller_id)
                .fetch_optional(&mut *conn)
                .await?;
        if seller.is_none() {
            return Err(ApiError::BadRequest(format!(
                "seller {seller_id} not found"
            )));
        }
    }

    let slab_ids: Vec<i64> = input
        .rooms
        .iter()
        .flat_map(|room| room.slabs.iter().map(|slab| slab.id))
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    let mut query = QueryBuilder::<MySql>::new(
        "SELECT CAST(slab.id AS SIGNED) AS id, slab.sale_id
         FROM slab_inventory slab
         JOIN stones ON stones.id = slab.stone_id
         WHERE slab.deleted_at IS NULL AND stones.company_id = ",
    );
    query.push_bind(company_id);
    push_in(&mut query, "slab.id", &slab_ids);
    let found: Vec<(i64, Option<i32>)> = query.build_query_as().fetch_all(&mut *conn).await?;
    for id in &slab_ids {
        match found.iter().find(|(found_id, _)| found_id == id) {
            None => return Err(ApiError::BadRequest(format!("slab {id} not found"))),
            Some((_, Some(other))) if Some(*other) != sale_id => {
                return Err(ApiError::BadRequest(format!(
                    "slab {id} is already sold (sale {other})"
                )));
            }
            Some(_) => {}
        }
    }

    for (table, column, ids) in [
        (
            "sink_type",
            "sink",
            fixture_types(input, |room| &room.sinks),
        ),
        (
            "faucet_type",
            "faucet",
            fixture_types(input, |room| &room.faucets),
        ),
    ] {
        if ids.is_empty() {
            continue;
        }
        let mut query =
            QueryBuilder::<MySql>::new(format!("SELECT id FROM {table} WHERE company_id = "));
        query.push_bind(company_id);
        push_in(&mut query, "id", &ids);
        let found: Vec<i32> = query.build_query_scalar().fetch_all(&mut *conn).await?;
        if let Some(missing) = ids.iter().find(|id| !found.contains(id)) {
            return Err(ApiError::BadRequest(format!(
                "{column} model {missing} not found"
            )));
        }
    }
    Ok(())
}

/// Shape checks that need no database: rooms, slabs, square feet, room ids.
fn validate_rooms(rooms: &[RoomInput]) -> Result<(), ApiError> {
    if rooms.is_empty() {
        return Err(ApiError::BadRequest("at least one room is required".into()));
    }
    for (index, room) in rooms.iter().enumerate() {
        let number = index + 1;
        if room.slabs.is_empty() {
            return Err(ApiError::BadRequest(format!(
                "At least one slab is required in each room (room {number})"
            )));
        }
        if !room
            .square_feet
            .is_some_and(|sqft| sqft.is_finite() && sqft > 0.0)
        {
            return Err(ApiError::BadRequest(format!(
                "Square feet is required (room {number})"
            )));
        }
        if !is_uuid(&room.room_id) {
            return Err(ApiError::BadRequest(format!(
                "invalid room_id in room {number}: {}",
                room.room_id
            )));
        }
    }
    Ok(())
}

fn fixture_types(
    input: &SaleInput,
    pick: impl Fn(&RoomInput) -> &Vec<super::schemas::FixtureInput>,
) -> Vec<i32> {
    input
        .rooms
        .iter()
        .flat_map(|room| pick(room).iter().map(|fixture| fixture.type_id))
        .collect::<HashSet<_>>()
        .into_iter()
        .collect()
}

/// 8-4-4-4-12 hex digits, what `UUID_TO_BIN` accepts.
fn is_uuid(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() == 36
        && bytes.iter().enumerate().all(|(i, b)| match i {
            8 | 13 | 18 | 23 => *b == b'-',
            _ => b.is_ascii_hexdigit(),
        })
}

// ---- Writes

fn non_empty(value: Option<&str>) -> Option<&str> {
    value.filter(|text| !text.trim().is_empty())
}

fn total_square_feet(input: &SaleInput) -> f64 {
    input
        .rooms
        .iter()
        .map(|room| room.square_feet.unwrap_or(0.0))
        .sum()
}

async fn project_address(
    conn: &mut MySqlConnection,
    company_id: i32,
    input: &SaleInput,
) -> Result<String, sqlx::Error> {
    if let Some(address) = non_empty(input.project_address.as_deref()) {
        return Ok(address.to_string());
    }
    let address: Option<Option<String>> = sqlx::query_scalar(
        "SELECT address FROM customers WHERE id = ? AND company_id = ? AND deleted_at IS NULL",
    )
    .bind(input.customer_id)
    .bind(company_id)
    .fetch_optional(conn)
    .await?;
    Ok(address.flatten().unwrap_or_default())
}

/// A copy of a slab cut from it (`parent_id`), free and unsold.
async fn create_child_slab(conn: &mut MySqlConnection, parent_id: i64) -> Result<i64, sqlx::Error> {
    let result = sqlx::query(
        "INSERT INTO slab_inventory (stone_id, bundle, length, width, url, parent_id)
         SELECT stone_id, bundle, length, width, url, id FROM slab_inventory WHERE id = ?",
    )
    .bind(parent_id)
    .execute(conn)
    .await?;
    Ok(i64::try_from(result.last_insert_id()).unwrap_or(i64::MAX))
}

/// For every room and slab position, the slab actually sold there: the
/// first use of a slab sells it, later uses (other rooms) get a new piece cut
/// from it.
async fn assign_slabs(
    conn: &mut MySqlConnection,
    rooms: &[RoomInput],
) -> Result<Vec<Vec<i64>>, sqlx::Error> {
    let mut seen = HashSet::new();
    let mut assigned = Vec::with_capacity(rooms.len());
    for room in rooms {
        let mut ids = Vec::with_capacity(room.slabs.len());
        for slab in &room.slabs {
            if seen.insert(slab.id) {
                ids.push(slab.id);
            } else {
                ids.push(create_child_slab(conn, slab.id).await?);
            }
        }
        assigned.push(ids);
    }
    Ok(assigned)
}

async fn put_slab_in_sale(
    conn: &mut MySqlConnection,
    sale_id: i32,
    slab_id: i64,
    room: &RoomInput,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE slab_inventory SET sale_id = ?, room_uuid = UUID_TO_BIN(?), seam = ?, room = ?,
            backsplash = ?, tear_out = ?, square_feet = ?, stove = ?, ten_year_sealer = ?,
            waterfall = ?, corbels = ?, price = ?, extras = ?
         WHERE id = ?",
    )
    .bind(sale_id)
    .bind(&room.room_id)
    .bind(&room.seam)
    .bind(&room.room)
    .bind(&room.backsplash)
    .bind(&room.tear_out)
    .bind(room.square_feet.filter(|sqft| *sqft != 0.0))
    .bind(&room.stove)
    .bind(room.ten_year_sealer)
    .bind(&room.waterfall)
    .bind(room.corbels)
    .bind(room.retail_price)
    .bind(room.extras.to_string())
    .bind(slab_id)
    .execute(conn)
    .await?;
    Ok(())
}

const CLEAR_SLAB_SALE: &str = "sale_id = NULL, room_uuid = NULL, seam = NULL, room = NULL,
    backsplash = NULL, tear_out = NULL, square_feet = NULL, stove = NULL,
    ten_year_sealer = NULL, waterfall = NULL, corbels = NULL, price = NULL, extras = NULL";

/// Takes the lowest-id free unit of each model and installs it on `slab_id`.
async fn install_fixtures(
    conn: &mut MySqlConnection,
    table: &str,
    type_column: &str,
    label: &str,
    slab_id: i64,
    fixtures: &[super::schemas::FixtureInput],
) -> Result<(), ApiError> {
    for fixture in fixtures {
        let result = sqlx::query(AssertSqlSafe(format!(
            "UPDATE {table} SET slab_id = ?, price = ?
             WHERE {type_column} = ? AND slab_id IS NULL AND is_deleted = 0
             ORDER BY id LIMIT 1"
        )))
        .bind(slab_id)
        .bind(fixture.price)
        .bind(fixture.type_id)
        .execute(&mut *conn)
        .await?;
        if result.rows_affected() == 0 {
            return Err(ApiError::BadRequest(format!(
                "No available {label} of type {}. Cannot complete the sale.",
                fixture.type_id
            )));
        }
    }
    Ok(())
}

async fn install_room_fixtures(
    conn: &mut MySqlConnection,
    slab_id: i64,
    room: &RoomInput,
) -> Result<(), ApiError> {
    install_fixtures(conn, "sinks", "sink_type_id", "sinks", slab_id, &room.sinks).await?;
    install_fixtures(
        conn,
        "faucets",
        "faucet_type_id",
        "faucets",
        slab_id,
        &room.faucets,
    )
    .await
}

/// Removes the free remnants cut from a slab. Remnants another sale bought
/// stay (Remix deleted them too).
async fn delete_free_children(conn: &mut MySqlConnection, slab_id: i64) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM slab_inventory WHERE parent_id = ? AND sale_id IS NULL")
        .bind(slab_id)
        .execute(conn)
        .await?;
    Ok(())
}

async fn has_children(conn: &mut MySqlConnection, slab_id: i64) -> Result<bool, sqlx::Error> {
    let child: Option<i64> = sqlx::query_scalar(
        "SELECT CAST(id AS SIGNED) FROM slab_inventory WHERE parent_id = ? LIMIT 1",
    )
    .bind(slab_id)
    .fetch_optional(conn)
    .await?;
    Ok(child.is_some())
}

/// Remix: `Contract.sell` (plus the customer's first sales rep assignment of
/// `createSale`). Returns the new sale id.
pub async fn sell(
    conn: &mut MySqlConnection,
    company_id: i32,
    user: (i32, &str),
    input: &SaleInput,
) -> Result<i32, ApiError> {
    let (user_id, user_name) = user;
    let seller_id = input.seller_id.unwrap_or(user_id);
    let address = project_address(&mut *conn, company_id, input).await?;
    let result = sqlx::query(
        "INSERT INTO sales (customer_id, deal_id, seller_id, company_id, sale_date, notes,
            status, square_feet, cancelled_date, installed_date, price, project_address, extras)
         VALUES (?, ?, ?, ?, CURRENT_TIMESTAMP, ?, 'pending', ?, NULL, NULL, ?, ?, ?)",
    )
    .bind(input.customer_id)
    .bind(input.deal_id.filter(|id| *id > 0))
    .bind(seller_id)
    .bind(company_id)
    .bind(non_empty(input.notes.as_deref()))
    .bind(total_square_feet(input))
    .bind(input.price)
    .bind(address)
    .bind(input.extras.to_string())
    .execute(&mut *conn)
    .await?;
    let sale_id = i32::try_from(result.last_insert_id())
        .map_err(|_| ApiError::BadRequest("sale id out of range".into()))?;

    assign_first_sales_rep(
        &mut *conn,
        company_id,
        input.customer_id,
        seller_id,
        user_name,
    )
    .await?;

    let assigned = assign_slabs(&mut *conn, &input.rooms).await?;
    for (room, slab_ids) in input.rooms.iter().zip(&assigned) {
        for (slab, &slab_id) in room.slabs.iter().zip(slab_ids) {
            put_slab_in_sale(&mut *conn, sale_id, slab_id, room).await?;
            if !slab.is_full {
                create_child_slab(&mut *conn, slab_id).await?;
            }
        }
        install_room_fixtures(&mut *conn, slab_ids[0], room).await?;
    }
    Ok(sale_id)
}

/// A customer without a sales rep gets the seller, and the change is logged
/// in `customers_history` under the caller's name.
async fn assign_first_sales_rep(
    conn: &mut MySqlConnection,
    company_id: i32,
    customer_id: i32,
    seller_id: i32,
    assigned_by: &str,
) -> Result<(), sqlx::Error> {
    let updated = sqlx::query(
        "UPDATE customers SET sales_rep = ?, assigned_date = COALESCE(assigned_date, NOW())
         WHERE id = ? AND company_id = ? AND sales_rep IS NULL AND deleted_at IS NULL",
    )
    .bind(seller_id)
    .bind(customer_id)
    .bind(company_id)
    .execute(&mut *conn)
    .await?;
    let assigned_by: String = assigned_by.trim().chars().take(100).collect();
    if updated.rows_affected() == 0 || assigned_by.is_empty() {
        return Ok(());
    }
    let seller_name: Option<Option<String>> =
        sqlx::query_scalar("SELECT name FROM users WHERE id = ? AND is_deleted = 0")
            .bind(seller_id)
            .fetch_optional(&mut *conn)
            .await?;
    let seller_name = seller_name
        .flatten()
        .map(|name| name.trim().to_string())
        .filter(|name| !name.is_empty());
    sqlx::query(
        "INSERT INTO customers_history (customer_id, reassigned_by, reassigned_to) VALUES (?, ?, ?)",
    )
    .bind(customer_id)
    .bind(assigned_by)
    .bind(seller_name)
    .execute(conn)
    .await?;
    Ok(())
}

/// Remix: `Contract.edit`. The sale's slabs are taken back, then the rooms
/// are saved again as given.
pub async fn edit(
    conn: &mut MySqlConnection,
    company_id: i32,
    sale_id: i32,
    input: &SaleInput,
) -> Result<(), ApiError> {
    let address = project_address(&mut *conn, company_id, input).await?;
    sqlx::query(
        // deal_id first: MySQL assigns left to right, so customer_id is still the old one.
        "UPDATE sales SET
            deal_id = CASE WHEN ? IS NOT NULL THEN NULLIF(?, 0)
                           WHEN customer_id = ? THEN deal_id END,
            customer_id = ?, seller_id = COALESCE(?, seller_id), notes = ?,
            square_feet = ?, price = ?, project_address = ?, extras = ?
         WHERE id = ?",
    )
    .bind(input.deal_id)
    .bind(input.deal_id)
    .bind(input.customer_id)
    .bind(input.customer_id)
    .bind(input.seller_id)
    .bind(non_empty(input.notes.as_deref()))
    .bind(total_square_feet(input))
    .bind(input.price)
    .bind(address)
    .bind(input.extras.to_string())
    .bind(sale_id)
    .execute(&mut *conn)
    .await?;

    // Units on slabs leaving the sale are freed too (Remix left them installed).
    for table in ["sinks", "faucets"] {
        sqlx::query(AssertSqlSafe(format!(
            "UPDATE {table} SET slab_id = NULL, price = NULL
             WHERE slab_id IN (SELECT id FROM slab_inventory WHERE sale_id = ?)"
        )))
        .bind(sale_id)
        .execute(&mut *conn)
        .await?;
    }
    sqlx::query(AssertSqlSafe(format!(
        "UPDATE slab_inventory SET {CLEAR_SLAB_SALE} WHERE sale_id = ?"
    )))
    .bind(sale_id)
    .execute(&mut *conn)
    .await?;

    let assigned = assign_slabs(&mut *conn, &input.rooms).await?;
    for (room, slab_ids) in input.rooms.iter().zip(&assigned) {
        for (slab, &slab_id) in room.slabs.iter().zip(slab_ids) {
            put_slab_in_sale(&mut *conn, sale_id, slab_id, room).await?;
            let cut = has_children(&mut *conn, slab_id).await?;
            if !slab.is_full && !cut {
                create_child_slab(&mut *conn, slab_id).await?;
            } else if slab.is_full && cut {
                delete_free_children(&mut *conn, slab_id).await?;
            }
        }
        install_room_fixtures(&mut *conn, slab_ids[0], room).await?;
    }
    Ok(())
}

/// Remix: `Contract.unsell` (`POST /api/unsell/:sale`). Cancels the sale,
/// frees its slabs with their sinks and faucets, and removes the free
/// remnants its partial slabs left.
pub async fn unsell(conn: &mut MySqlConnection, sale_id: i32) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE sales SET cancelled_date = CURRENT_TIMESTAMP, status = 'cancelled' WHERE id = ?",
    )
    .bind(sale_id)
    .execute(&mut *conn)
    .await?;
    let slab_ids: Vec<i64> = sqlx::query_scalar(
        "SELECT CAST(id AS SIGNED) FROM slab_inventory WHERE sale_id = ? ORDER BY id",
    )
    .bind(sale_id)
    .fetch_all(&mut *conn)
    .await?;
    let mut partial = Vec::new();
    for &slab_id in &slab_ids {
        if has_children(&mut *conn, slab_id).await? {
            partial.push(slab_id);
        }
    }
    sqlx::query(AssertSqlSafe(format!(
        "UPDATE slab_inventory SET {CLEAR_SLAB_SALE} WHERE sale_id = ?"
    )))
    .bind(sale_id)
    .execute(&mut *conn)
    .await?;
    if !slab_ids.is_empty() {
        for table in ["sinks", "faucets"] {
            let mut query = QueryBuilder::<MySql>::new(format!(
                "UPDATE {table} SET slab_id = NULL, price = NULL WHERE 1 = 1"
            ));
            push_in(&mut query, "slab_id", &slab_ids);
            query.build().execute(&mut *conn).await?;
        }
    }
    for slab_id in partial {
        delete_free_children(&mut *conn, slab_id).await?;
    }
    Ok(())
}
