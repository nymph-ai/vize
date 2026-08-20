use std::path::{Component, Path, PathBuf};

use serde_json::Value;
use vize_carton::{CompactString, cstr};

use super::comparable_path;

const PACKAGE_EXTENSIONS: &[&str] = &[
    "d.ts", "d.mts", "d.cts", "ts", "tsx", "mts", "cts", "js", "jsx", "mjs", "cjs",
];

pub(super) fn resolve_package_import(importer_dir: &Path, specifier: &str) -> Option<PathBuf> {
    let (package, subpath) = split_package_specifier(specifier)?;
    let package_root = importer_dir
        .ancestors()
        .map(|directory| directory.join("node_modules").join(package))
        .find(|candidate| candidate.is_dir())?;
    let manifest = std::fs::read_to_string(package_root.join("package.json"))
        .ok()
        .and_then(|content| serde_json::from_str::<Value>(&content).ok());
    let has_exports = manifest
        .as_ref()
        .is_some_and(|manifest| manifest.get("exports").is_some());

    if let Some(target) = manifest
        .as_ref()
        .into_iter()
        .flat_map(|manifest| package_export_targets(manifest, subpath))
        .find_map(|target| resolve_package_target(&package_root, target.as_str()))
    {
        return Some(comparable_path(&target));
    }

    if has_exports {
        return None;
    }

    if let Some(subpath) = subpath {
        return resolve_package_candidate(package_root.join(subpath))
            .map(|path| comparable_path(&path));
    }

    manifest
        .as_ref()
        .and_then(|manifest| {
            ["types", "typings", "module", "main"]
                .iter()
                .find_map(|key| manifest.get(key).and_then(Value::as_str))
        })
        .and_then(|target| resolve_legacy_package_target(&package_root, target))
        .or_else(|| resolve_package_candidate(package_root.join("index")))
        .map(|path| comparable_path(&path))
}

fn split_package_specifier(specifier: &str) -> Option<(&str, Option<&str>)> {
    let mut parts = specifier.split('/');
    let first = parts.next()?;
    if first.is_empty() {
        return None;
    }

    if first.starts_with('@') {
        if first.len() == 1 {
            return None;
        }
        let name = parts.next()?;
        if name.is_empty() {
            return None;
        }
        let package_len = first.len() + 1 + name.len();
        let subpath = specifier
            .get(package_len + 1..)
            .filter(|value| !value.is_empty());
        return Some((&specifier[..package_len], subpath));
    }

    let subpath = specifier
        .get(first.len() + 1..)
        .filter(|value| !value.is_empty());
    Some((first, subpath))
}

fn package_export_targets(manifest: &Value, subpath: Option<&str>) -> Vec<CompactString> {
    let Some(exports) = manifest.get("exports") else {
        return Vec::new();
    };
    let key = subpath.map_or_else(|| cstr!("."), |subpath| cstr!("./{subpath}"));
    match exports.get(key.as_str()) {
        Some(entry) => return conditional_export_targets(entry, None),
        None if subpath.is_none()
            && exports.as_object().is_none_or(|conditions| {
                conditions
                    .keys()
                    .all(|condition| !condition.starts_with('.'))
            }) =>
        {
            return conditional_export_targets(exports, None);
        }
        None => {}
    }

    let Some(exports) = exports.as_object() else {
        return Vec::new();
    };
    exports
        .iter()
        .filter_map(|(pattern, entry)| {
            let capture = export_pattern_capture(pattern, key.as_str())?;
            let prefix_len = pattern.find('*')?;
            Some((prefix_len, pattern.len(), entry, capture))
        })
        .max_by_key(|(prefix_len, pattern_len, _, _)| (*prefix_len, *pattern_len))
        .map_or_else(Vec::new, |(_, _, entry, capture)| {
            conditional_export_targets(entry, Some(capture))
        })
}

fn export_pattern_capture<'a>(pattern: &str, requested: &'a str) -> Option<&'a str> {
    let (prefix, suffix) = pattern.split_once('*')?;
    if !pattern.starts_with("./")
        || suffix.contains('*')
        || requested.len() < prefix.len() + suffix.len()
        || !requested.starts_with(prefix)
        || !requested.ends_with(suffix)
    {
        return None;
    }
    requested.get(prefix.len()..requested.len() - suffix.len())
}

fn conditional_export_targets(value: &Value, capture: Option<&str>) -> Vec<CompactString> {
    let mut targets = Vec::new();
    collect_conditional_export_targets(value, capture, &mut targets);
    targets
}

fn collect_conditional_export_targets(
    value: &Value,
    capture: Option<&str>,
    targets: &mut Vec<CompactString>,
) {
    match value {
        Value::String(target) => targets.push(capture.map_or_else(
            || CompactString::from(target.as_str()),
            |capture| CompactString::from(target.replace('*', capture)),
        )),
        Value::Array(entries) => {
            for entry in entries {
                collect_conditional_export_targets(entry, capture, targets);
            }
        }
        Value::Object(conditions) => {
            for condition in ["types", "import", "require", "default"] {
                let Some(entry) = conditions.get(condition) else {
                    continue;
                };
                collect_conditional_export_targets(entry, capture, targets);
                return;
            }
        }
        _ => {}
    }
}

fn resolve_package_target(package_root: &Path, target: &str) -> Option<PathBuf> {
    let target = target.strip_prefix("./")?;
    resolve_relative_package_target(package_root, target)
}

fn resolve_legacy_package_target(package_root: &Path, target: &str) -> Option<PathBuf> {
    resolve_relative_package_target(package_root, target.strip_prefix("./").unwrap_or(target))
}

fn resolve_relative_package_target(package_root: &Path, target: &str) -> Option<PathBuf> {
    let target = Path::new(target);
    if target.as_os_str().is_empty()
        || target.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        return None;
    }
    resolve_package_candidate(package_root.join(target))
}

fn resolve_package_candidate(base: PathBuf) -> Option<PathBuf> {
    if let Some(sidecar) = declaration_sidecar(&base) {
        return Some(sidecar);
    }
    if base.is_file() {
        return Some(base);
    }
    if base.extension().is_none() {
        for extension in PACKAGE_EXTENSIONS {
            let candidate = base.with_extension(extension);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
        for extension in PACKAGE_EXTENSIONS {
            let candidate = base.join("index").with_extension(extension);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

fn declaration_sidecar(base: &Path) -> Option<PathBuf> {
    let extensions: &[&str] = match base.extension().and_then(|extension| extension.to_str()) {
        Some("mjs") => &["d.mts", "d.ts"],
        Some("cjs") => &["d.cts", "d.ts"],
        Some("js" | "jsx") => &["d.ts"],
        _ => &[],
    };
    extensions
        .iter()
        .map(|extension| base.with_extension(extension))
        .find(|candidate| candidate.is_file())
}

#[cfg(test)]
mod tests;
