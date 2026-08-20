#![allow(clippy::disallowed_methods)]

use super::{package_export_targets, resolve_package_import, split_package_specifier};

#[test]
fn package_specifiers_preserve_scopes_and_subpaths() {
    assert_eq!(split_package_specifier("vue"), Some(("vue", None)));
    assert_eq!(
        split_package_specifier("vue-router/auto-routes"),
        Some(("vue-router", Some("auto-routes")))
    );
    assert_eq!(
        split_package_specifier("@vue/language-core/lib/types"),
        Some(("@vue/language-core", Some("lib/types")))
    );
    assert_eq!(
        split_package_specifier("@vue/language-core"),
        Some(("@vue/language-core", None))
    );
    assert_eq!(split_package_specifier(""), None);
    assert_eq!(split_package_specifier("@vue"), None);
    assert_eq!(split_package_specifier("@/invalid"), None);
}

#[test]
fn package_exports_select_types_without_guessing_a_root_subpath() {
    let manifest = serde_json::json!({
        "exports": {
            ".": {
                "types": "./dist/index.d.mts",
                "import": "./dist/index.mjs"
            },
            "./auto-routes": [
                null,
                { "types": "./routes.d.cts", "default": "./routes.cjs" }
            ]
        }
    });
    assert_eq!(
        package_export_targets(&manifest, None),
        ["./dist/index.d.mts"]
    );
    assert_eq!(
        package_export_targets(&manifest, Some("auto-routes")),
        ["./routes.d.cts"]
    );
    assert!(package_export_targets(&manifest, Some("missing")).is_empty());

    let conditional_root = serde_json::json!({
        "exports": {
            "types": "./index.d.ts",
            "default": "./index.js"
        }
    });
    assert_eq!(
        package_export_targets(&conditional_root, None),
        ["./index.d.ts"]
    );

    let subpaths_only = serde_json::json!({
        "exports": { "./feature": { "types": "./feature.d.ts" } }
    });
    assert!(package_export_targets(&subpaths_only, None).is_empty());
}

#[test]
fn package_exports_are_authoritative_and_support_nested_wildcards() {
    let dir = tempfile::tempdir().unwrap();
    let package = dir.path().join("node_modules/@scope/router");
    let nested_declaration = package.join("types/features/admin.d.mts");
    std::fs::create_dir_all(nested_declaration.parent().unwrap()).unwrap();
    std::fs::write(&nested_declaration, "export declare const route: unknown").unwrap();
    std::fs::write(
        package.join("private.d.ts"),
        "export declare const secret: unknown",
    )
    .unwrap();
    std::fs::write(
        package.join("package.json"),
        r#"{
  "types": "./private.d.ts",
  "exports": {
    "./features/*": { "types": "./types/features/*.d.mts" },
    "./*": { "types": "./types/*.d.ts" }
  }
}"#,
    )
    .unwrap();

    assert_eq!(
        resolve_package_import(dir.path(), "@scope/router/features/admin"),
        Some(std::fs::canonicalize(&nested_declaration).unwrap())
    );
    assert_eq!(
        resolve_package_import(dir.path(), "@scope/router/private"),
        None
    );
    assert_eq!(resolve_package_import(dir.path(), "@scope/router"), None);
}

#[test]
fn package_runtime_exports_prefer_declaration_sidecars() {
    let dir = tempfile::tempdir().unwrap();
    let package = dir.path().join("node_modules/runtime-package");
    let runtime = package.join("dist/index.mjs");
    let declaration = package.join("dist/index.d.mts");
    std::fs::create_dir_all(runtime.parent().unwrap()).unwrap();
    std::fs::write(&runtime, "export const value = 1").unwrap();
    std::fs::write(&declaration, "export declare const value: number").unwrap();
    std::fs::write(
        package.join("package.json"),
        r#"{ "exports": { ".": { "import": "./dist/index.mjs" } } }"#,
    )
    .unwrap();

    assert_eq!(
        resolve_package_import(dir.path(), "runtime-package"),
        Some(std::fs::canonicalize(&declaration).unwrap())
    );
}

