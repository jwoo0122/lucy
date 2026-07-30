#[test]
fn trust_model_links_and_authority_wording_stay_consistent() {
    let trust = include_str!("../docs/trust-model.md");
    let readme = include_str!("../README.md");
    let homepage = include_str!("../site/index.html");

    for required in [
        "arbitrary shell command text",
        "not a security sandbox",
        "allowed by default",
        "removes the active API-provider credential variable",
        "External isolation",
    ] {
        assert!(
            trust.contains(required),
            "missing trust-model claim: {required}"
        );
    }
    assert!(readme.contains("[trust model](docs/trust-model.md)"));
    assert!(readme.contains("not a sandbox"));
    assert!(!readme.contains("Safe local command execution"));
    assert!(homepage.contains("docs/trust-model.md"));
    assert!(homepage.contains("not a sandbox"));
    assert!(homepage.contains("external isolation"));
}
