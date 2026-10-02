fn main() {
    // Track new migration files as well as edits to embedded ones.
    // https://docs.rs/sqlx/0.9.0/sqlx/macro.migrate.html
    println!("cargo:rerun-if-changed=migrations");
}
