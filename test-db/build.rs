// `sqlx::migrate!` and `include_str!` embed these at compile time; rebuild
// (and so get a new template hash) when a migration is added or the seed changes.
fn main() {
    println!("cargo:rerun-if-changed=../migrations");
    println!("cargo:rerun-if-changed=../api/seed/local_seed.sql");
}
