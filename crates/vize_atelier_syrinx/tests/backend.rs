#![allow(clippy::disallowed_macros, clippy::disallowed_types)]

use oxc_allocator::Allocator;
use oxc_parser::Parser;
use oxc_span::SourceType;
use sha2::{Digest, Sha256};
use vize_atelier_syrinx::{
    BindingKind, SyrinxCompileOptions, SyrinxRsxOptions, compile_syrinx, compile_syrinx_hybrid,
    compile_syrinx_rsx,
};

const TABLE: &str = r#"<script setup>
import { computed, ref } from 'vue'

const props = defineProps({ rows: Array })
const hovered = ref(null)
const selected = ref(null)
const rootClass = computed(() => selected.value === null ? 'table' : 'table editing')
const metaLine = (row, index) => `${index + 1}: ${row.label}`
const choose = id => { selected.value = id }
</script>

<template>
  <section :class="rootClass">
    <table data-kind="transcript">
      <tbody>
        <tr
          v-for="(row, index) in props.rows"
          :key="row.id"
          :class="{ sparkle: hovered === row.id }"
          @mouseenter="hovered = row.id"
        >
          <td
            :data-row="row.id"
            :style="{ color: selected === row.id ? 'gold' : 'inherit' }"
            @click.stop="choose(row.id)"
          >{{ metaLine(row, index) }}</td>
        </tr>
      </tbody>
    </table>
    <aside v-if="selected">selected {{ selected }}</aside>
    <span v-else>none</span>
  </section>
</template>

<style scoped>
.table { width: 100%; }
.sparkle { color: gold; }
</style>
"#;

const CANONICAL_TIME_TRAVEL_TABLE: &str = include_str!("fixtures/TimeTravelTable.vue");
const CANONICAL_TIME_TRAVEL_TABLE_SHA256: &str =
    "f93c37898853f1c44584daa672cb97859db42ddf908bb52d00b1925f0295897a";

fn options() -> SyrinxCompileOptions {
    SyrinxCompileOptions {
        filename: "TimeTravelTable.vue".to_owned(),
        component_name: Some("TimeTravelTable".to_owned()),
        component_id: 100,
        protocol_schema_sha256: "a".repeat(64),
        ..Default::default()
    }
}

fn rsx_options() -> SyrinxRsxOptions {
    SyrinxRsxOptions {
        filename: "TimeTravelTable.vue".to_owned(),
        component_name: Some("TimeTravelTable".to_owned()),
        ..Default::default()
    }
}

fn assert_javascript_module(source: &str) {
    let allocator = Allocator::default();
    let parsed = Parser::new(&allocator, source, SourceType::default().with_module(true)).parse();
    assert!(
        parsed.diagnostics.is_empty(),
        "guest module did not parse: {:?}\n\n{source}",
        parsed.diagnostics
    );
}

#[test]
fn v3b_emits_deterministic_explicit_rsx_without_the_v2_abi() {
    let first = compile_syrinx_rsx(TABLE, rsx_options()).expect("table SFC should compile to RSX");
    let second =
        compile_syrinx_rsx(TABLE, rsx_options()).expect("repeat RSX compilation should work");

    assert_eq!(first, second, "identical inputs must be byte deterministic");
    assert_eq!(first.component_name, "TimeTravelTable");
    for required in [
        "pub struct TimeTravelTableModel",
        "pub struct TimeTravelTableList1Item",
        "pub fn TimeTravelTable",
        "rsx! {",
        "section {",
        "table {",
        "for (_index_1, item_1) in model.list_1.iter().enumerate() {",
        "key:",
        "if selected.read().is_some()",
        "onmouseenter:",
        "onclick:",
    ] {
        assert!(
            first.rust_source.contains(required),
            "RSX artifact missed {required:?}:\n{}",
            first.rust_source
        );
    }
    for forbidden in [
        "guest.mjs",
        "ComponentPlan",
        "protocol",
        "NodeId",
        "querySelector",
        "dangerous_inner_html",
    ] {
        assert!(
            !first.rust_source.contains(forbidden),
            "RSX artifact retained forbidden v2 token {forbidden:?}"
        );
    }
    assert!(
        first
            .expression_hooks
            .iter()
            .any(|hook| hook.role == "list" && hook.expression.contains("props.rows"))
    );
    assert!(
        first
            .expression_hooks
            .iter()
            .any(|hook| hook.role == "event:click")
    );
    assert_eq!(
        first.compiled_bindings,
        ["choose", "hovered", "metaLine", "rootClass", "selected"]
    );
    assert!(first.rust_source.contains("use_signal(|| None::<String>)"));
    assert!(
        first
            .rust_source
            .contains("let rootClass = if selected.read().is_none()")
    );
    assert!(!first.rust_source.contains("pub attribute_1: String"));
    assert_eq!(
        first.css,
        ".table { width: 100%; }\n.sparkle { color: gold; }"
    );
}

