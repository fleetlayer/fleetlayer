// `sqlx::migrate!` embeds migrations/ at compile time: rebuild when a migration is added or changed, or a new one is
// silently left out of the binary (sqlx's own advice).
fn main() {
    println!("cargo:rerun-if-changed=migrations");
}
