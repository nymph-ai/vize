// CLI serialization and filesystem boundaries intentionally use owned standard strings.
#![allow(clippy::disallowed_types)]

use std::collections::BTreeMap;
use std::env;
use std::fmt::Write as _;
use std::fs;
use std::process::ExitCode;

use serde::Serialize;
use sha2::{Digest, Sha256};
use vize_atelier_syrinx::{SyrinxRsxOptions, compile_syrinx_checked};

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct EmitManifest {
    format: &'static str,
    source: String,
    source_sha256: String,
    compiler_revision: String,
    component_name: String,
    compiled_bindings: Vec<String>,
    outputs_sha256: BTreeMap<&'static str, String>,
}

fn main() -> ExitCode {
    let arguments = env::args().skip(1).collect::<Vec<_>>();
    if arguments.len() != 5 {
        eprintln!(
            "usage: syrinx-rsx-emit <component.vue> <component.rs> <style.css> \
             <coverage.json> <manifest.json>"
        );
        return ExitCode::from(2);
    }
    let source_path = &arguments[0];
    let source = match fs::read_to_string(source_path) {
        Ok(source) => source,
        Err(error) => {
            eprintln!("failed to read {source_path}: {error}");
            return ExitCode::FAILURE;
        }
    };
    let compiler_revision =
        env::var("VIZE_RSX_COMPILER_REVISION").unwrap_or_else(|_| "working-tree".to_owned());
    let options = SyrinxRsxOptions {
        filename: source_path.clone(),
        component_name: source_path
            .rsplit('/')
            .next()
            .and_then(|name| name.strip_suffix(".vue"))
            .map(str::to_owned),
        compiler_revision: compiler_revision.clone(),
        ..Default::default()
    };
    let artifact = match compile_syrinx_checked(&source, options) {
        Ok(artifact) => artifact,
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::FAILURE;
        }
    };
    let outputs = [
        ("rust", artifact.rsx.rust_source.as_str()),
        ("css", artifact.rsx.css.as_str()),
        ("coverage", artifact.classification_json.as_str()),
    ];
    let output_hashes = outputs
        .iter()
        .map(|(name, contents)| (*name, sha256(contents.as_bytes())))
        .collect();
    let manifest = EmitManifest {
        format: "syrinx-v3b-rsx-emit-v2",
        source: source_path.clone(),
        source_sha256: sha256(source.as_bytes()),
        compiler_revision,
        component_name: artifact.rsx.component_name,
        compiled_bindings: artifact.rsx.compiled_bindings,
        outputs_sha256: output_hashes,
    };
    let manifest_json = match serde_json::to_string_pretty(&manifest) {
        Ok(json) => json + "\n",
        Err(error) => {
            eprintln!("failed to serialize manifest: {error}");
            return ExitCode::FAILURE;
        }
    };
    let files = [
        (arguments[1].as_str(), artifact.rsx.rust_source.as_str()),
        (arguments[2].as_str(), artifact.rsx.css.as_str()),
        (arguments[3].as_str(), artifact.classification_json.as_str()),
        (arguments[4].as_str(), manifest_json.as_str()),
    ];
    for (path, contents) in files {
        if let Err(error) = fs::write(path, contents) {
            eprintln!("failed to write {path}: {error}");
            return ExitCode::FAILURE;
        }
    }
    ExitCode::SUCCESS
}

fn sha256(bytes: &[u8]) -> String {
    let mut hash = String::with_capacity(64);
    for byte in Sha256::digest(bytes) {
        let _ = write!(hash, "{byte:02x}");
    }
    hash
}
