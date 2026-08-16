use std::collections::BTreeMap;
use std::env;
use std::fmt::Write as _;
use std::fs;
use std::process::ExitCode;

use serde::Serialize;
use sha2::{Digest, Sha256};
use vize_atelier_syrinx::{SyrinxRsxOptions, compile_syrinx_hybrid};

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
    if arguments.len() != 6 {
        eprintln!(
            "usage: syrinx-rsx-emit <component.vue> <component.rs> <style.css> \
             <coverage.json> <residual.js> <manifest.json>"
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
    let artifact = match compile_syrinx_hybrid(&source, options) {
        Ok(artifact) => artifact,
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::FAILURE;
        }
    };
    let outputs = [
        (
            "rust",
            arguments[1].as_str(),
            artifact.rsx.rust_source.as_str(),
        ),
        ("css", arguments[2].as_str(), artifact.rsx.css.as_str()),
        (
            "coverage",
            arguments[3].as_str(),
            artifact.classification_json.as_str(),
        ),
        (
            "residual",
            arguments[4].as_str(),
            artifact.residual_module.as_str(),
        ),
    ];
    let mut output_hashes = BTreeMap::new();
    for (name, path, contents) in outputs {
        if let Err(error) = fs::write(path, contents) {
            eprintln!("failed to write {path}: {error}");
            return ExitCode::FAILURE;
        }
        output_hashes.insert(name, sha256(contents.as_bytes()));
    }
    let manifest = EmitManifest {
        format: "syrinx-v3b-rsx-emit-v1",
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
    if let Err(error) = fs::write(&arguments[5], manifest_json) {
        eprintln!("failed to write {}: {error}", arguments[5]);
        return ExitCode::FAILURE;
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
