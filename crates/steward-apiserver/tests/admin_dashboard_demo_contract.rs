use std::fs;
use std::path::Path;

#[test]
fn localhost_demo_is_opt_in_and_absent_from_production_packaging() -> Result<(), String> {
    let crate_root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let manifest_name = ["Cargo", "toml"].join(".");
    let manifest = fs::read_to_string(crate_root.join(manifest_name))
        .map_err(|error| format!("read steward-apiserver manifest: {error}"))?;
    assert!(
        manifest.contains("admin-demo = []"),
        "the localhost demo must require an explicit, empty opt-in feature"
    );
    assert!(
        manifest.contains("required-features = [\"admin-demo\"]"),
        "the localhost demo example must not compile without its opt-in feature"
    );

    let package_name = ["package", "Dockerfile"].join(".");
    let package = fs::read_to_string(crate_root.join("../../build").join(package_name))
        .map_err(|error| format!("read production package Dockerfile: {error}"))?;
    assert!(
        package.contains("--bin steward-apiserver-bin"),
        "production packaging must continue to select the reviewed apiserver binary"
    );
    assert!(
        !package.contains("admin-dashboard-demo") && !package.contains("admin-demo"),
        "the localhost demo must be absent from the production image build"
    );
    Ok(())
}
