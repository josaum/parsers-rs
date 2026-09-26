#[test]
fn core_manifest_has_no_fetch_runtime_or_external_state_dependencies() {
    let manifest = include_str!("../Cargo.toml");
    for forbidden in [
        "reqwest",
        "tokio",
        "chromium",
        "redis",
        "sqlx",
        "object_store",
    ] {
        assert!(
            !manifest.contains(forbidden),
            "web-fast-core must remain side-effect-free: found {forbidden}"
        );
    }
}
