//! An in-memory `BillingStore` for the state machine and gate tests.
#![allow(clippy::future_not_send)]

use super::store::{BillingStore, StoreResult};
use super::{BillingInvoice, BillingNotification, CompanyBilling};
use chrono::NaiveDateTime;
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};

#[derive(Default)]
pub struct Memory {
    pub records: RefCell<HashMap<i32, CompanyBilling>>,
    pub invoices: RefCell<Vec<BillingInvoice>>,
    pub events: RefCell<HashSet<String>>,
    /// (user id, company id, is manager)
    pub users: RefCell<Vec<(i32, i32, bool)>>,
    pub notifications: RefCell<Vec<(BillingNotification, bool)>>,
    pub not_required: RefCell<HashSet<i32>>,
    /// `company.billing_required_at` by company.
    pub required_since: RefCell<HashMap<i32, NaiveDateTime>>,
    /// Reading a company's record fails (the database is down).
    pub fail_reads: Cell<bool>,
}

fn read_failed() -> sqlx::Error {
    sqlx::Error::Protocol("read failed".into())
}

#[allow(clippy::unused_async_trait_impl)] // the trait's signature
impl BillingStore for Memory {
    async fn by_company(&self, company_id: i32) -> StoreResult<Option<CompanyBilling>> {
        if self.fail_reads.get() {
            return Err(read_failed());
        }
        Ok(self.records.borrow().get(&company_id).cloned())
    }
    async fn by_customer(&self, customer_id: &str) -> StoreResult<Option<CompanyBilling>> {
        if self.fail_reads.get() {
            return Err(read_failed());
        }
        Ok(self
            .records
            .borrow()
            .values()
            .find(|r| r.stripe_customer_id.as_deref() == Some(customer_id))
            .cloned())
    }
    async fn by_subscription(&self, subscription_id: &str) -> StoreResult<Option<CompanyBilling>> {
        if self.fail_reads.get() {
            return Err(read_failed());
        }
        Ok(self
            .records
            .borrow()
            .values()
            .find(|r| r.stripe_subscription_id.as_deref() == Some(subscription_id))
            .cloned())
    }
    async fn save(&self, record: &CompanyBilling) -> StoreResult<()> {
        self.records
            .borrow_mut()
            .insert(record.company_id, record.clone());
        Ok(())
    }
    async fn claim_event(&self, event_id: &str, _: &str) -> StoreResult<bool> {
        Ok(self.events.borrow_mut().insert(event_id.to_string()))
    }
    async fn release_event(&self, event_id: &str) -> StoreResult<()> {
        self.events.borrow_mut().remove(event_id);
        Ok(())
    }
    async fn upsert_invoice(&self, invoice: &BillingInvoice) -> StoreResult<()> {
        let mut invoices = self.invoices.borrow_mut();
        match invoices
            .iter_mut()
            .find(|i| i.stripe_invoice_id == invoice.stripe_invoice_id)
        {
            Some(existing) => *existing = invoice.clone(),
            None => invoices.push(invoice.clone()),
        }
        Ok(())
    }
    async fn invoices(&self, company_id: i32) -> StoreResult<Vec<BillingInvoice>> {
        Ok(self
            .invoices
            .borrow()
            .iter()
            .filter(|i| i.company_id == company_id)
            .cloned()
            .collect())
    }
    async fn managers(&self, company_id: i32) -> StoreResult<Vec<i32>> {
        Ok(self
            .users
            .borrow()
            .iter()
            .filter(|(_, company, manager)| *company == company_id && *manager)
            .map(|(id, _, _)| *id)
            .collect())
    }
    async fn insert_notification(&self, notification: &BillingNotification) -> StoreResult<bool> {
        let mut rows = self.notifications.borrow_mut();
        if rows.iter().any(|(row, _)| {
            row.company_id == notification.company_id
                && row.user_id == notification.user_id
                && row.dedupe_key == notification.dedupe_key
        }) {
            return Ok(false);
        }
        rows.push((notification.clone(), false));
        Ok(true)
    }
    async fn clear_payment_notifications(&self, company_id: i32) -> StoreResult<()> {
        for (row, done) in self.notifications.borrow_mut().iter_mut() {
            if row.company_id == company_id && row.dedupe_key.starts_with("payment_required:") {
                *done = true;
            }
        }
        Ok(())
    }
    async fn billing_required(&self, company_id: i32) -> StoreResult<bool> {
        Ok(!self.not_required.borrow().contains(&company_id))
    }
    async fn billing_required_since(&self, company_id: i32) -> StoreResult<Option<NaiveDateTime>> {
        Ok(self.required_since.borrow().get(&company_id).copied())
    }
}

impl Memory {
    pub fn open_notifications(&self, company_id: i32) -> Vec<BillingNotification> {
        self.notifications
            .borrow()
            .iter()
            .filter(|(row, done)| row.company_id == company_id && !done)
            .map(|(row, _)| row.clone())
            .collect()
    }
    pub fn record(&self, company_id: i32) -> CompanyBilling {
        self.records.borrow()[&company_id].clone()
    }
}

pub fn at(text: &str) -> NaiveDateTime {
    NaiveDateTime::parse_from_str(text, "%Y-%m-%d %H:%M:%S").unwrap()
}
