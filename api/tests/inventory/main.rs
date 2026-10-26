//! Stones, slabs, sinks, faucets, suppliers and their files, transactions and
//! sales, documents and images (api-inventory).
//!
//! One file's tests: `cargo test -p api --test inventory <file>::`.

#[path = "../common/mod.rs"]
mod common;

mod attachable;
mod documents;
mod faucets;
mod fixture_models;
mod image_library;
mod installed_images;
mod places;
mod sale_contract;
mod sales;
mod shop;
mod sinks;
mod stone_admin;
mod stone_search;
mod stone_slabs;
mod stones;
mod supplier_files;
mod suppliers;
mod supports;
mod transaction_slabs;
mod transactions;
