use std::collections::BTreeMap;

use oxc_allocator::Allocator;
use oxc_parser::Parser;
use oxc_span::SourceType;
use vize_atelier_syrinx::{
    SyrinxCompileOptions, SyrinxComponentLink, SyrinxProgramSource, compile_syrinx,
    compile_syrinx_program, compile_syrinx_rsx,
};

const CHILD: &str = r#"
<script setup lang="ts">
const props = defineProps<{ value: string }>()
</script>

<template>
  <section class="child-cell">
    <span>{{ props.value }}</span>
    <slot name="cell" :child-value="props.value">
      <em>{{ props.value }}</em>
    </slot>
  </section>
</template>
"#;

const ROOT: &str = r#"
<script setup lang="ts">
import ChildCell from './ChildCell.vue'
import { ref } from 'vue'

const frame = ref('frame-1')
const chosen = ref('none')
const choose = (value: string) => { chosen.value = value }
</script>

<template>
  <ChildCell :value="frame" @choose="choose">
    <template #cell="{ childValue }">
      <strong>{{ childValue }} / {{ frame }} / {{ chosen }}</strong>
    </template>
  </ChildCell>
</template>
"#;

const KEYED_ROOT: &str = r#"
<script setup>
import ChildCell from './ChildCell.vue'
import { ref } from 'vue'
const rows = ref([{ id: 'a', value: 'A' }, { id: 'b', value: 'B' }])
</script>
<template>
  <ChildCell v-for="row in rows" :key="row.id" :value="row.value" />
</template>
"#;

const CONDITIONAL_ROOT: &str = r#"
<script setup>
import ChildCell from './ChildCell.vue'
import { ref } from 'vue'
const show = ref(false)
const frame = ref('frame-1')
</script>
<template>
  <ChildCell :value="frame">
    <template #cell="{ childValue }" v-if="show">
      <strong>{{ childValue }}</strong>
    </template>
  </ChildCell>
</template>
"#;

const DEFAULT_CHILD: &str = r#"
<template><article><slot><i>fallback</i></slot></article></template>
"#;

const DEFAULT_ROOT: &str = r#"
<script setup>
import DefaultChild from './DefaultChild.vue'
const label = 'provided'
</script>
<template><DefaultChild><b>{{ label }}</b></DefaultChild></template>
"#;

fn options(filename: &str, component_id: u32) -> SyrinxCompileOptions {
    SyrinxCompileOptions {
        filename: filename.to_owned(),
        component_id,
        protocol_schema_sha256: "a".repeat(64),
        ..Default::default()
    }
}