#[test]
fn package_export_arrays_fall_back_without_escaping_the_package() {
    let dir = tempfile::tempdir().unwrap();
    let package = dir.path().join("node_modules/fallback-package");
    let valid = package.join("dist/valid.d.ts");
    let parent_escape = dir.path().join("node_modules/escape.d.ts");
    let absolute_escape = dir.path().join("absolute-escape.d.ts");
    std::fs::create_dir_all(valid.parent().unwrap()).unwrap();
    std::fs::write(&valid, "export declare const valid: true").unwrap();
    std::fs::write(&parent_escape, "export declare const escaped: true").unwrap();
    std::fs::write(&absolute_escape, "export declare const escaped: true").unwrap();
    let manifest = serde_json::json!({
        "exports": {
            ".": ["./dist/missing.d.ts", "./dist/valid.d.ts"],
            "./parent-escape": "./../escape.d.ts",
            "./absolute-escape": absolute_escape.to_string_lossy(),
        }
    });
    std::fs::write(
        package.join("package.json"),
        serde_json::to_vec_pretty(&manifest).unwrap(),
    )
    .unwrap();

    assert_eq!(
        package_export_targets(&manifest, None),
        ["./dist/missing.d.ts", "./dist/valid.d.ts"]
    );
    assert_eq!(
        resolve_package_import(dir.path(), "fallback-package"),
        Some(std::fs::canonicalize(&valid).unwrap())
    );
    assert_eq!(
        resolve_package_import(dir.path(), "fallback-package/parent-escape"),
        None
    );
    assert_eq!(
        resolve_package_import(dir.path(), "fallback-package/absolute-escape"),
        None
    );
}

#[test]
fn package_export_conditions_block_empty_and_unknown_targets() {
    let dir = tempfile::tempdir().unwrap();
    let package = dir.path().join("node_modules/conditional-package");
    let runtime = package.join("dist/runtime.mjs");
    let declaration = package.join("dist/runtime.d.mts");
    let browser = package.join("dist/browser.d.ts");
    std::fs::create_dir_all(runtime.parent().unwrap()).unwrap();
    std::fs::write(&runtime, "export const runtime = true").unwrap();
    std::fs::write(&declaration, "export declare const runtime: true").unwrap();
    std::fs::write(&browser, "export declare const browser: true").unwrap();
    std::fs::write(
        package.join("package.json"),
        r#"{
  "exports": {
    ".": { "types": null, "import": "./dist/runtime.mjs" },
    "./unknown": { "browser": "./dist/browser.d.ts" }
  }
}"#,
    )
    .unwrap();

    assert_eq!(
        resolve_package_import(dir.path(), "conditional-package"),
        None
    );
    assert_eq!(
        resolve_package_import(dir.path(), "conditional-package/unknown"),
        None
    );
}

#[test]
fn package_export_patterns_prefer_the_longest_prefix_before_total_length() {
    let dir = tempfile::tempdir().unwrap();
    let package = dir.path().join("node_modules/pattern-package");
    let expected = package.join("types/admin/settings.d.ts");
    let longer_pattern = package.join("types/settings/admin.d.ts");
    std::fs::create_dir_all(expected.parent().unwrap()).unwrap();
    std::fs::create_dir_all(longer_pattern.parent().unwrap()).unwrap();
    std::fs::write(&expected, "export declare const selected: 'prefix'").unwrap();
    std::fs::write(&longer_pattern, "export declare const selected: 'length'").unwrap();
    std::fs::write(
        package.join("package.json"),
        r#"{
  "exports": {
    "./features/admin/*": "./types/admin/*.d.ts",
    "./features/*/settings": "./types/settings/*.d.ts"
  }
}"#,
    )
    .unwrap();

    assert_eq!(
        resolve_package_import(dir.path(), "pattern-package/features/admin/settings"),
        Some(std::fs::canonicalize(&expected).unwrap())
    );
}
