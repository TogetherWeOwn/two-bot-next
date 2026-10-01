fn main() {
    // Adding a feature migration must refresh sqlx's compile-time embedding.
    println!("cargo:rerun-if-changed=migrations");
}
