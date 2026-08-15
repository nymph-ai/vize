#![allow(clippy::disallowed_macros, clippy::disallowed_types)]

use oxc_allocator::Allocator;
use oxc_parser::Parser;
use oxc_span::SourceType;
use sha2::{Digest, Sha256};
use vize_atelier_syrinx::{BindingKind, SyrinxCompileOptions, compile_syrinx};

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

fn options() -> SyrinxCompileOptions {
    SyrinxCompileOptions {
        filename: "TimeTravelTable.vue".to_owned(),
        component_name: Some("TimeTravelTable".to_owned()),
        component_id: 100,
        protocol_schema_sha256: "a".repeat(64),
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
