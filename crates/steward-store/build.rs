fn main() {
    // sqlx::migrate! cannot otherwise notice newly added migration files.
    println!("cargo:rerun-if-changed=../../migrations");
}