#[test]
fn keyed_child_inherits_the_v_for_identity_inside_its_stable_item_scope() {
    let mut root_options = options("KeyedRoot.vue", 1);
    root_options.component_links = BTreeMap::from([(
        "ChildCell".to_owned(),
        SyrinxComponentLink {
            definition_id: 2,
            slot_outlets: BTreeMap::from([("cell".to_owned(), 1)]),
        },
    )]);
    let root = compile_syrinx(KEYED_ROOT, root_options).expect("keyed children should compile");
    assert_eq!(root.plan.components[0].list_sites.len(), 1);
    assert_eq!(root.plan.components[0].child_sites.len(), 1);
    assert!(root.guest_module.contains("key:"));
    assert!(root.guest_module.contains("__syrinxValue1.value.id"));
    assert_javascript_module(&root.guest_module);
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
fn static_child_and_scoped_slot_lower_to_owner_mount_composition() {
    let child = compile_syrinx(CHILD, options("ChildCell.vue", 2))
        .expect("child slot outlet should compile");
    let outlet = &child.plan.components[0].slot_outlets[0];
    assert_eq!(outlet.id, 1);
    assert_eq!(outlet.name, "cell");
    assert!(outlet.fallback_template.is_some());
    assert!(child.guest_module.contains("__installSlotOutlet"));
    assert_javascript_module(&child.guest_module);

    let mut root_options = options("RootCell.vue", 1);
    root_options.component_links = BTreeMap::from([(
        "ChildCell".to_owned(),
        SyrinxComponentLink {
            definition_id: 2,
            slot_outlets: BTreeMap::from([("cell".to_owned(), outlet.id)]),
        },
    )]);
    let root =
        compile_syrinx(ROOT, root_options).expect("linked child and scoped slot should compile");
    let component = &root.plan.components[0];
    assert_eq!(component.child_sites.len(), 1);
    assert_eq!(component.child_sites[0].child_definition, 2);
    assert_eq!(component.slot_definitions.len(), 1);
    assert_eq!(component.slot_definitions[0].name, "cell");
    assert_ne!(component.slot_definitions[0].template, 0);
    assert!(root.plan.required_capability_bits & (1 << 3) != 0);
    assert!(root.plan.required_capability_bits & (1 << 4) != 0);
    assert!(root.plan.templates[0].markup.contains("syrinx:v1:child:1"));
    assert!(root.guest_module.contains("__installChild"));
    assert!(root.guest_module.contains("slotDefinitionId: 1"));
    assert!(root.guest_module.contains("__slotProps[\"childValue\"]"));
    assert_javascript_module(&root.guest_module);

    let program = compile_syrinx_program(
        &[
            SyrinxProgramSource {
                tag: "RootCell".to_owned(),
                source: ROOT,
                options: options("RootCell.vue", 1),
            },
            SyrinxProgramSource {
                tag: "ChildCell".to_owned(),
                source: CHILD,
                options: options("ChildCell.vue", 2),
            },
        ],
        1,
    )
    .expect("closed SFC program should link");
    assert_eq!(program.plan.root_component, 1);
    assert_eq!(program.plan.components.len(), 2);
    assert_eq!(
        program.plan.components[0].child_sites[0].child_definition,
        2
    );
    assert!(program.guest_module.contains("const __syrinxDefinition1"));
    assert!(program.guest_module.contains("const __syrinxDefinition2"));
    assert!(!program.guest_module.contains("./ChildCell.vue"));
    assert_javascript_module(&program.guest_module);
}

#[test]
fn v3b_rsx_keeps_components_and_slots_as_rust_tree_structure() {
    let child = compile_syrinx_rsx(CHILD, options("ChildCell.vue", 2))
        .expect("slot outlet should compile to a typed Element field");
    assert!(child.rust_source.contains("pub slot_"));
    assert!(child.rust_source.contains(": Element,"));
    assert!(!child.rust_source.contains("__installSlotOutlet"));

    let mut root_options = options("RootCell.vue", 1);
    root_options.component_links = BTreeMap::from([(
        "ChildCell".to_owned(),
        SyrinxComponentLink {
            definition_id: 2,
            slot_outlets: BTreeMap::from([("cell".to_owned(), 1)]),
        },
    )]);
    let root = compile_syrinx_rsx(ROOT, root_options)
        .expect("linked child and scoped slot should compile to RSX");
    assert!(root.rust_source.contains("ChildCell {"));
    assert!(root.rust_source.contains("strong {"));
    assert!(!root.rust_source.contains("child_definition"));
    assert!(
        root.expression_hooks
            .iter()
            .all(|hook| hook.start_byte < hook.end_byte)
    );
}

#[test]
fn default_and_conditional_slots_lower_without_dynamic_slot_inspection() {
    let default_program = compile_syrinx_program(
        &[
            SyrinxProgramSource {
                tag: "DefaultRoot".to_owned(),
                source: DEFAULT_ROOT,
                options: options("DefaultRoot.vue", 11),
            },
            SyrinxProgramSource {
                tag: "DefaultChild".to_owned(),
                source: DEFAULT_CHILD,
                options: options("DefaultChild.vue", 12),
            },
        ],
        11,
    )
    .expect("implicit default slot should link");
    let default_root = &default_program.plan.components[0];
    assert_eq!(default_root.slot_definitions.len(), 1);
    assert_eq!(default_root.slot_definitions[0].name, "default");
    let default_child = &default_program.plan.components[1];
    assert_eq!(default_child.slot_outlets[0].name, "default");
    assert!(default_child.slot_outlets[0].fallback_template.is_some());
    assert_javascript_module(&default_program.guest_module);

    let mut conditional_options = options("ConditionalRoot.vue", 21);
    conditional_options.component_links = BTreeMap::from([(
        "ChildCell".to_owned(),
        SyrinxComponentLink {
            definition_id: 2,
            slot_outlets: BTreeMap::from([("cell".to_owned(), 1)]),
        },
    )]);
    let conditional = compile_syrinx(CONDITIONAL_ROOT, conditional_options)
        .expect("one statically named v-if slot should compile");
    assert_eq!(conditional.plan.components[0].slot_definitions.len(), 1);
    assert!(
        conditional
            .guest_module
            .contains("__installConditionalSlot")
    );
    assert!(conditional.guest_module.contains("show.value"));
    assert!(
        conditional
            .source_map
            .guest_expression_spans
            .contains_key("slot:1:condition")
    );
    assert_javascript_module(&conditional.guest_module);
}

#[test]
fn grouped_and_repeated_slot_sets_fail_with_named_compile_diagnostics() {
    let cases = [
        (
            r#"<script setup>import ChildCell from './ChildCell.vue'; const rows = [1]</script>
<template><ChildCell><template v-for="row in rows" #cell>{{ row }}</template></ChildCell></template>"#,
            "SYRINX_UNSUPPORTED_SLOT_V_FOR",
        ),
        (
            r#"<script setup>import ChildCell from './ChildCell.vue'; const show = true</script>
<template><ChildCell><template v-if="show" #cell>A</template><template v-else #cell>B</template></ChildCell></template>"#,
            "SYRINX_UNSUPPORTED_SLOT_ELSE_BRANCH",
        ),
    ];
    for (source, code) in cases {
        let mut compile_options = options("UnsupportedSlots.vue", 31);
        compile_options.component_links = BTreeMap::from([(
            "ChildCell".to_owned(),
            SyrinxComponentLink {
                definition_id: 2,
                slot_outlets: BTreeMap::from([("cell".to_owned(), 1)]),
            },
        )]);
        let failure = compile_syrinx(source, compile_options)
            .expect_err("dynamic slot-set topology must fail before artifact emit");
        assert!(
            failure
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == code)
        );
    }
}
