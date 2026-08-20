#![allow(
    clippy::disallowed_macros,
    clippy::disallowed_methods,
    clippy::disallowed_types
)]

use std::fs;
use std::process::Command;

use tempfile::tempdir;

#[test]
fn rejected_source_exits_before_writing_any_artifact() {
    let directory = tempdir().expect("temporary directory");
    let source = directory.path().join("Rejected.vue");
    fs::write(
        &source,
        "<script setup>const label = () => fancyFormat()</script><template>{{ label() }}</template>",
    )
    .expect("write source");
    let outputs = [
        "component.rs",
        "style.css",
        "coverage.json",
        "manifest.json",
    ]
    .map(|name| directory.path().join(name));
    for output in &outputs {
        fs::write(output, "sentinel").expect("write sentinel");
    }

    let result = Command::new(env!("CARGO_BIN_EXE_syrinx-rsx-emit"))
        .arg(&source)
        .args(&outputs)
        .output()
        .expect("run emitter");

    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("SYRINX_RSX_UNLOWERED_EXPRESSION"));
    for output in &outputs {
        assert_eq!(fs::read_to_string(output).unwrap(), "sentinel");
    }
}

#[test]
fn successful_emit_writes_only_rsx_css_coverage_and_manifest() {
    let directory = tempdir().expect("temporary directory");
    let source = directory.path().join("Compiled.vue");
    fs::write(&source, "<template><p>compiled</p></template>").expect("write source");
    let outputs = [
        "component.rs",
        "style.css",
        "coverage.json",
        "manifest.json",
    ]
    .map(|name| directory.path().join(name));

    let status = Command::new(env!("CARGO_BIN_EXE_syrinx-rsx-emit"))
        .arg(&source)
        .args(&outputs)
        .status()
        .expect("run emitter");

    assert!(status.success());
    assert!(fs::read_to_string(&outputs[0]).unwrap().contains("rsx!"));
    let coverage: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&outputs[2]).unwrap()).unwrap();
    assert_eq!(coverage["format"], "syrinx-v3b-rsx-coverage-v2");
    assert!(coverage.get("decision").is_none());
    assert!(coverage["summary"].get("residualSites").is_none());
    let manifest: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&outputs[3]).unwrap()).unwrap();
    assert_eq!(manifest["format"], "syrinx-v3b-rsx-emit-v2");
    assert_eq!(manifest["outputsSha256"].as_object().unwrap().len(), 3);
    assert!(manifest["outputsSha256"].get("residual").is_none());
}