#[test]
fn exact_canonical_sfc_compiles_through_rsx_and_stock_vapor_backends() {
    let source_hash = Sha256::digest(CANONICAL_TIME_TRAVEL_TABLE.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    assert_eq!(source_hash, CANONICAL_TIME_TRAVEL_TABLE_SHA256);
    let artifact = compile_syrinx_hybrid(CANONICAL_TIME_TRAVEL_TABLE, rsx_options())
        .expect("canonical SFC should compile through Syrinx RSX");
    assert_eq!(artifact.classification.summary.rejected_sites, 0);
    assert_eq!(artifact.classification.summary.residual_render_sites, 0);
    assert_eq!(artifact.rsx.compiled_bindings, ["hovered", "rootClass"]);

    let descriptor = vize_atelier_sfc::parse_sfc(
        CANONICAL_TIME_TRAVEL_TABLE,
        vize_atelier_sfc::SfcParseOptions {
            filename: "TimeTravelTable.vue".into(),
            ..Default::default()
        },
    )
    .expect("canonical SFC should parse for stock Vapor");
    let vapor = vize_atelier_sfc::compile_sfc(
        &descriptor,
        vize_atelier_sfc::SfcCompileOptions {
            vapor: true,
            ..Default::default()
        },
    )
    .expect("canonical SFC should compile through stock Vapor");
    assert!(
        vapor.errors.is_empty(),
        "stock Vapor errors: {:?}",
        vapor.errors
    );
    assert!(vapor.code.contains("render"));
}

#[test]
fn v3b_hybrid_emit_is_empty_for_the_fully_compiled_table() {
    let artifact = compile_syrinx_hybrid(TABLE, rsx_options()).expect("table should classify");
    assert!(artifact.residual_exports.is_empty());
    assert_eq!(
        artifact.residual_module,
        "// @generated by vize_atelier_syrinx. Pure args-in/value-out residuals only.\nexport {};\n"
    );
    assert_javascript_module(&artifact.residual_module);
}

#[test]
fn v3b_residual_exports_are_pure_functions_with_explicit_dependencies() {
    let source = r#"<script setup>
const format = value => fancyFormat(value)
const label = format('alpha')
</script>
<template><p>{{ label }}</p></template>
"#;
    let mut compile_options = rsx_options();
    compile_options.filename = "ResidualLabel.vue".to_owned();
    compile_options.component_name = Some("ResidualLabel".to_owned());
    let artifact = compile_syrinx_hybrid(source, compile_options).expect("pure residuals compile");

    assert!(!artifact.residual_exports.is_empty());
    let format = artifact
        .residual_exports
        .iter()
        .find(|export| export.role == "format")
        .expect("format binding is residual");
    assert_eq!(format.dependencies, ["fancyFormat"]);
    assert!(artifact.residual_module.contains("(fancyFormat)"));
    assert!(!artifact.residual_module.contains("document"));
    assert_javascript_module(&artifact.residual_module);
}

#[test]
fn v3b_residual_that_closes_over_reactive_state_names_the_binding_and_fails() {
    let source = r#"<script setup>
import { ref } from 'vue'
const count = ref(0)
const label = () => fancyFormat(count.value)
</script>
<template><p>{{ label() }}</p></template>
"#;
    let mut compile_options = rsx_options();
    compile_options.filename = "RejectedResidual.vue".to_owned();
    compile_options.component_name = Some("RejectedResidual".to_owned());
    let error = compile_syrinx_hybrid(source, compile_options)
        .expect_err("a residual reactive closure must fail");
    assert!(error.diagnostics.iter().any(|diagnostic| {
        diagnostic.code == "SYRINX_RSX_RESIDUAL_REJECTED"
            && diagnostic.message.contains("'label'")
            && diagnostic.message.contains("reactive state")
            && diagnostic.start_byte < diagnostic.end_byte
    }));
}

#[test]
fn ordinary_table_matches_nym_754_contract_and_emits_five_deterministic_artifacts() {
    let first = compile_syrinx(TABLE, options()).expect("table SFC should compile");
    let second = compile_syrinx(TABLE, options()).expect("repeat compilation should compile");

    assert_eq!(first, second, "identical inputs must be byte deterministic");
    assert!(!first.plan_json.is_empty());
    assert!(!first.guest_module.is_empty());
    assert!(!first.css.is_empty());
    assert!(!first.manifest_json.is_empty());
    assert!(!first.source_map_json.is_empty());
    assert_eq!(
        first.files().map(|(name, _)| name),
        [
            "component.plan.json",
            "guest.mjs",
            "style.css",
            "manifest.json",
            "source-map.json",
        ]
    );
    assert_eq!(
        first.manifest.input_names.get(&1).map(String::as_str),
        Some("rows")
    );
    assert_javascript_module(&first.guest_module);
    assert!(!first.plan_json.contains("readExpression"));
    assert!(!first.plan_json.contains("handlerExpression"));
    assert!(!first.plan_json.contains("conditionExpression"));
    assert!(!first.plan_json.contains("sourceExpression"));

    let forbidden = [
        "@vue/runtime-vapor",
        "document.",
        "querySelector",
        "getElementById",
        "parentNode",
        "nextSibling",
        "Blitz",
        "NodeId",
    ];
    for token in forbidden {
        assert!(
            !first.guest_module.contains(token),
            "guest contained forbidden renderer token {token}"
        );
    }
    assert!(first.guest_module.contains("metaLine"));
    assert!(first.guest_module.contains("__installKeyedList"));
    assert!(first.guest_module.contains("__installBranch"));
    assert!(
        first.guest_module.contains("__syrinxValue1.value"),
        "keyed item bindings must read the runtime-updated value slot\n{}",
        first.guest_module
    );
    assert!(
        first.guest_module.contains("__syrinxKey1.value"),
        "the second Vue list alias must read the runtime-updated key/index slot\n{}",
        first.guest_module
    );

    let component = &first.plan.components[0];
    // NYM-754's hand-authored architecture fixture reserves component 100,
    // keyed row site 1, conditional menu site 1, and the same ordinary
    // binding/handler categories. This comparison intentionally excludes the
    // typed capability targets owned by the NYM-756 SFC-profile issue.
    assert_eq!(component.id, 100);
    assert_eq!(component.name, "TimeTravelTable");
    assert_eq!(component.template, 1);
    assert_eq!(component.list_sites.len(), 1);
    assert_eq!(component.list_sites[0].id, 1);
    assert_eq!(component.if_sites.len(), 1);
    assert_eq!(component.if_sites[0].id, 1);
    assert!(component.handlers.len() >= 2);
    assert!(
        component
            .bindings
            .iter()
            .any(|site| site.kind == BindingKind::Text)
    );
    assert!(
        component
            .bindings
            .iter()
            .any(|site| site.kind == BindingKind::Class)
    );
    assert!(
        component
            .bindings
            .iter()
            .any(|site| site.kind == BindingKind::Attribute)
    );
    assert!(
        component.bindings.iter().any(|site| {
            site.kind == BindingKind::Style && site.name.as_deref() == Some("color")
        })
    );
    assert_eq!(first.plan.stylesheets.len(), 1);
    assert!(first.plan.stylesheets[0].scope_token.is_some());
    let scope = first.plan.stylesheets[0].scope_token.as_deref().unwrap();
    assert!(
        first
            .plan
            .templates
            .iter()
            .any(|template| template.markup.contains(scope))
    );
    assert!(first.css.contains(scope));
    for binding in &component.bindings {
        assert_eq!(
            first
                .source_map
                .guest_expression_spans
                .get(&format!("binding:{}:read", binding.id)),
            Some(&binding.source_span),
            "every plan binding must map its retained guest expression"
        );
    }
    for handler in &component.handlers {
        assert_eq!(
            first
                .source_map
                .guest_expression_spans
                .get(&format!("handler:{}:invoke", handler.id)),
            Some(&handler.source_span),
            "every plan handler must map its retained guest expression"
        );
    }
    assert!(
        first
            .source_map
            .guest_expression_spans
            .contains_key("if:1:condition:0")
    );
    assert!(
        first
            .source_map
            .guest_expression_spans
            .contains_key("list:1:source")
    );
    assert!(
        first
            .source_map
            .guest_expression_spans
            .contains_key("list:1:key")
    );
    let guest_hash = Sha256::digest(first.guest_module.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    assert_eq!(first.plan.guest_module_sha256, guest_hash);
    assert_eq!(
        first.manifest.artifact_sha256.get("guest.mjs"),
        Some(&first.plan.guest_module_sha256)
    );
    assert_eq!(
        first.manifest.vize_revision,
        "fd841c9fb20edc6e538d1c951e16a9780ae4e013"
    );
    assert_eq!(first.manifest.guest_runtime_version, "2.2.0");
}

#[test]
fn refs_and_browser_node_identity_fail_with_source_spans() {
    let reference =
        r#"<script setup>const el = ref(null)</script><template><div ref="el" /></template>"#;
    let error = compile_syrinx(reference, options()).expect_err("template refs must fail");
    assert!(error.diagnostics.iter().any(|diagnostic| diagnostic.code
        == "SYRINX_TEMPLATE_REF_FORBIDDEN"
        && diagnostic.end_byte > diagnostic.start_byte));

    let browser = r#"<script setup>const box = document.querySelector('#box')</script><template><div /></template>"#;
    let error = compile_syrinx(browser, options()).expect_err("browser identity must fail");
    assert!(error.diagnostics.iter().any(|diagnostic| {
        diagnostic.code == "SYRINX_BROWSER_NODE_IDENTITY_FORBIDDEN"
            && diagnostic.end_byte > diagnostic.start_byte
    }));
}

#[test]
fn destructured_for_alias_fails_closed() {
    let source = r#"<script setup>const rows = [{ id: 1 }]</script>
<template><p v-for="{ id } in rows" :key="id">{{ id }}</p></template>"#;
    let error =
        compile_syrinx(source, options()).expect_err("alias patterns need explicit lowering");
    assert!(error.diagnostics.iter().any(|diagnostic| {
        diagnostic.code == "SYRINX_UNSUPPORTED_FOR_ALIAS_PATTERN"
            && diagnostic.end_byte > diagnostic.start_byte
    }));
}

#[test]
fn script_analysis_is_ast_exact_and_keeps_arbitrary_helpers() {
    let typescript = r#"<script setup lang="ts">
import{ref}from"vue"
type Row = { id: string }
const props = defineProps<{ rows: Row[] }>()
const selected = ref<string | null>(null)
const helper = (row: Row): string => `${row.id}:${selected.value ?? 'none'}`
const nested = async () => await Promise.resolve('fine')
</script>
<template><p v-for="row in props.rows" :key="row.id">{{ helper(row) }}</p></template>"#;
    let artifacts = compile_syrinx(typescript, options()).expect("nested async helpers remain JS");
    assert_javascript_module(&artifacts.guest_module);
    assert!(
        artifacts
            .guest_module
            .contains("from \"@nymphai/syrinx-guest-runtime\"")
    );
    assert!(!artifacts.guest_module.contains("from\"vue\""));
    assert!(artifacts.guest_module.contains("Promise.resolve"));
    assert!(!artifacts.guest_module.contains(": Row"));

    let shadowed = r#"<script setup>
const document = { label: 'authored value' }
</script><template><p>{{ document.label }}</p></template>"#;
    compile_syrinx(shadowed, options()).expect("a lexically bound name is not a browser global");

    let template_literal = r#"<script setup>
const identity = `${document.body}`
</script><template><p>{{ identity }}</p></template>"#;
    let error = compile_syrinx(template_literal, options())
        .expect_err("template substitutions are real references");
    assert!(error.diagnostics.iter().any(|diagnostic| {
        diagnostic.code == "SYRINX_BROWSER_NODE_IDENTITY_FORBIDDEN"
            && diagnostic.end_byte > diagnostic.start_byte
    }));

    let top_level = r#"<script setup>
const value = await Promise.resolve('nope')
</script><template><p>{{ value }}</p></template>"#;
    let error = compile_syrinx(top_level, options()).expect_err("top-level await must fail");
    assert!(error.diagnostics.iter().any(|diagnostic| {
        diagnostic.code == "SYRINX_UNSUPPORTED_ASYNC_SETUP"
            && diagnostic.end_byte > diagnostic.start_byte
    }));
}

#[test]
fn unsupported_template_semantics_fail_closed_with_named_diagnostics() {
    for (source, code) in [
        (
            r#"<script setup>const styles = {}; const go = () => {}</script><template><button :style="styles">go</button></template>"#,
            "SYRINX_UNSUPPORTED_DYNAMIC_STYLE_MAP",
        ),
        (
            r#"<script setup>const go = () => {}</script><template><button @click.passive.prevent="go">go</button></template>"#,
            "SYRINX_INVALID_PASSIVE_PREVENT_HANDLER",
        ),
        (
            r#"<script setup>const go = () => {}</script><template><button @click.foo="go">go</button></template>"#,
            "SYRINX_UNSUPPORTED_EVENT_MODIFIER",
        ),
        (
            r#"<script setup>const rows = [1]</script><template><i v-for="row in rows">{{ row }}</i></template>"#,
            "SYRINX_UNKEYED_FOR_FORBIDDEN",
        ),
        (
            r#"<script setup>const name = 'title'; const value = 'x'</script><template><p :[name]="value" /></template>"#,
            "SYRINX_UNSUPPORTED_DYNAMIC_PROP_NAME",
        ),
        (
            r#"<script setup>const el = null</script><template><p :ref="el" /></template>"#,
            "SYRINX_TEMPLATE_REF_FORBIDDEN",
        ),
        (
            r#"<script setup></script><template><Widget /></template>"#,
            "SYRINX_UNRESOLVED_COMPONENT",
        ),
    ] {
        let error = compile_syrinx(source, options()).expect_err(code);
        assert!(
            error.diagnostics.iter().any(|diagnostic| {
                diagnostic.code == code && diagnostic.end_byte > diagnostic.start_byte
            }),
            "missing {code}: {:?}",
            error.diagnostics
        );
    }
}

#[test]
fn compiler_runtime_pair_options_fail_before_artifact_emit() {
    let mut mismatch = options();
    mismatch.vue_reactivity_version = "3.5.0".to_owned();
    let error = compile_syrinx(TABLE, mismatch).expect_err("mixed Vue runtime must fail");
    assert!(
        error
            .diagnostics
            .iter()
            .any(|diagnostic| { diagnostic.code == "SYRINX_VUE_VERSION_MISMATCH" })
    );
}
