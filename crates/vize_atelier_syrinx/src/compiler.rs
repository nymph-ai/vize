//! Vapor-IR to Syrinx ComponentPlan compilation.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use oxc_allocator::Allocator;
use oxc_ast::ast::{Expression, ObjectPropertyKind, PropertyKey, PropertyKind};
use oxc_ast_visit::{
    Visit,
    walk::{
        walk_arrow_function_expression, walk_function, walk_object_property,
        walk_variable_declarator,
    },
};
use oxc_parser::Parser;
use oxc_semantic::SemanticBuilder;
use oxc_span::{GetSpan, SourceType};
use oxc_syntax::scope::ScopeFlags;
use sha2::{Digest, Sha256};
use vize_atelier_core::{
    ElementNode, ElementType, ExpressionNode, ForNode, IfNode, Namespace, PropNode,
    SimpleExpressionNode, SourceLocation, TemplateChildNode,
    options::{BindingMetadata, TemplateSyntaxMode},
};
use vize_atelier_sfc::{
    SfcDescriptor, SfcParseOptions, StyleCompileOptions, parse_sfc, style::compile_style,
    types::BindingType,
};
use vize_atelier_vapor::{
    BlockIRNode, ComponentKind, CreateComponentIRNode, ForIRNode, IRProp, IfIRNode, NegativeBranch,
    OperationNode, RootIRNode, SlotOutletIRNode, VaporCompilerOptions, VaporTemplateAnnotations,
    compile_vapor_ir_with_template_syntax, generate_vapor_fragment_template,
};
use vize_carton::Bump;

use crate::diagnostic::{SyrinxCompileFailure, SyrinxDiagnostic};
use crate::guest::{GuestEmitOptions, GuestHandler, emit_guest};
use crate::model::{
    AcceptedValueKind, BindingKind, BindingSite, BranchDefinition, ChildSite, ComponentDefinition,
    ComponentPlanV1, HandlerSite, IfSite, KeyedListSite, ProtocolLimits, ProtocolVersion,
    SlotDefinition, SlotOutlet, SourceSpan, Stylesheet, SyrinxArtifacts, SyrinxManifestV1,
    SyrinxSourceMapV1, TemplateDefinition,
};

const FORMAT: &str = "syrinx-component-plan-v1";
const COMPILER_NAME: &str = "vize_atelier_syrinx";
const GUEST_ABI_VERSION: u32 = 1;
const VIZE_UPSTREAM_REVISION: &str = "fd841c9fb20edc6e538d1c951e16a9780ae4e013";
const VUE_REACTIVITY_VERSION: &str = "3.6.0-rc.3";
const GUEST_RUNTIME_MODULE: &str = "@nymphai/syrinx-guest-runtime";
const GUEST_RUNTIME_VERSION: &str = "2.2.0";
const ALL_V1_CAPABILITY_BITS: u64 = (1 << 7) - 1;
const CAPABILITY_LIST_PATCH: u64 = 1 << 0;
const CAPABILITY_NESTED_COMPONENTS: u64 = 1 << 3;
const CAPABILITY_SLOTS: u64 = 1 << 4;
const CAPABILITY_EVENT_FLAGS: u64 = 1 << 6;

/// Link-time identity and static outlet table for one ordinary child SFC.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyrinxComponentLink {
    pub definition_id: u32,
    pub slot_outlets: BTreeMap<String, u32>,
}

/// One statically linked SFC in a renderer-neutral Syrinx program.
#[derive(Debug, Clone)]
pub struct SyrinxProgramSource<'a> {
    /// Template tag used by parent SFCs, for example `ChildCell`.
    pub tag: String,
    pub source: &'a str,
    pub options: SyrinxCompileOptions,
}

/// Explicit inputs that pin a compiler/runtime pair.
#[derive(Debug, Clone)]
pub struct SyrinxCompileOptions {
    pub filename: String,
    pub component_name: Option<String>,
    pub component_id: u32,
    pub protocol: ProtocolVersion,
    pub protocol_schema_sha256: String,
    pub vue_reactivity_version: String,
    pub vize_revision: String,
    pub guest_runtime_module: String,
    pub guest_runtime_version: String,
    pub input_names: BTreeMap<u32, String>,
    pub required_capability_bits: u64,
    pub limits: ProtocolLimits,
    pub template_syntax: TemplateSyntaxMode,
    pub component_links: BTreeMap<String, SyrinxComponentLink>,
}

impl Default for SyrinxCompileOptions {
    fn default() -> Self {
        Self {
            filename: "Component.vue".to_owned(),
            component_name: None,
            component_id: 1,
            protocol: ProtocolVersion {
                major: 1,
                minor: 0,
                patch: 0,
            },
            protocol_schema_sha256: String::new(),
            vue_reactivity_version: VUE_REACTIVITY_VERSION.to_owned(),
            vize_revision: VIZE_UPSTREAM_REVISION.to_owned(),
            guest_runtime_module: GUEST_RUNTIME_MODULE.to_owned(),
            guest_runtime_version: GUEST_RUNTIME_VERSION.to_owned(),
            input_names: BTreeMap::new(),
            required_capability_bits: 0,
            limits: ProtocolLimits::default(),
            template_syntax: TemplateSyntaxMode::Standard,
            component_links: BTreeMap::new(),
        }
    }
}

/// Compile one ordinary Vue SFC into plan, guest, CSS, manifest, and source map.
pub fn compile_syrinx(
    source: &str,
    options: SyrinxCompileOptions,
) -> Result<SyrinxArtifacts, SyrinxCompileFailure> {
    let mut diagnostics = validate_options(source, &options);
    let descriptor = match parse_sfc(
        source,
        SfcParseOptions {
            filename: options.filename.clone().into(),
            source_map: true,
            ..Default::default()
        },
    ) {
        Ok(descriptor) => descriptor,
        Err(error) => {
            diagnostics.push(sfc_diagnostic(source, &options.filename, error));
            return Err(SyrinxCompileFailure { diagnostics });
        }
    };

    let Some(template) = descriptor.template.as_ref() else {
        diagnostics.push(file_diagnostic(
            source,
            &options.filename,
            "SYRINX_MISSING_TEMPLATE",
            "The Syrinx backend requires an inline <template> block.",
            0,
            source.len(),
        ));
        return Err(SyrinxCompileFailure { diagnostics });
    };
    if template.src.is_some() || template.lang.is_some() {
        diagnostics.push(file_diagnostic(
            source,
            &options.filename,
            "SYRINX_UNSUPPORTED_EXTERNAL_TEMPLATE",
            "External or preprocessed templates must be resolved before the Syrinx backend.",
            template.loc.tag_start,
            template.loc.tag_end,
        ));
    }
    diagnostics.extend(reject_browser_identities(&descriptor, &options.filename));
    if !diagnostics.is_empty() {
        return Err(SyrinxCompileFailure { diagnostics });
    }

    let script_content = descriptor
        .script_setup
        .as_ref()
        .map(|block| block.content.as_ref())
        .unwrap_or("");
    let mut script_context = vize_atelier_sfc::script::ScriptCompileContext::new(script_content);
    script_context.analyze();
    let binding_metadata: BindingMetadata = script_context.bindings.clone();
    let input_names = resolve_input_names(&options.input_names, &binding_metadata);

    let allocator = Bump::new();
    let lowered = compile_vapor_ir_with_template_syntax(
        &allocator,
        template.content.as_ref(),
        VaporCompilerOptions {
            prefix_identifiers: true,
            binding_metadata: Some(binding_metadata),
            inline: true,
            custom_renderer: true,
            ..Default::default()
        },
        options.template_syntax,
    );
    for error in &lowered.parser_diagnostics {
        diagnostics.push(template_diagnostic(
            source,
            &options.filename,
            &template.loc,
            "SYRINX_TEMPLATE_PARSE_ERROR",
            error.message.as_str(),
            error.loc.as_ref(),
        ));
    }
    for message in &lowered.transform_diagnostics {
        diagnostics.push(file_diagnostic(
            source,
            &options.filename,
            "SYRINX_TEMPLATE_TRANSFORM_ERROR",
            message,
            template.loc.start,
            template.loc.end,
        ));
    }
    let Some(ir) = lowered.ir.as_ref() else {
        return Err(SyrinxCompileFailure { diagnostics });
    };
    diagnostics.extend(preflight_template(
        source,
        &options.filename,
        &template.loc,
        &lowered.root.children,
        ir,
        &options.component_links,
    ));
    if !diagnostics.is_empty() {
        return Err(SyrinxCompileFailure { diagnostics });
    }

    let component_name = options
        .component_name
        .clone()
        .unwrap_or_else(|| component_name_from_filename(&options.filename));
    let scope_token = descriptor
        .styles
        .iter()
        .any(|style| style.scoped)
        .then(|| format!("data-v-syrinx-{}", &sha256(source.as_bytes())[..8]));
    let mut spans = SpanRegistry::new(source, &options.filename, &template.loc);
    let component_span = spans.block_span(&template.loc);
    let mut state = CompileState::new(
        ir,
        &mut spans,
        scope_token.clone(),
        options.component_links.clone(),
    );
    let root_template = state.allocate_template(&lowered.root.children, component_span);
    debug_assert_eq!(root_template, 1);
    let install_body =
        state.compile_block(&ir.block, &lowered.root.children, root_template, "__scope");
    state.materialize_templates();

    let mut required_capability_bits = options.required_capability_bits;
    if !state.list_sites.is_empty() {
        required_capability_bits |= CAPABILITY_LIST_PATCH;
    }
    if !state.handlers.is_empty() {
        required_capability_bits |= CAPABILITY_EVENT_FLAGS;
    }
    if !state.child_sites.is_empty() {
        required_capability_bits |= CAPABILITY_NESTED_COMPONENTS;
    }
    if !state.slot_definitions.is_empty() || !state.slot_outlets.is_empty() {
        required_capability_bits |= CAPABILITY_SLOTS;
    }
    let guest = emit_guest(
        &descriptor,
        &GuestEmitOptions {
            component_id: options.component_id,
            component_name: &component_name,
            input_names: &input_names,
            required_capability_bits,
            runtime_module: &options.guest_runtime_module,
            install_body: &install_body,
            handlers: &state.guest_handlers,
            filename: &options.filename,
        },
    )
    .map_err(|diagnostics| SyrinxCompileFailure { diagnostics })?;
    let guest_hash = sha256(guest.code.as_bytes());

    let templates = std::mem::take(&mut state.templates);
    let bindings = std::mem::take(&mut state.bindings);
    let handlers = std::mem::take(&mut state.handlers);
    let if_sites = std::mem::take(&mut state.if_sites);
    let list_sites = std::mem::take(&mut state.list_sites);
    let child_sites = std::mem::take(&mut state.child_sites);
    let slot_definitions = std::mem::take(&mut state.slot_definitions);
    let slot_outlets = std::mem::take(&mut state.slot_outlets);
    let guest_expression_spans = std::mem::take(&mut state.guest_expression_spans);
    drop(state);

    let (css, stylesheets) = compile_styles(
        source,
        &options.filename,
        &descriptor,
        options.component_id,
        scope_token.clone(),
        &mut spans,
    )?;
    let source_spans = spans.into_spans();
    let component = ComponentDefinition {
        id: options.component_id,
        name: component_name.clone(),
        template: root_template,
        bindings,
        handlers,
        if_sites,
        list_sites,
        child_sites,
        slot_definitions,
        slot_outlets,
        capability_targets: Vec::new(),
        lifecycle_bits: 0,
        source_span: component_span,
    };
    let plan = ComponentPlanV1 {
        format: FORMAT.to_owned(),
        protocol: options.protocol,
        compiler_name: COMPILER_NAME.to_owned(),
        compiler_version: env!("CARGO_PKG_VERSION").to_owned(),
        schema_sha256: options.protocol_schema_sha256.clone(),
        guest_module_sha256: guest_hash,
        required_capability_bits,
        limits: options.limits,
        root_component: options.component_id,
        source_spans: source_spans.clone(),
        templates,
        components: vec![component],
        stylesheets,
    };
    let plan_json = canonical_json(&plan);
    let source_map = SyrinxSourceMapV1 {
        format: "syrinx-source-map-v1".to_owned(),
        source: options.filename.clone(),
        source_sha256: sha256(source.as_bytes()),
        spans: source_spans,
        binding_spans: plan.components[0]
            .bindings
            .iter()
            .map(|site| (site.id, site.source_span))
            .collect(),
        handler_spans: plan.components[0]
            .handlers
            .iter()
            .map(|site| (site.id, site.source_span))
            .collect(),
        if_spans: plan.components[0]
            .if_sites
            .iter()
            .map(|site| (site.id, site.source_span))
            .collect(),
        list_spans: plan.components[0]
            .list_sites
            .iter()
            .map(|site| (site.id, site.source_span))
            .collect(),
        guest_expression_spans,
    };
    let source_map_json = canonical_json(&source_map);
    let mut artifact_sha256 = BTreeMap::new();
    artifact_sha256.insert(
        "component.plan.json".to_owned(),
        sha256(plan_json.as_bytes()),
    );
    artifact_sha256.insert("guest.mjs".to_owned(), sha256(guest.code.as_bytes()));
    artifact_sha256.insert("style.css".to_owned(), sha256(css.as_bytes()));
    artifact_sha256.insert(
        "source-map.json".to_owned(),
        sha256(source_map_json.as_bytes()),
    );
    let manifest = SyrinxManifestV1 {
        format: "syrinx-manifest-v1".to_owned(),
        component_id: options.component_id,
        component_name,
        protocol: options.protocol,
        protocol_schema_sha256: options.protocol_schema_sha256,
        compiler_name: COMPILER_NAME.to_owned(),
        compiler_version: env!("CARGO_PKG_VERSION").to_owned(),
        vize_version: env!("CARGO_PKG_VERSION").to_owned(),
        vize_revision: options.vize_revision,
        vue_reactivity_version: options.vue_reactivity_version,
        guest_runtime_module: options.guest_runtime_module,
        guest_runtime_version: options.guest_runtime_version,
        guest_abi_version: GUEST_ABI_VERSION,
        required_capability_bits,
        limits: options.limits,
        source_sha256: sha256(source.as_bytes()),
        input_names,
        artifact_sha256,
    };
    let manifest_json = canonical_json(&manifest);

    Ok(SyrinxArtifacts {
        plan,
        manifest,
        source_map,
        plan_json,
        guest_module: guest.code,
        css,
        manifest_json,
        source_map_json,
    })
}

/// Compile a closed set of statically linked SFCs into one host-verifiable
/// component bundle and one DOM-less guest module.
pub fn compile_syrinx_program(
    sources: &[SyrinxProgramSource<'_>],
    root_component: u32,
) -> Result<SyrinxArtifacts, SyrinxCompileFailure> {
    let Some(root_source) = sources
        .iter()
        .find(|source| source.options.component_id == root_component)
    else {
        return Err(SyrinxCompileFailure {
            diagnostics: vec![file_diagnostic(
                "",
                "<syrinx-program>",
                "SYRINX_MISSING_ROOT_COMPONENT",
                "The linked Syrinx program does not contain its requested root component.",
                0,
                0,
            )],
        });
    };
    let mut ids = BTreeSet::new();
    let mut tags = BTreeSet::new();
    for source in sources {
        if !ids.insert(source.options.component_id) {
            return Err(SyrinxCompileFailure {
                diagnostics: vec![file_diagnostic(
                    source.source,
                    &source.options.filename,
                    "SYRINX_DUPLICATE_COMPONENT_ID",
                    "Every linked SFC requires a unique component definition ID.",
                    0,
                    source.source.len(),
                )],
            });
        }
        if source.tag.is_empty() || !tags.insert(source.tag.clone()) {
            return Err(SyrinxCompileFailure {
                diagnostics: vec![file_diagnostic(
                    source.source,
                    &source.options.filename,
                    "SYRINX_DUPLICATE_COMPONENT_TAG",
                    "Every linked SFC requires a unique non-empty template tag.",
                    0,
                    source.source.len(),
                )],
            });
        }
    }

    let mut links = BTreeMap::new();
    for source in sources {
        links.insert(
            source.tag.clone(),
            SyrinxComponentLink {
                definition_id: source.options.component_id,
                slot_outlets: discover_slot_outlets(source.source, &source.options)?,
            },
        );
    }
    let mut ordered = sources.iter().collect::<Vec<_>>();
    ordered.sort_by_key(|source| source.options.component_id);
    let mut units = Vec::with_capacity(ordered.len());
    for source in ordered {
        let mut options = source.options.clone();
        options.component_links = links.clone();
        units.push((source, compile_syrinx(source.source, options)?));
    }
    let root_index = units
        .iter()
        .position(|(source, _)| source.options.component_id == root_component)
        .expect("root membership was checked above");
    let root_artifact = &units[root_index].1;
    for (_, artifact) in &units {
        let manifest = &artifact.manifest;
        let root = &root_artifact.manifest;
        if manifest.protocol != root.protocol
            || manifest.protocol_schema_sha256 != root.protocol_schema_sha256
            || manifest.vue_reactivity_version != root.vue_reactivity_version
            || manifest.vize_revision != root.vize_revision
            || manifest.guest_runtime_module != root.guest_runtime_module
            || manifest.guest_runtime_version != root.guest_runtime_version
            || manifest.guest_abi_version != root.guest_abi_version
            || manifest.limits != root.limits
        {
            return Err(SyrinxCompileFailure {
                diagnostics: vec![file_diagnostic(
                    root_source.source,
                    &root_source.options.filename,
                    "SYRINX_PROGRAM_PAIRING_MISMATCH",
                    "Every SFC in one program must pin the same protocol, runtime, Vize revision, and limits.",
                    0,
                    root_source.source.len(),
                )],
            });
        }
    }

    let guest_module = combine_guest_modules(&units, root_component);
    let guest_hash = sha256(guest_module.as_bytes());
    let mut source_spans = Vec::new();
    let mut templates = Vec::new();
    let mut components = Vec::new();
    let mut stylesheets = Vec::new();
    let mut css = String::new();
    let mut required_capability_bits = 0u64;
    let mut span_offset = 0u32;
    let mut template_offset = 0u32;
    let mut stylesheet_offset = 0u32;
    let mut root_source_map = None;
    for (source, artifact) in &units {
        let mut plan = artifact.plan.clone();
        remap_plan_ids(&mut plan, span_offset, template_offset, stylesheet_offset);
        if source.options.component_id == root_component {
            let mut map = artifact.source_map.clone();
            remap_source_map_spans(&mut map, span_offset);
            root_source_map = Some(map);
        }
        required_capability_bits |= plan.required_capability_bits;
        source_spans.extend(plan.source_spans);
        templates.extend(plan.templates);
        components.extend(plan.components);
        stylesheets.extend(plan.stylesheets);
        if !artifact.css.is_empty() {
            if !css.is_empty() && !css.ends_with('\n') {
                css.push('\n');
            }
            css.push_str(&artifact.css);
        }
        span_offset = source_spans.len() as u32;
        template_offset = templates.len() as u32;
        stylesheet_offset = stylesheets.len() as u32;
    }
    components.sort_by_key(|component| component.id);
    let root_manifest = &root_artifact.manifest;
    let plan = ComponentPlanV1 {
        format: FORMAT.to_owned(),
        protocol: root_manifest.protocol,
        compiler_name: COMPILER_NAME.to_owned(),
        compiler_version: env!("CARGO_PKG_VERSION").to_owned(),
        schema_sha256: root_manifest.protocol_schema_sha256.clone(),
        guest_module_sha256: guest_hash,
        required_capability_bits,
        limits: root_manifest.limits,
        root_component,
        source_spans: source_spans.clone(),
        templates,
        components,
        stylesheets,
    };
    let plan_json = canonical_json(&plan);
    let mut combined_source = String::new();
    for (source, _) in &units {
        writeln!(combined_source, "{}\0{}", source.tag, source.source).unwrap();
    }
    let program_source_hash = sha256(combined_source.as_bytes());
    let mut source_map = root_source_map.expect("root source map exists");
    source_map.source = root_source.options.filename.clone();
    source_map.source_sha256 = program_source_hash.clone();
    source_map.spans = source_spans;
    let source_map_json = canonical_json(&source_map);
    let mut artifact_sha256 = BTreeMap::new();
    artifact_sha256.insert(
        "component.plan.json".to_owned(),
        sha256(plan_json.as_bytes()),
    );
    artifact_sha256.insert("guest.mjs".to_owned(), sha256(guest_module.as_bytes()));
    artifact_sha256.insert("style.css".to_owned(), sha256(css.as_bytes()));
    artifact_sha256.insert(
        "source-map.json".to_owned(),
        sha256(source_map_json.as_bytes()),
    );
    let root_definition = plan
        .components
        .iter()
        .find(|component| component.id == root_component)
        .expect("root component remains linked");
    let manifest = SyrinxManifestV1 {
        format: "syrinx-manifest-v1".to_owned(),
        component_id: root_component,
        component_name: root_definition.name.clone(),
        protocol: root_manifest.protocol,
        protocol_schema_sha256: root_manifest.protocol_schema_sha256.clone(),
        compiler_name: COMPILER_NAME.to_owned(),
        compiler_version: env!("CARGO_PKG_VERSION").to_owned(),
        vize_version: env!("CARGO_PKG_VERSION").to_owned(),
        vize_revision: root_manifest.vize_revision.clone(),
        vue_reactivity_version: root_manifest.vue_reactivity_version.clone(),
        guest_runtime_module: root_manifest.guest_runtime_module.clone(),
        guest_runtime_version: root_manifest.guest_runtime_version.clone(),
        guest_abi_version: root_manifest.guest_abi_version,
        required_capability_bits,
        limits: root_manifest.limits,
        source_sha256: program_source_hash,
        input_names: root_manifest.input_names.clone(),
        artifact_sha256,
    };
    let manifest_json = canonical_json(&manifest);
    Ok(SyrinxArtifacts {
        plan,
        manifest,
        source_map,
        plan_json,
        guest_module,
        css,
        manifest_json,
        source_map_json,
    })
}

fn discover_slot_outlets(
    source: &str,
    options: &SyrinxCompileOptions,
) -> Result<BTreeMap<String, u32>, SyrinxCompileFailure> {
    let descriptor = parse_sfc(
        source,
        SfcParseOptions {
            filename: options.filename.clone().into(),
            source_map: true,
            ..Default::default()
        },
    )
    .map_err(|error| SyrinxCompileFailure {
        diagnostics: vec![sfc_diagnostic(source, &options.filename, error)],
    })?;
    let Some(template) = descriptor.template.as_ref() else {
        return Err(SyrinxCompileFailure {
            diagnostics: vec![file_diagnostic(
                source,
                &options.filename,
                "SYRINX_MISSING_TEMPLATE",
                "The Syrinx backend requires an inline <template> block.",
                0,
                source.len(),
            )],
        });
    };
    let script_content = descriptor
        .script_setup
        .as_ref()
        .map(|block| block.content.as_ref())
        .unwrap_or("");
    let mut script_context = vize_atelier_sfc::script::ScriptCompileContext::new(script_content);
    script_context.analyze();
    let allocator = Bump::new();
    let lowered = compile_vapor_ir_with_template_syntax(
        &allocator,
        template.content.as_ref(),
        VaporCompilerOptions {
            prefix_identifiers: true,
            binding_metadata: Some(script_context.bindings.clone()),
            inline: true,
            custom_renderer: true,
            ..Default::default()
        },
        options.template_syntax,
    );
    let Some(ir) = lowered.ir.as_ref() else {
        return Err(SyrinxCompileFailure {
            diagnostics: vec![file_diagnostic(
                source,
                &options.filename,
                "SYRINX_TEMPLATE_TRANSFORM_ERROR",
                "Vize could not produce Vapor IR while discovering static slot outlets.",
                template.loc.start,
                template.loc.end,
            )],
        });
    };
    let mut outlets = BTreeMap::new();
    let mut next = 1u32;
    collect_slot_outlets(&ir.block, &mut next, &mut outlets).map_err(|name| {
        SyrinxCompileFailure {
            diagnostics: vec![file_diagnostic(
                source,
                &options.filename,
                "SYRINX_DUPLICATE_SLOT_OUTLET_NAME",
                &format!(
                    "Component '{}' declares more than one static '{name}' outlet; protocol v1 requires one physical route per slot name.",
                    options
                        .component_name
                        .clone()
                        .unwrap_or_else(|| component_name_from_filename(&options.filename))
                ),
                template.loc.start,
                template.loc.end,
            )],
        }
    })?;
    Ok(outlets)
}

fn collect_slot_outlets(
    block: &BlockIRNode<'_>,
    next: &mut u32,
    outlets: &mut BTreeMap<String, u32>,
) -> Result<(), String> {
    for operation in block.operation.iter().chain(
        block
            .effect
            .iter()
            .flat_map(|effect| effect.operations.iter()),
    ) {
        match operation {
            OperationNode::SlotOutlet(outlet) => {
                let id = *next;
                *next += 1;
                if outlet.name.is_static {
                    let name = outlet.name.content.to_string();
                    if outlets.insert(name.clone(), id).is_some() {
                        return Err(name);
                    }
                }
                if let Some(fallback) = outlet.fallback.as_ref() {
                    collect_slot_outlets(fallback, next, outlets)?;
                }
            }
            OperationNode::CreateComponent(component) => {
                for slot in component.slots.iter() {
                    collect_slot_outlets(&slot.block, next, outlets)?;
                }
            }
            OperationNode::If(node) => {
                collect_slot_outlets(&node.positive, next, outlets)?;
                collect_negative_slot_outlets(node.negative.as_ref(), next, outlets)?;
            }
            OperationNode::For(node) => collect_slot_outlets(&node.render, next, outlets)?,
            _ => {}
        }
    }
    Ok(())
}

fn collect_negative_slot_outlets(
    negative: Option<&NegativeBranch<'_>>,
    next: &mut u32,
    outlets: &mut BTreeMap<String, u32>,
) -> Result<(), String> {
    match negative {
        Some(NegativeBranch::Block(block)) => collect_slot_outlets(block, next, outlets),
        Some(NegativeBranch::If(node)) => {
            collect_slot_outlets(&node.positive, next, outlets)?;
            collect_negative_slot_outlets(node.negative.as_ref(), next, outlets)
        }
        None => Ok(()),
    }
}

fn combine_guest_modules(
    units: &[(&SyrinxProgramSource<'_>, SyrinxArtifacts)],
    root_component: u32,
) -> String {
    let mut imports = Vec::new();
    let mut seen_imports = BTreeSet::new();
    let mut bodies = Vec::new();
    for (source, artifact) in units {
        let mut body = String::new();
        for line in artifact.guest_module.lines() {
            if line.starts_with("export default ") {
                break;
            }
            if line.starts_with("import ") {
                if line.contains(".vue'") || line.contains(".vue\"") {
                    continue;
                }
                if line.contains("createCompiledSetup as __createCompiledSetup") {
                    continue;
                }
                if seen_imports.insert(line.to_owned()) {
                    imports.push(line.to_owned());
                }
                continue;
            }
            writeln!(body, "  {line}").unwrap();
        }
        let id = source.options.component_id;
        bodies.push(format!(
            "const __syrinxDefinition{id} = (() => {{\n{body}  return __syrinxDefinition;\n}})();\n"
        ));
    }
    let mut output = String::new();
    writeln!(
        output,
        "import {{ createCompiledSetup as __createCompiledSetup, displayValue as __displayValue, installBranch as __installBranch, installChild as __installChild, installConditionalSlot as __installConditionalSlot, installKeyedList as __installKeyedList, installSlotOutlet as __installSlotOutlet, invokeHandler as __invokeHandler, styleValue as __styleValue }} from {};",
        json(units[0].1.manifest.guest_runtime_module.as_str())
    )
    .unwrap();
    for import in imports {
        writeln!(output, "{import}").unwrap();
    }
    output.push('\n');
    for body in bodies {
        output.push_str(&body);
        output.push('\n');
    }
    let definitions = units
        .iter()
        .map(|(source, _)| format!("__syrinxDefinition{}", source.options.component_id))
        .collect::<Vec<_>>()
        .join(", ");
    writeln!(output, "export default __syrinxDefinition{root_component};").unwrap();
    writeln!(
        output,
        "export const definitions = Object.freeze([{definitions}]);"
    )
    .unwrap();
    output.push_str("export const syrinxGuestAbi = Object.freeze({ version: 1, dom: false });\n");
    output
}

fn remap_plan_ids(
    plan: &mut ComponentPlanV1,
    span_offset: u32,
    template_offset: u32,
    stylesheet_offset: u32,
) {
    for span in &mut plan.source_spans {
        span.id += span_offset;
    }
    for template in &mut plan.templates {
        template.id += template_offset;
        template.source_span += span_offset;
    }
    for component in &mut plan.components {
        component.template += template_offset;
        component.source_span += span_offset;
        for site in &mut component.bindings {
            site.template += template_offset;
            site.source_span += span_offset;
        }
        for site in &mut component.handlers {
            site.template += template_offset;
            site.source_span += span_offset;
        }
        for site in &mut component.if_sites {
            site.template += template_offset;
            site.source_span += span_offset;
            for branch in &mut site.branches {
                branch.template += template_offset;
            }
        }
        for site in &mut component.list_sites {
            site.template += template_offset;
            site.item_template += template_offset;
            site.source_span += span_offset;
        }
        for site in &mut component.child_sites {
            site.template += template_offset;
            site.source_span += span_offset;
        }
        for site in &mut component.slot_definitions {
            site.template += template_offset;
            site.source_span += span_offset;
        }
        for site in &mut component.slot_outlets {
            site.template += template_offset;
            site.fallback_template = site
                .fallback_template
                .map(|template| template + template_offset);
            site.source_span += span_offset;
        }
        for site in &mut component.capability_targets {
            site.template += template_offset;
            site.source_span += span_offset;
        }
    }
    for stylesheet in &mut plan.stylesheets {
        stylesheet.id += stylesheet_offset;
        stylesheet.source_span += span_offset;
    }
}

fn remap_source_map_spans(source_map: &mut SyrinxSourceMapV1, span_offset: u32) {
    for span in &mut source_map.spans {
        span.id += span_offset;
    }
    for span in source_map
        .binding_spans
        .values_mut()
        .chain(source_map.handler_spans.values_mut())
        .chain(source_map.if_spans.values_mut())
        .chain(source_map.list_spans.values_mut())
        .chain(source_map.guest_expression_spans.values_mut())
    {
        *span += span_offset;
    }
}

struct TemplateJob<'a> {
    id: u32,
    children: &'a [TemplateChildNode<'a>],
    source_span: u32,
}

struct CompileState<'a, 's> {
    ir: &'a RootIRNode<'a>,
    spans: &'s mut SpanRegistry<'a>,
    annotations: VaporTemplateAnnotations,
    jobs: Vec<TemplateJob<'a>>,
    templates: Vec<TemplateDefinition>,
    bindings: Vec<BindingSite>,
    handlers: Vec<HandlerSite>,
    guest_handlers: Vec<GuestHandler>,
    if_sites: Vec<IfSite>,
    list_sites: Vec<KeyedListSite>,
    child_sites: Vec<ChildSite>,
    slot_definitions: Vec<SlotDefinition>,
    slot_outlets: Vec<SlotOutlet>,
    guest_expression_spans: BTreeMap<String, u32>,
    component_links: BTreeMap<String, SyrinxComponentLink>,
    next_template: u32,
    next_binding: u32,
    next_handler: u32,
    next_if: u32,
    next_list: u32,
    next_child: u32,
    next_slot_definition: u32,
    next_slot_outlet: u32,
    alias_scopes: Vec<BTreeMap<String, String>>,
}

impl<'a, 's> CompileState<'a, 's> {
    fn new(
        ir: &'a RootIRNode<'a>,
        spans: &'s mut SpanRegistry<'a>,
        scope_token: Option<String>,
        component_links: BTreeMap<String, SyrinxComponentLink>,
    ) -> Self {
        let mut annotations = VaporTemplateAnnotations::default();
        if let Some(token) = scope_token.as_ref() {
            annotations.set_scope_attribute(token.as_str());
        }
        Self {
            ir,
            spans,
            annotations,
            jobs: Vec::new(),
            templates: Vec::new(),
            bindings: Vec::new(),
            handlers: Vec::new(),
            guest_handlers: Vec::new(),
            if_sites: Vec::new(),
            list_sites: Vec::new(),
            child_sites: Vec::new(),
            slot_definitions: Vec::new(),
            slot_outlets: Vec::new(),
            guest_expression_spans: BTreeMap::new(),
            component_links,
            next_template: 1,
            next_binding: 1,
            next_handler: 1,
            next_if: 1,
            next_list: 1,
            next_child: 1,
            next_slot_definition: 1,
            next_slot_outlet: 1,
            alias_scopes: Vec::new(),
        }
    }

    fn allocate_template(
        &mut self,
        children: &'a [TemplateChildNode<'a>],
        source_span: u32,
    ) -> u32 {
        let id = self.next_template;
        self.next_template += 1;
        self.jobs.push(TemplateJob {
            id,
            children,
            source_span,
        });
        id
    }

    fn materialize_templates(&mut self) {
        self.templates
            .extend(self.jobs.iter().map(|job| TemplateDefinition {
                id: job.id,
                markup:
                    generate_vapor_fragment_template(job.children, &self.annotations).to_string(),
                source_span: job.source_span,
            }));
        self.templates.sort_by_key(|template| template.id);
    }

    fn compile_block(
        &mut self,
        block: &'a BlockIRNode<'a>,
        children: &'a [TemplateChildNode<'a>],
        template_id: u32,
        scope: &str,
    ) -> String {
        let mut body = String::new();
        for operation in block.operation.iter() {
            self.compile_operation(operation, children, template_id, scope, &mut body);
        }
        for effect in block.effect.iter() {
            for operation in effect.operations.iter() {
                self.compile_operation(operation, children, template_id, scope, &mut body);
            }
        }
        body
    }

    fn compile_operation(
        &mut self,
        operation: &'a OperationNode<'a>,
        children: &'a [TemplateChildNode<'a>],
        template_id: u32,
        scope: &str,
        body: &mut String,
    ) {
        match operation {
            OperationNode::SetProp(node) => {
                let key = node.prop.key.content.as_str();
                if key == "style" {
                    let expression =
                        self.rewrite_aliases(&expression_values(&node.prop.values, false));
                    let properties = split_style_object(&expression)
                        .expect("preflight accepts only statically named style object literals");
                    let location = self.element_location(node.element).clone();
                    let span = node
                        .prop
                        .values
                        .first()
                        .map(|value| self.spans.location_span(&value.loc))
                        .unwrap_or_else(|| self.spans.location_span(&node.prop.key.loc));
                    for property in properties {
                        let id = self.next_binding;
                        self.next_binding += 1;
                        let marker = marker("binding", id);
                        self.annotations.add_element_attribute(
                            &location,
                            format!("data-syrinx-binding-{id}"),
                            Some(marker.as_str()),
                        );
                        let read_expression = format!(
                            "__styleValue({}, ({}))",
                            serde_json::to_string(&property.name)
                                .expect("style property names are JSON serializable"),
                            property.expression
                        );
                        self.bindings.push(BindingSite {
                            id,
                            template: template_id,
                            marker,
                            name: Some(property.name),
                            kind: BindingKind::Style,
                            accepted_value: AcceptedValueKind::Text,
                            source_span: span,
                        });
                        self.guest_expression_spans
                            .insert(format!("binding:{id}:read"), span);
                        writeln!(
                            body,
                            "{scope}.bind({id}, () => ({read_expression}), {{ jobId: {id} }});"
                        )
                        .unwrap();
                    }
                    return;
                }
                let id = self.next_binding;
                self.next_binding += 1;
                let marker = marker("binding", id);
                let location = self.element_location(node.element).clone();
                self.annotations.add_element_attribute(
                    &location,
                    format!("data-syrinx-binding-{id}"),
                    Some(marker.as_str()),
                );
                let (kind, name, accepted) = if key == "class" {
                    (BindingKind::Class, None, AcceptedValueKind::Any)
                } else if node.prop_modifier || is_property(node.tag.as_str(), key) {
                    (
                        BindingKind::Property,
                        Some(key.to_owned()),
                        accepted_for_property(key),
                    )
                } else {
                    (
                        BindingKind::Attribute,
                        Some(key.to_owned()),
                        AcceptedValueKind::Any,
                    )
                };
                let expression = self.rewrite_aliases(&expression_values(
                    &node.prop.values,
                    kind == BindingKind::Class,
                ));
                let span = node
                    .prop
                    .values
                    .first()
                    .map(|value| self.spans.location_span(&value.loc))
                    .unwrap_or_else(|| self.spans.location_span(&node.prop.key.loc));
                self.bindings.push(BindingSite {
                    id,
                    template: template_id,
                    marker,
                    name,
                    kind,
                    accepted_value: accepted,
                    source_span: span,
                });
                self.guest_expression_spans
                    .insert(format!("binding:{id}:read"), span);
                writeln!(
                    body,
                    "{scope}.bind({id}, () => ({expression}), {{ jobId: {id} }});"
                )
                .unwrap();
            }
            OperationNode::SetText(node) => {
                let id = self.next_binding;
                self.next_binding += 1;
                let marker = marker("binding", id);
                let dynamic_location = node
                    .values
                    .iter()
                    .find(|value| !value.is_static)
                    .map(|value| &value.loc);
                let element_location = self.element_location(node.element).clone();
                if let Some(location) = dynamic_location
                    && contains_interpolation(children, location)
                {
                    self.annotations.add_text_anchor(location, marker.as_str());
                } else {
                    self.annotations.add_element_attribute(
                        &element_location,
                        format!("data-syrinx-binding-{id}"),
                        Some(marker.as_str()),
                    );
                }
                let expression = self.rewrite_aliases(&text_expression(&node.values));
                let span = dynamic_location
                    .or_else(|| node.values.first().map(|value| &value.loc))
                    .map(|location| self.spans.location_span(location))
                    .unwrap_or_else(|| self.spans.location_span(&element_location));
                self.bindings.push(BindingSite {
                    id,
                    template: template_id,
                    marker,
                    name: None,
                    kind: BindingKind::Text,
                    accepted_value: AcceptedValueKind::Text,
                    source_span: span,
                });
                self.guest_expression_spans
                    .insert(format!("binding:{id}:read"), span);
                writeln!(
                    body,
                    "{scope}.bind({id}, () => ({expression}), {{ jobId: {id} }});"
                )
                .unwrap();
            }
            OperationNode::SetEvent(node) => {
                let id = self.next_handler;
                self.next_handler += 1;
                let marker = marker("handler", id);
                let element_location = self.element_location(node.element).clone();
                self.annotations.add_element_attribute(
                    &element_location,
                    format!("data-syrinx-handler-{id}"),
                    Some(marker.as_str()),
                );
                let bits = modifier_bits(&node.modifiers);
                let expression = node
                    .value
                    .as_ref()
                    .map(|value| value.content.to_string())
                    .unwrap_or_else(|| "() => undefined".to_owned());
                let expression = self.rewrite_aliases(&expression);
                let span = node
                    .value
                    .as_ref()
                    .map(|value| self.spans.location_span(&value.loc))
                    .unwrap_or_else(|| self.spans.location_span(&node.key.loc));
                self.handlers.push(HandlerSite {
                    id,
                    template: template_id,
                    marker,
                    event_name: node.key.content.to_string(),
                    modifier_bits: bits,
                    source_span: span,
                });
                self.guest_expression_spans
                    .insert(format!("handler:{id}:invoke"), span);
                self.guest_handlers.push(GuestHandler {
                    id,
                    modifiers: node
                        .modifiers
                        .non_keys
                        .iter()
                        .map(ToString::to_string)
                        .collect(),
                    keys: node
                        .modifiers
                        .keys
                        .iter()
                        .map(ToString::to_string)
                        .collect(),
                    once: node.modifiers.options.once,
                    capture: node.modifiers.options.capture,
                    passive: node.modifiers.options.passive,
                });
                writeln!(
                    body,
                    "__compiled.on({scope}, {id}, __event => __invokeHandler(({expression}), __event));"
                )
                .unwrap();
            }
            OperationNode::If(node) => {
                self.compile_if(node, children, template_id, scope, body);
            }
            OperationNode::For(node) => {
                self.compile_for(node, children, template_id, scope, body);
            }
            OperationNode::CreateComponent(node) => {
                self.compile_component(node, children, template_id, scope, body);
            }
            OperationNode::SlotOutlet(node) => {
                self.compile_slot_outlet(node, children, template_id, scope, body);
            }
            OperationNode::SetDynamicProps(_)
            | OperationNode::SetHtml(_)
            | OperationNode::SetTemplateRef(_)
            | OperationNode::Directive(_) => {
                unreachable!("preflight rejects unsupported operations")
            }
            OperationNode::InsertNode(_)
            | OperationNode::PrependNode(_)
            | OperationNode::GetTextChild(_)
            | OperationNode::ChildRef(_)
            | OperationNode::NextRef(_) => {}
        }
    }

    fn compile_if(
        &mut self,
        node: &'a IfIRNode<'a>,
        children: &'a [TemplateChildNode<'a>],
        template_id: u32,
        scope: &str,
        body: &mut String,
    ) {
        let id = self.next_if;
        self.next_if += 1;
        let marker = marker("if", id);
        let location = self
            .ir
            .control_source_map
            .get(&node.id)
            .expect("preflight pins conditional source location");
        self.annotations
            .add_control_anchor(location, marker.as_str());
        let ast = find_if(children, location).expect("preflight pins conditional AST");
        let span = self.spans.location_span(location);
        let mut branches = Vec::new();
        let mut installs = String::new();
        let mut conditions = Vec::new();
        self.compile_if_branches(
            &node.positive,
            node.negative.as_ref(),
            ast,
            id,
            scope,
            &mut branches,
            &mut installs,
            &mut conditions,
        );
        body.push_str(&installs);
        writeln!(
            body,
            "__installBranch({scope}, {id}, [{}]);",
            conditions
                .iter()
                .map(|condition| condition
                    .as_ref()
                    .map(|condition| format!("() => ({condition})"))
                    .unwrap_or_else(|| "null".to_owned()))
                .collect::<Vec<_>>()
                .join(", ")
        )
        .unwrap();
        self.if_sites.push(IfSite {
            id,
            template: template_id,
            marker,
            branches,
            source_span: span,
        });
    }

    #[allow(clippy::too_many_arguments)]
    fn compile_if_branches(
        &mut self,
        positive: &'a BlockIRNode<'a>,
        negative: Option<&'a NegativeBranch<'a>>,
        ast: &'a IfNode<'a>,
        site_id: u32,
        scope: &str,
        branches: &mut Vec<BranchDefinition>,
        installs: &mut String,
        conditions: &mut Vec<Option<String>>,
    ) {
        let mut ast_index = 0usize;
        let mut current_positive = positive;
        let mut current_negative = negative;
        loop {
            let branch = &ast.branches[ast_index];
            let span = self.spans.location_span(&branch.loc);
            let template = self.allocate_template(&branch.children, span);
            installs.push_str(&self.compile_block(
                current_positive,
                &branch.children,
                template,
                scope,
            ));
            let condition = branch
                .condition
                .as_ref()
                .map(expression_content)
                .map(|condition| self.rewrite_aliases(&condition));
            if let Some(condition) = branch.condition.as_ref() {
                let condition_span = self.spans.location_span(condition.loc());
                self.guest_expression_spans.insert(
                    format!("if:{site_id}:condition:{}", branches.len()),
                    condition_span,
                );
            }
            conditions.push(condition.clone());
            branches.push(BranchDefinition {
                index: branches.len() as u16,
                template,
            });
            ast_index += 1;
            match current_negative {
                Some(NegativeBranch::If(next)) => {
                    current_positive = &next.positive;
                    current_negative = next.negative.as_ref();
                }
                Some(NegativeBranch::Block(next)) => {
                    let branch = &ast.branches[ast_index];
                    let span = self.spans.location_span(&branch.loc);
                    let template = self.allocate_template(&branch.children, span);
                    installs.push_str(&self.compile_block(next, &branch.children, template, scope));
                    conditions.push(None);
                    branches.push(BranchDefinition {
                        index: branches.len() as u16,
                        template,
                    });
                    return;
                }
                None => {
                    // The host represents an inactive conditional with an
                    // immutable empty branch, never with an absent node handle.
                    let template = self.next_template;
                    self.next_template += 1;
                    self.templates.push(TemplateDefinition {
                        id: template,
                        markup: "<span hidden data-syrinx-empty-branch></span>".to_owned(),
                        source_span: self.spans.location_span(&ast.loc),
                    });
                    conditions.push(None);
                    branches.push(BranchDefinition {
                        index: branches.len() as u16,
                        template,
                    });
                    return;
                }
            }
        }
    }

    fn compile_for(
        &mut self,
        node: &'a ForIRNode<'a>,
        children: &'a [TemplateChildNode<'a>],
        template_id: u32,
        scope: &str,
        body: &mut String,
    ) {
        let id = self.next_list;
        self.next_list += 1;
        let marker = marker("list", id);
        let location = self
            .ir
            .control_source_map
            .get(&node.id)
            .expect("preflight pins list source location");
        self.annotations
            .add_control_anchor(location, marker.as_str());
        let ast = find_for(children, location).expect("preflight pins list AST");
        let span = self.spans.location_span(location);
        let item_template = self.allocate_template(&ast.children, span);
        let value_alias = node
            .value
            .as_ref()
            .map(|value| value.content.to_string())
            .unwrap_or_else(|| "__value".to_owned());
        let key_alias = node.key.as_ref().map(|key| key.content.to_string());
        let index_alias = node.index.as_ref().map(|index| index.content.to_string());
        let callback_key = key_alias.as_deref().unwrap_or("__key");
        let callback_index = index_alias.as_deref().unwrap_or("__index");
        let key_expression = self.rewrite_aliases(
            node.key_prop
                .as_ref()
                .expect("preflight requires a keyed v-for")
                .content
                .as_str(),
        );
        let source_expression = self.rewrite_aliases(node.source.content.as_str());
        self.guest_expression_spans.insert(
            format!("list:{id}:source"),
            self.spans.location_span(&node.source.loc),
        );
        self.guest_expression_spans.insert(
            format!("list:{id}:key"),
            self.spans.location_span(
                &node
                    .key_prop
                    .as_ref()
                    .expect("preflight requires a keyed v-for")
                    .loc,
            ),
        );
        let value_slot = format!("__syrinxValue{id}");
        let key_slot = format!("__syrinxKey{id}");
        let index_slot = format!("__syrinxIndex{id}");
        let mut aliases = BTreeMap::new();
        if let Some(alias) = node.value.as_ref() {
            aliases.insert(alias.content.to_string(), format!("{value_slot}.value"));
        }
        if let Some(alias) = node.key.as_ref() {
            aliases.insert(alias.content.to_string(), format!("{key_slot}.value"));
        }
        if let Some(alias) = node.index.as_ref() {
            aliases.insert(alias.content.to_string(), format!("{index_slot}.value"));
        }
        self.alias_scopes.push(aliases);
        let item_body =
            self.compile_block(&node.render, &ast.children, item_template, "__itemScope");
        self.alias_scopes.pop();
        writeln!(body, "__installKeyedList({scope}, {{").unwrap();
        writeln!(body, "  siteId: {id},").unwrap();
        writeln!(body, "  source: () => ({source_expression}),").unwrap();
        writeln!(
            body,
            "  key: ({value_alias}, {callback_key}, {callback_index}) => ({key_expression}),"
        )
        .unwrap();
        writeln!(
            body,
            "  install: (__itemScope, {value_slot}, {key_slot}, {index_slot}) => {{"
        )
        .unwrap();
        for line in item_body.lines() {
            writeln!(body, "    {line}").unwrap();
        }
        body.push_str("  },\n});\n");
        self.list_sites.push(KeyedListSite {
            id,
            template: template_id,
            marker,
            item_template,
            source_span: span,
        });
    }

    fn compile_component(
        &mut self,
        node: &'a CreateComponentIRNode<'a>,
        children: &'a [TemplateChildNode<'a>],
        template_id: u32,
        scope: &str,
        body: &mut String,
    ) {
        let link = self
            .component_links
            .get(node.tag.as_str())
            .cloned()
            .expect("preflight resolves every static child component");
        let id = self.next_child;
        self.next_child += 1;
        let marker = marker("child", id);
        let location = self.element_location(node.id).clone();
        self.annotations
            .add_control_anchor(&location, marker.as_str());
        let span = self.spans.location_span(&location);
        self.child_sites.push(ChildSite {
            id,
            template: template_id,
            marker,
            child_definition: link.definition_id,
            source_span: span,
        });

        let mut props = Vec::new();
        let mut listeners = Vec::new();
        let mut key = None;
        for prop in node.props.iter() {
            let name = prop.key.content.as_str();
            let expression = self.component_prop_expression(prop);
            if name == "key" {
                key = Some(expression);
            } else if name.starts_with("on") && name.len() > 2 {
                listeners.push(format!("{}: ({expression})", json(name)));
            } else {
                props.push(format!("{}: ({expression})", json(name)));
            }
        }
        let child_var = format!("__syrinxChild{id}");
        writeln!(body, "const {child_var} = __installChild({scope}, {{").unwrap();
        writeln!(body, "  siteId: {id},").unwrap();
        writeln!(body, "  definition: {},", link.definition_id).unwrap();
        if let Some(key) = key {
            writeln!(body, "  key: ({key}),").unwrap();
        }
        if !props.is_empty() {
            writeln!(body, "  props: () => ({{ {} }}),", props.join(", ")).unwrap();
        }
        if !listeners.is_empty() {
            writeln!(body, "  listeners: () => ({{ {} }}),", listeners.join(", ")).unwrap();
        }
        body.push_str("});\n");

        let ast =
            find_element(children, &location).expect("preflight pins component source location");
        for slot in node.slots.iter() {
            let slot_id = self.next_slot_definition;
            self.next_slot_definition += 1;
            let name = slot.name.content.to_string();
            let outlet_id = link.slot_outlets[&name];
            let slot_element = find_explicit_slot_element(ast, name.as_str());
            let slot_children = find_slot_children(ast, name.as_str())
                .expect("preflight pins static slot children");
            let condition_expression = find_slot_condition(ast, name.as_str())
                .or_else(|| slot_element.and_then(slot_if_expression));
            let condition = condition_expression
                .map(expression_content)
                .map(|condition| self.rewrite_aliases(&condition));
            if let Some(expression) = condition_expression {
                self.guest_expression_spans.insert(
                    format!("slot:{slot_id}:condition"),
                    self.spans.location_span(expression.loc()),
                );
            }
            let slot_span = self
                .spans
                .location_span(slot_element.map_or(&ast.loc, |element| &element.loc));
            let slot_template = self.allocate_template(slot_children, slot_span);
            let aliases = slot
                .fn_exp
                .as_ref()
                .map(|expression| slot_aliases(expression.content.as_str(), "__slotProps"))
                .unwrap_or_default();
            self.alias_scopes.push(aliases);
            let install =
                self.compile_block(&slot.block, slot_children, slot_template, "__slotScope");
            self.alias_scopes.pop();
            self.slot_definitions.push(SlotDefinition {
                id: slot_id,
                name: name.clone(),
                template: slot_template,
                source_span: slot_span,
            });
            if let Some(condition) = condition {
                writeln!(
                    body,
                    "__installConditionalSlot({scope}, () => ({condition}), () => "
                )
                .unwrap();
            }
            writeln!(body, "{scope}.mountSlot({{").unwrap();
            writeln!(body, "  slotDefinitionId: {slot_id},").unwrap();
            writeln!(body, "  mountOwner: {child_var},").unwrap();
            writeln!(body, "  slotOutletId: {outlet_id},").unwrap();
            writeln!(body, "  name: {},", json(name.as_str())).unwrap();
            body.push_str("  install(__slotScope, __slotProps) {\n");
            for line in install.lines() {
                writeln!(body, "    {line}").unwrap();
            }
            body.push_str("  },\n})");
            if condition_expression.is_some() {
                body.push_str(");\n");
            } else {
                body.push_str(";\n");
            }
        }
    }

    fn compile_slot_outlet(
        &mut self,
        node: &'a SlotOutletIRNode<'a>,
        children: &'a [TemplateChildNode<'a>],
        template_id: u32,
        scope: &str,
        body: &mut String,
    ) {
        let id = self.next_slot_outlet;
        self.next_slot_outlet += 1;
        let marker = marker("slot-outlet", id);
        let location = self.element_location(node.id).clone();
        self.annotations
            .add_control_anchor(&location, marker.as_str());
        let span = self.spans.location_span(&location);
        let ast =
            find_element(children, &location).expect("preflight pins slot outlet source location");
        let fallback_template = if let Some(fallback) = node.fallback.as_ref() {
            let template = self.allocate_template(&ast.children, span);
            let install = self.compile_block(fallback, &ast.children, template, scope);
            body.push_str(&install);
            Some(template)
        } else {
            None
        };
        let name = node.name.content.to_string();
        self.slot_outlets.push(SlotOutlet {
            id,
            template: template_id,
            marker,
            name: name.clone(),
            fallback_template,
            source_span: span,
        });
        let props = node
            .props
            .iter()
            .map(|prop| {
                format!(
                    "{}: ({})",
                    json(camelize_prop(prop.key.content.as_str()).as_str()),
                    self.component_prop_expression(prop)
                )
            })
            .collect::<Vec<_>>();
        writeln!(body, "__installSlotOutlet({scope}, {{").unwrap();
        writeln!(body, "  outletId: {id},").unwrap();
        writeln!(body, "  name: {},", json(name.as_str())).unwrap();
        if !props.is_empty() {
            writeln!(body, "  props: () => ({{ {} }}),", props.join(", ")).unwrap();
        }
        body.push_str("});\n");
    }

    fn component_prop_expression(&self, prop: &IRProp<'a>) -> String {
        let values = prop
            .values
            .iter()
            .map(|value| {
                if value.is_static {
                    json(value.content.as_str())
                } else {
                    self.rewrite_aliases(value.content.as_str())
                }
            })
            .collect::<Vec<_>>();
        match values.as_slice() {
            [] => json(""),
            [value] => value.clone(),
            _ => format!("[{}]", values.join(", ")),
        }
    }

    fn element_location(&self, id: usize) -> &SourceLocation {
        self.ir
            .element_source_map
            .get(&id)
            .expect("preflight pins renderer element source location")
    }

    fn rewrite_aliases(&self, source: &str) -> String {
        if self.alias_scopes.is_empty() {
            return source.to_owned();
        }
        let mut aliases = BTreeMap::new();
        for scope in &self.alias_scopes {
            aliases.extend(scope.clone());
        }
        rewrite_alias_references(source, &aliases)
    }
}

struct AliasRewrite {
    start: usize,
    end: usize,
    replacement: String,
}

struct AliasRewriteCollector<'m> {
    aliases: &'m BTreeMap<String, String>,
    rewrites: Vec<AliasRewrite>,
    local_scopes: Vec<BTreeSet<String>>,
}

impl<'m> AliasRewriteCollector<'m> {
    fn new(aliases: &'m BTreeMap<String, String>) -> Self {
        Self {
            aliases,
            rewrites: Vec::new(),
            local_scopes: Vec::new(),
        }
    }

    fn push_scope(&mut self) {
        self.local_scopes.push(BTreeSet::new());
    }

    fn pop_scope(&mut self) {
        self.local_scopes.pop();
    }

    fn is_local(&self, name: &str) -> bool {
        self.local_scopes
            .iter()
            .rev()
            .any(|scope| scope.contains(name))
    }

    fn replacement(&self, name: &str) -> Option<&str> {
        (!self.is_local(name))
            .then(|| self.aliases.get(name).map(String::as_str))
            .flatten()
    }

    fn add_binding_pattern(&mut self, pattern: &oxc_ast::ast::BindingPattern<'_>) {
        match pattern {
            oxc_ast::ast::BindingPattern::BindingIdentifier(identifier) => {
                if let Some(scope) = self.local_scopes.last_mut() {
                    scope.insert(identifier.name.to_string());
                }
            }
            oxc_ast::ast::BindingPattern::ObjectPattern(object) => {
                for property in &object.properties {
                    self.add_binding_pattern(&property.value);
                }
                if let Some(rest) = &object.rest {
                    self.add_binding_pattern(&rest.argument);
                }
            }
            oxc_ast::ast::BindingPattern::ArrayPattern(array) => {
                for element in array.elements.iter().flatten() {
                    self.add_binding_pattern(element);
                }
                if let Some(rest) = &array.rest {
                    self.add_binding_pattern(&rest.argument);
                }
            }
            oxc_ast::ast::BindingPattern::AssignmentPattern(assignment) => {
                self.add_binding_pattern(&assignment.left);
            }
        }
    }
}

impl<'a> Visit<'a> for AliasRewriteCollector<'_> {
    fn visit_identifier_reference(&mut self, identifier: &oxc_ast::ast::IdentifierReference<'a>) {
        if let Some(replacement) = self.replacement(identifier.name.as_str()) {
            self.rewrites.push(AliasRewrite {
                start: identifier.span.start as usize,
                end: identifier.span.end as usize,
                replacement: replacement.to_owned(),
            });
        }
    }

    fn visit_object_property(&mut self, property: &oxc_ast::ast::ObjectProperty<'a>) {
        if property.shorthand
            && let PropertyKey::StaticIdentifier(identifier) = &property.key
            && let Some(replacement) = self.replacement(identifier.name.as_str())
        {
            self.rewrites.push(AliasRewrite {
                start: property.span.start as usize,
                end: property.span.end as usize,
                replacement: format!("{}: {replacement}", identifier.name),
            });
            return;
        }
        walk_object_property(self, property);
    }

    fn visit_arrow_function_expression(
        &mut self,
        arrow: &oxc_ast::ast::ArrowFunctionExpression<'a>,
    ) {
        self.push_scope();
        for parameter in &arrow.params.items {
            self.add_binding_pattern(&parameter.pattern);
        }
        walk_arrow_function_expression(self, arrow);
        self.pop_scope();
    }

    fn visit_function(&mut self, function: &oxc_ast::ast::Function<'a>, flags: ScopeFlags) {
        self.push_scope();
        for parameter in &function.params.items {
            self.add_binding_pattern(&parameter.pattern);
        }
        walk_function(self, function, flags);
        self.pop_scope();
    }

    fn visit_variable_declarator(&mut self, declarator: &oxc_ast::ast::VariableDeclarator<'a>) {
        walk_variable_declarator(self, declarator);
        self.add_binding_pattern(&declarator.id);
    }
}

fn rewrite_alias_references(source: &str, aliases: &BTreeMap<String, String>) -> String {
    if aliases.is_empty() {
        return source.to_owned();
    }
    let source_type = SourceType::ts().with_module(true);
    let allocator = Allocator::default();
    let mut wrapped = String::with_capacity(source.len() + 2);
    wrapped.push('(');
    wrapped.push_str(source);
    wrapped.push(')');
    if let Ok(expression) = Parser::new(&allocator, &wrapped, source_type).parse_expression() {
        let mut collector = AliasRewriteCollector::new(aliases);
        collector.visit_expression(&expression);
        return apply_alias_rewrites(source, collector.rewrites, 1);
    }

    let allocator = Allocator::default();
    let parsed = Parser::new(&allocator, source, source_type).parse();
    if !parsed.diagnostics.is_empty() {
        return source.to_owned();
    }
    let mut collector = AliasRewriteCollector::new(aliases);
    collector.push_scope();
    collector.visit_program(&parsed.program);
    collector.pop_scope();
    apply_alias_rewrites(source, collector.rewrites, 0)
}

fn apply_alias_rewrites(source: &str, mut rewrites: Vec<AliasRewrite>, offset: usize) -> String {
    rewrites.sort_by(|left, right| {
        right
            .start
            .cmp(&left.start)
            .then_with(|| right.end.cmp(&left.end))
    });
    let mut output = source.to_owned();
    for rewrite in rewrites {
        let start = rewrite.start.saturating_sub(offset);
        let end = rewrite.end.saturating_sub(offset);
        if start <= end && end <= output.len() {
            output.replace_range(start..end, &rewrite.replacement);
        }
    }
    output
}

struct SpanRegistry<'a> {
    source: &'a str,
    filename: &'a str,
    template_loc: &'a vize_atelier_sfc::BlockLocation,
    by_range: BTreeMap<(u32, u32), u32>,
    spans: Vec<SourceSpan>,
}

impl<'a> SpanRegistry<'a> {
    fn new(
        source: &'a str,
        filename: &'a str,
        template_loc: &'a vize_atelier_sfc::BlockLocation,
    ) -> Self {
        Self {
            source,
            filename,
            template_loc,
            by_range: BTreeMap::new(),
            spans: Vec::new(),
        }
    }

    fn block_span(&mut self, location: &vize_atelier_sfc::BlockLocation) -> u32 {
        self.absolute_span(location.start, location.end)
    }

    fn location_span(&mut self, location: &SourceLocation) -> u32 {
        self.absolute_span(
            self.template_loc.start + location.start.offset as usize,
            self.template_loc.start + location.end.offset as usize,
        )
    }

    fn absolute_span(&mut self, start: usize, end: usize) -> u32 {
        let start = start.min(self.source.len()) as u32;
        let end = end.min(self.source.len()) as u32;
        if let Some(id) = self.by_range.get(&(start, end)) {
            return *id;
        }
        let id = self.spans.len() as u32 + 1;
        let (start_line, start_column) = line_column(self.source, start as usize);
        let (end_line, end_column) = line_column(self.source, end as usize);
        self.spans.push(SourceSpan {
            id,
            source: self.filename.to_owned(),
            start_byte: start,
            end_byte: end,
            start_line,
            start_column,
            end_line,
            end_column,
        });
        self.by_range.insert((start, end), id);
        id
    }

    fn into_spans(self) -> Vec<SourceSpan> {
        self.spans
    }
}

fn preflight_template(
    source: &str,
    filename: &str,
    template_loc: &vize_atelier_sfc::BlockLocation,
    children: &[TemplateChildNode<'_>],
    ir: &RootIRNode<'_>,
    component_links: &BTreeMap<String, SyrinxComponentLink>,
) -> Vec<SyrinxDiagnostic> {
    let mut diagnostics = Vec::new();
    inspect_ast(source, filename, template_loc, children, &mut diagnostics);
    inspect_block(
        source,
        filename,
        template_loc,
        &ir.block,
        ir,
        component_links,
        &mut diagnostics,
    );
    diagnostics
}

fn inspect_ast(
    source: &str,
    filename: &str,
    block: &vize_atelier_sfc::BlockLocation,
    children: &[TemplateChildNode<'_>],
    diagnostics: &mut Vec<SyrinxDiagnostic>,
) {
    for child in children {
        match child {
            TemplateChildNode::Element(element) => {
                if element.tag.as_str() == "component" {
                    diagnostics.push(location_diagnostic(
                        source,
                        filename,
                        block,
                        "SYRINX_UNSUPPORTED_DYNAMIC_COMPONENT",
                        "Dynamic components are not in the protocol-v1 composition profile.",
                        &element.loc,
                    ));
                }
                if element.ns != Namespace::Html {
                    diagnostics.push(location_diagnostic(
                        source,
                        filename,
                        block,
                        "SYRINX_UNSUPPORTED_NAMESPACE",
                        "SVG and MathML namespaces are not yet represented by ComponentPlanV1.",
                        &element.loc,
                    ));
                }
                let is_explicit_slot = element.tag_type == ElementType::Template
                    && element.props.iter().any(|prop| {
                        matches!(prop, PropNode::Directive(directive)
                            if directive.name.as_str() == "slot")
                    });
                for prop in element.props.iter() {
                    let is_ref = match prop {
                        PropNode::Attribute(attribute) => {
                            matches!(attribute.name.as_str(), "ref" | "ref_for" | "ref_key")
                        }
                        PropNode::Directive(directive) if directive.name.as_str() == "bind" => {
                            matches!(
                                directive.arg.as_ref(),
                                Some(ExpressionNode::Simple(argument))
                                    if matches!(argument.content.as_str(), "ref" | "ref_for" | "ref_key")
                            )
                        }
                        _ => false,
                    };
                    if is_ref {
                        diagnostics.push(location_diagnostic(
                            source,
                            filename,
                            block,
                            "SYRINX_TEMPLATE_REF_FORBIDDEN",
                            "Template refs expose node identity; use a compiler-declared Query/Measure or HostAction target.",
                            prop.loc(),
                        ));
                    }
                    if let PropNode::Directive(directive) = prop
                        && directive.name.as_str() == "bind"
                        && matches!(
                            directive.arg.as_ref(),
                            Some(ExpressionNode::Simple(argument)) if !argument.is_static
                        )
                    {
                        diagnostics.push(location_diagnostic(
                            source,
                            filename,
                            block,
                            "SYRINX_UNSUPPORTED_DYNAMIC_PROP_NAME",
                            "Dynamic attribute/property names cannot become closed compiler-authorized sites.",
                            prop.loc(),
                        ));
                    }
                    if is_explicit_slot && let PropNode::Directive(directive) = prop {
                        let (code, message) = match directive.name.as_str() {
                            "for" => (
                                "SYRINX_UNSUPPORTED_SLOT_V_FOR",
                                "v-for slot declarations require a dynamic slot-set protocol; put the keyed list inside a static slot instead.",
                            ),
                            "else" | "else-if" => (
                                "SYRINX_UNSUPPORTED_SLOT_ELSE_BRANCH",
                                "Conditional slots support one v-if declaration in protocol v1; v-else branches require grouped slot-set routing.",
                            ),
                            _ => continue,
                        };
                        diagnostics.push(location_diagnostic(
                            source,
                            filename,
                            block,
                            code,
                            message,
                            prop.loc(),
                        ));
                    }
                }
                inspect_ast(source, filename, block, &element.children, diagnostics);
            }
            TemplateChildNode::If(node) => {
                if node
                    .branches
                    .iter()
                    .any(|branch| contains_explicit_slot_template(&branch.children))
                    && (node.branches.len() != 1 || node.branches[0].condition.is_none())
                {
                    diagnostics.push(location_diagnostic(
                        source,
                        filename,
                        block,
                        "SYRINX_UNSUPPORTED_SLOT_ELSE_BRANCH",
                        "Conditional slots support one v-if declaration in protocol v1; v-else branches require grouped slot-set routing.",
                        &node.loc,
                    ));
                }
                for branch in node.branches.iter() {
                    inspect_ast(source, filename, block, &branch.children, diagnostics);
                }
            }
            TemplateChildNode::For(node) => {
                if contains_explicit_slot_template(&node.children) {
                    diagnostics.push(location_diagnostic(
                        source,
                        filename,
                        block,
                        "SYRINX_UNSUPPORTED_SLOT_V_FOR",
                        "v-for slot declarations require a dynamic slot-set protocol; put the keyed list inside a static slot instead.",
                        &node.loc,
                    ));
                }
                inspect_ast(source, filename, block, &node.children, diagnostics);
            }
            _ => {}
        }
    }
}

fn contains_explicit_slot_template(children: &[TemplateChildNode<'_>]) -> bool {
    children.iter().any(|child| match child {
        TemplateChildNode::Element(element) => {
            element.tag_type == ElementType::Template
                && element.props.iter().any(|prop| {
                    matches!(prop, PropNode::Directive(directive)
                        if directive.name.as_str() == "slot")
                })
        }
        TemplateChildNode::If(node) => node
            .branches
            .iter()
            .any(|branch| contains_explicit_slot_template(&branch.children)),
        TemplateChildNode::For(node) => contains_explicit_slot_template(&node.children),
        _ => false,
    })
}

fn inspect_block(
    source: &str,
    filename: &str,
    template_loc: &vize_atelier_sfc::BlockLocation,
    block: &BlockIRNode<'_>,
    ir: &RootIRNode<'_>,
    component_links: &BTreeMap<String, SyrinxComponentLink>,
    diagnostics: &mut Vec<SyrinxDiagnostic>,
) {
    for operation in block.operation.iter() {
        inspect_operation(
            source,
            filename,
            template_loc,
            operation,
            ir,
            component_links,
            diagnostics,
        );
    }
    for effect in block.effect.iter() {
        for operation in effect.operations.iter() {
            inspect_operation(
                source,
                filename,
                template_loc,
                operation,
                ir,
                component_links,
                diagnostics,
            );
        }
    }
}

fn inspect_operation(
    source: &str,
    filename: &str,
    template_loc: &vize_atelier_sfc::BlockLocation,
    operation: &OperationNode<'_>,
    ir: &RootIRNode<'_>,
    component_links: &BTreeMap<String, SyrinxComponentLink>,
    diagnostics: &mut Vec<SyrinxDiagnostic>,
) {
    let unsupported = match operation {
        OperationNode::SetDynamicProps(node) => node
            .props
            .first()
            .map(|value| ("SYRINX_UNSUPPORTED_DYNAMIC_PROPS", "Dynamic property names and object v-bind cannot be authorized as closed ComponentPlan sites.", &value.loc)),
        OperationNode::SetHtml(node) => Some((
            "SYRINX_UNSUPPORTED_RAW_HTML",
            "v-html/innerHTML is outside the renderer-neutral SFC profile.",
            &node.value.loc,
        )),
        OperationNode::SetTemplateRef(node) => Some((
            "SYRINX_TEMPLATE_REF_FORBIDDEN",
            "Template refs expose node identity; use a typed Query/Measure target.",
            &node.value.loc,
        )),
        OperationNode::Directive(node) => Some((
            "SYRINX_UNSUPPORTED_DIRECTIVE",
            "This runtime directive has no renderer-neutral ComponentPlan lowering.",
            &node.dir.loc,
        )),
        _ => None,
    };
    if let Some((code, message, location)) = unsupported {
        diagnostics.push(location_diagnostic(
            source,
            filename,
            template_loc,
            code,
            message,
            location,
        ));
    }
    match operation {
        OperationNode::SetProp(node) if !node.prop.key.is_static => {
            diagnostics.push(location_diagnostic(
                source,
                filename,
                template_loc,
                "SYRINX_UNSUPPORTED_DYNAMIC_PROP_NAME",
                "Dynamic attribute/property names cannot become closed compiler-authorized sites.",
                &node.prop.key.loc,
            ));
        }
        OperationNode::SetProp(node) if node.prop.key.content.as_str() == "style" => {
            let expression = expression_values(&node.prop.values, false);
            if let Err(reason) = split_style_object(&expression) {
                let location = node
                    .prop
                    .values
                    .first()
                    .map(|value| &value.loc)
                    .unwrap_or(&node.prop.key.loc);
                diagnostics.push(location_diagnostic(
                    source,
                    filename,
                    template_loc,
                    "SYRINX_UNSUPPORTED_DYNAMIC_STYLE_MAP",
                    &format!(
                        "ComponentPlan style sites require a statically named object literal: {reason}"
                    ),
                    location,
                ));
            }
        }
        OperationNode::SetEvent(node) if !node.key.is_static => {
            diagnostics.push(location_diagnostic(
                source,
                filename,
                template_loc,
                "SYRINX_UNSUPPORTED_DYNAMIC_EVENT_NAME",
                "Dynamic event names cannot become closed compiler-authorized handlers.",
                &node.key.loc,
            ));
        }
        OperationNode::SetEvent(node) => {
            const SUPPORTED: &[&str] = &[
                "stop", "prevent", "self", "ctrl", "shift", "alt", "meta", "exact", "left",
                "middle", "right",
            ];
            for modifier in &node.modifiers.non_keys {
                if !SUPPORTED.contains(&modifier.as_str()) {
                    diagnostics.push(location_diagnostic(
                        source,
                        filename,
                        template_loc,
                        "SYRINX_UNSUPPORTED_EVENT_MODIFIER",
                        &format!(
                            "Event modifier .{modifier} has no protocol-v1 value-event guard."
                        ),
                        node.value
                            .as_ref()
                            .map(|value| &value.loc)
                            .unwrap_or(&node.key.loc),
                    ));
                }
            }
            if node.modifiers.options.passive
                && node
                    .modifiers
                    .non_keys
                    .iter()
                    .any(|modifier| modifier == "prevent")
            {
                diagnostics.push(location_diagnostic(
                    source,
                    filename,
                    template_loc,
                    "SYRINX_INVALID_PASSIVE_PREVENT_HANDLER",
                    "A passive listener cannot prevent default; remove .passive or .prevent.",
                    node.value
                        .as_ref()
                        .map(|value| &value.loc)
                        .unwrap_or(&node.key.loc),
                ));
            }
        }
        OperationNode::CreateComponent(node) => {
            let stub = SourceLocation::STUB;
            let location = ir.element_source_map.get(&node.id).unwrap_or(&stub);
            if node.kind != ComponentKind::Regular || node.is_expr.is_some() {
                diagnostics.push(location_diagnostic(
                    source,
                    filename,
                    template_loc,
                    "SYRINX_UNSUPPORTED_DYNAMIC_COMPONENT",
                    "Only statically linked ordinary SFC components are supported; dynamic and built-in components fail compilation.",
                    location,
                ));
            }
            let link = component_links.get(node.tag.as_str());
            if link.is_none() {
                diagnostics.push(location_diagnostic(
                    source,
                    filename,
                    template_loc,
                    "SYRINX_UNRESOLVED_COMPONENT",
                    &format!(
                        "Component <{}> has no static Syrinx component link.",
                        node.tag
                    ),
                    location,
                ));
            }
            if node.dynamic_slots {
                diagnostics.push(location_diagnostic(
                    source,
                    filename,
                    template_loc,
                    "SYRINX_UNSUPPORTED_DYNAMIC_SLOT",
                    "Dynamic slot names are outside protocol v1; use a statically named slot, optionally guarded by v-if.",
                    location,
                ));
            }
            if node.v_show.is_some() {
                diagnostics.push(location_diagnostic(
                    source,
                    filename,
                    template_loc,
                    "SYRINX_UNSUPPORTED_COMPONENT_V_SHOW",
                    "v-show on a component has no renderer-neutral component operation.",
                    location,
                ));
            }
            for prop in node.props.iter() {
                if !prop.key.is_static || prop.key.content.as_str() == "$" {
                    diagnostics.push(location_diagnostic(
                        source,
                        filename,
                        template_loc,
                        "SYRINX_UNSUPPORTED_COMPONENT_PROP_SPREAD",
                        "Component props/listeners require statically named value entries.",
                        &prop.key.loc,
                    ));
                }
            }
            for slot in node.slots.iter() {
                if !slot.name.is_static {
                    diagnostics.push(location_diagnostic(
                        source,
                        filename,
                        template_loc,
                        "SYRINX_UNSUPPORTED_DYNAMIC_SLOT_NAME",
                        "Slot names must be static so the child outlet route is compiler-authorized.",
                        location,
                    ));
                } else if link
                    .is_some_and(|link| !link.slot_outlets.contains_key(slot.name.content.as_str()))
                {
                    diagnostics.push(location_diagnostic(
                        source,
                        filename,
                        template_loc,
                        "SYRINX_UNKNOWN_SLOT_OUTLET",
                        &format!(
                            "Child <{}> does not declare a '{}' slot outlet.",
                            node.tag, slot.name.content
                        ),
                        location,
                    ));
                }
                if let Some(pattern) = slot.fn_exp.as_ref()
                    && !is_supported_slot_pattern(pattern.content.as_str())
                {
                    diagnostics.push(location_diagnostic(
                        source,
                        filename,
                        template_loc,
                        "SYRINX_UNSUPPORTED_SLOT_PATTERN",
                        "Scoped slots accept one identifier or a flat object destructure in protocol v1.",
                        location,
                    ));
                }
                inspect_block(
                    source,
                    filename,
                    template_loc,
                    &slot.block,
                    ir,
                    component_links,
                    diagnostics,
                );
            }
        }
        OperationNode::SlotOutlet(node) => {
            let stub = SourceLocation::STUB;
            let location = ir.element_source_map.get(&node.id).unwrap_or(&stub);
            if !node.name.is_static {
                diagnostics.push(location_diagnostic(
                    source,
                    filename,
                    template_loc,
                    "SYRINX_UNSUPPORTED_DYNAMIC_SLOT_NAME",
                    "Slot outlet names must be static in protocol v1.",
                    location,
                ));
            }
            for prop in node.props.iter() {
                if !prop.key.is_static || prop.key.content.as_str() == "$" {
                    diagnostics.push(location_diagnostic(
                        source,
                        filename,
                        template_loc,
                        "SYRINX_UNSUPPORTED_SLOT_PROP_SPREAD",
                        "Scoped slot props require statically named value entries.",
                        &prop.key.loc,
                    ));
                }
            }
            if let Some(fallback) = node.fallback.as_ref() {
                inspect_block(
                    source,
                    filename,
                    template_loc,
                    fallback,
                    ir,
                    component_links,
                    diagnostics,
                );
            }
        }
        OperationNode::If(node) => {
            if !ir.control_source_map.contains_key(&node.id) {
                diagnostics.push(location_diagnostic(
                    source,
                    filename,
                    template_loc,
                    "SYRINX_MISSING_CONTROL_LOCATION",
                    "Vapor IR did not retain the source location of this conditional.",
                    &node.condition.loc,
                ));
            }
            inspect_block(
                source,
                filename,
                template_loc,
                &node.positive,
                ir,
                component_links,
                diagnostics,
            );
            inspect_negative(
                source,
                filename,
                template_loc,
                node.negative.as_ref(),
                ir,
                component_links,
                diagnostics,
            );
        }
        OperationNode::For(node) => {
            if node.key_prop.is_none() {
                diagnostics.push(location_diagnostic(
                    source,
                    filename,
                    template_loc,
                    "SYRINX_UNKEYED_FOR_FORBIDDEN",
                    "Every v-for crossing the ComponentPlan boundary requires an explicit stable :key.",
                    &node.source.loc,
                ));
            }
            for alias in [&node.value, &node.key, &node.index].into_iter().flatten() {
                if !is_simple_binding_identifier(alias.content.as_str()) {
                    diagnostics.push(location_diagnostic(
                        source,
                        filename,
                        template_loc,
                        "SYRINX_UNSUPPORTED_FOR_ALIAS_PATTERN",
                        "Destructured and non-identifier v-for aliases are not in the v2.2 keyed-list contract; bind one identifier and destructure it in <script setup>.",
                        ir.control_source_map
                            .get(&node.id)
                            .unwrap_or(&node.source.loc),
                    ));
                }
            }
            if !ir.control_source_map.contains_key(&node.id) {
                diagnostics.push(location_diagnostic(
                    source,
                    filename,
                    template_loc,
                    "SYRINX_MISSING_CONTROL_LOCATION",
                    "Vapor IR did not retain the source location of this keyed list.",
                    &node.source.loc,
                ));
            }
            inspect_block(
                source,
                filename,
                template_loc,
                &node.render,
                ir,
                component_links,
                diagnostics,
            );
        }
        _ => {}
    }
}

fn is_simple_binding_identifier(source: &str) -> bool {
    let allocator = Allocator::default();
    let wrapped = format!("({source}) => 0");
    let parser = Parser::new(&allocator, &wrapped, SourceType::ts().with_module(true));
    let Ok(Expression::ArrowFunctionExpression(arrow)) = parser.parse_expression() else {
        return false;
    };
    arrow.params.rest.is_none()
        && arrow.params.items.len() == 1
        && matches!(
            arrow.params.items[0].pattern,
            oxc_ast::ast::BindingPattern::BindingIdentifier(_)
        )
}

fn is_supported_slot_pattern(source: &str) -> bool {
    let trimmed = source.trim();
    if is_simple_binding_identifier(trimmed) {
        return true;
    }
    let Some(object) = trimmed
        .strip_prefix('{')
        .and_then(|value| value.strip_suffix('}'))
    else {
        return false;
    };
    object
        .split(',')
        .map(str::trim)
        .filter(|field| !field.is_empty())
        .all(|field| {
            if field.starts_with("...") {
                return false;
            }
            let field = field.split('=').next().unwrap_or(field).trim();
            let (property, local) = field
                .split_once(':')
                .map_or((field, field), |(property, local)| {
                    (property.trim(), local.trim())
                });
            is_simple_binding_identifier(property) && is_simple_binding_identifier(local)
        })
}

fn inspect_negative(
    source: &str,
    filename: &str,
    template_loc: &vize_atelier_sfc::BlockLocation,
    negative: Option<&NegativeBranch<'_>>,
    ir: &RootIRNode<'_>,
    component_links: &BTreeMap<String, SyrinxComponentLink>,
    diagnostics: &mut Vec<SyrinxDiagnostic>,
) {
    match negative {
        Some(NegativeBranch::Block(block)) => inspect_block(
            source,
            filename,
            template_loc,
            block,
            ir,
            component_links,
            diagnostics,
        ),
        Some(NegativeBranch::If(node)) => {
            inspect_block(
                source,
                filename,
                template_loc,
                &node.positive,
                ir,
                component_links,
                diagnostics,
            );
            inspect_negative(
                source,
                filename,
                template_loc,
                node.negative.as_ref(),
                ir,
                component_links,
                diagnostics,
            );
        }
        None => {}
    }
}

fn compile_styles(
    source: &str,
    filename: &str,
    descriptor: &SfcDescriptor<'_>,
    component_id: u32,
    scope_token: Option<String>,
    spans: &mut SpanRegistry<'_>,
) -> Result<(String, Vec<Stylesheet>), SyrinxCompileFailure> {
    let mut css = String::new();
    let mut stylesheets = Vec::new();
    for (index, style) in descriptor.styles.iter().enumerate() {
        if style.src.is_some() || style.lang.as_deref().is_some_and(|lang| lang != "css") {
            return Err(SyrinxCompileFailure {
                diagnostics: vec![file_diagnostic(
                    source,
                    filename,
                    "SYRINX_UNSUPPORTED_STYLE_SOURCE",
                    "External and preprocessed styles must be resolved before Syrinx compilation.",
                    style.loc.tag_start,
                    style.loc.tag_end,
                )],
            });
        }
        let compiled = compile_style(
            style,
            &StyleCompileOptions {
                id: scope_token
                    .clone()
                    .unwrap_or_else(|| "data-v-syrinx".to_owned())
                    .into(),
                scoped: style.scoped,
                trim: true,
                source_map: true,
                ..Default::default()
            },
        )
        .map_err(|error| SyrinxCompileFailure {
            diagnostics: vec![file_diagnostic(
                source,
                filename,
                error
                    .code
                    .as_deref()
                    .unwrap_or("SYRINX_STYLE_COMPILE_ERROR"),
                error.message.as_str(),
                style.loc.start,
                style.loc.end,
            )],
        })?
        .to_string();
        if !css.is_empty() {
            css.push('\n');
        }
        css.push_str(&compiled);
        if !compiled.ends_with('\n') {
            css.push('\n');
        }
        stylesheets.push(Stylesheet {
            id: index as u32 + 1,
            owner_component: component_id,
            scope_token: style.scoped.then(|| scope_token.clone()).flatten(),
            css: compiled,
            source_span: spans.absolute_span(style.loc.start, style.loc.end),
        });
    }
    Ok((css, stylesheets))
}

fn validate_options(source: &str, options: &SyrinxCompileOptions) -> Vec<SyrinxDiagnostic> {
    let mut diagnostics = Vec::new();
    if options.component_id == 0 {
        diagnostics.push(file_diagnostic(
            source,
            &options.filename,
            "SYRINX_INVALID_COMPONENT_ID",
            "Component IDs are non-zero protocol identities.",
            0,
            0,
        ));
    }
    if options.protocol_schema_sha256.len() != 64
        || !options
            .protocol_schema_sha256
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        diagnostics.push(file_diagnostic(
            source,
            &options.filename,
            "SYRINX_INVALID_SCHEMA_HASH",
            "protocol_schema_sha256 must be exactly 64 lowercase hexadecimal characters.",
            0,
            0,
        ));
    }
    if options.protocol
        != (ProtocolVersion {
            major: 1,
            minor: 0,
            patch: 0,
        })
    {
        diagnostics.push(file_diagnostic(
            source,
            &options.filename,
            "SYRINX_UNSUPPORTED_PROTOCOL_VERSION",
            "This backend emits exactly ComponentPlan protocol 1.0.0.",
            0,
            0,
        ));
    }
    for (actual, expected, code, label) in [
        (
            options.vize_revision.as_str(),
            VIZE_UPSTREAM_REVISION,
            "SYRINX_VIZE_REVISION_MISMATCH",
            "Vize revision",
        ),
        (
            options.vue_reactivity_version.as_str(),
            VUE_REACTIVITY_VERSION,
            "SYRINX_VUE_VERSION_MISMATCH",
            "Vue reactivity version",
        ),
        (
            options.guest_runtime_module.as_str(),
            GUEST_RUNTIME_MODULE,
            "SYRINX_GUEST_RUNTIME_MODULE_MISMATCH",
            "guest runtime module",
        ),
        (
            options.guest_runtime_version.as_str(),
            GUEST_RUNTIME_VERSION,
            "SYRINX_GUEST_RUNTIME_VERSION_MISMATCH",
            "guest runtime version",
        ),
    ] {
        if actual != expected {
            diagnostics.push(file_diagnostic(
                source,
                &options.filename,
                code,
                &format!("{label} {actual:?} does not match the backend pin {expected:?}."),
                0,
                0,
            ));
        }
    }
    if options.required_capability_bits & !ALL_V1_CAPABILITY_BITS != 0 {
        diagnostics.push(file_diagnostic(
            source,
            &options.filename,
            "SYRINX_UNKNOWN_CAPABILITY_BITS",
            "required_capability_bits contains features not assigned by ComponentPlan protocol v1.",
            0,
            0,
        ));
    }
    let mut names = BTreeSet::new();
    for (id, name) in &options.input_names {
        if *id == 0 || name.is_empty() || !names.insert(name.as_str()) {
            diagnostics.push(file_diagnostic(
                source,
                &options.filename,
                "SYRINX_INVALID_INPUT_MAP",
                "Input names require unique non-zero IDs and non-empty authored names.",
                0,
                0,
            ));
            break;
        }
    }
    diagnostics
}

fn resolve_input_names(
    declared: &BTreeMap<u32, String>,
    bindings: &BindingMetadata,
) -> BTreeMap<u32, String> {
    let mut result = declared.clone();
    let mut assigned: BTreeSet<_> = result.values().cloned().collect();
    let mut props: Vec<_> = bindings
        .bindings
        .iter()
        .filter(|(_, binding)| matches!(binding, BindingType::Props | BindingType::PropsAliased))
        .map(|(name, _)| name.to_string())
        .collect();
    props.sort();
    props.dedup();
    let mut next_id = 1u32;
    for name in props {
        if !assigned.insert(name.clone()) {
            continue;
        }
        while result.contains_key(&next_id) {
            next_id = next_id.checked_add(1).expect("input ID space exhausted");
        }
        result.insert(next_id, name);
        next_id = next_id.checked_add(1).expect("input ID space exhausted");
    }
    result
}

fn reject_browser_identities(
    descriptor: &SfcDescriptor<'_>,
    filename: &str,
) -> Vec<SyrinxDiagnostic> {
    let Some(script) = descriptor.script_setup.as_ref() else {
        return Vec::new();
    };
    let forbidden = [
        "document",
        "window",
        "globalThis",
        "self",
        "Element",
        "HTMLElement",
        "Node",
        "querySelector",
        "getElementById",
        "getBoundingClientRect",
    ];
    let source_type = match script.lang.as_deref() {
        Some("ts") => SourceType::ts(),
        Some("tsx") => SourceType::tsx(),
        Some("jsx") => SourceType::jsx(),
        _ => SourceType::default(),
    }
    .with_module(true);
    let allocator = Allocator::default();
    let parsed = Parser::new(&allocator, script.content.as_ref(), source_type).parse();
    if parsed.panicked || !parsed.diagnostics.is_empty() {
        // Guest assembly reports the authoritative script parse diagnostic.
        return Vec::new();
    }
    let semantic = SemanticBuilder::new_compiler()
        .with_build_nodes(true)
        .build(&parsed.program)
        .semantic;
    let scoping = semantic.scoping();
    let mut diagnostics = Vec::new();
    for identifier in forbidden {
        let Some(references) = scoping.root_unresolved_references().get(identifier) else {
            continue;
        };
        for reference_id in references {
            let reference = scoping.get_reference(*reference_id);
            let span = semantic.nodes().get_node(reference.node_id()).span();
            diagnostics.push(file_diagnostic(
                descriptor.source.as_ref(),
                filename,
                "SYRINX_BROWSER_NODE_IDENTITY_FORBIDDEN",
                &format!(
                    "{identifier} is a browser node/DOM capability; declare a typed Query/Measure or HostAction instead."
                ),
                script.loc.start + span.start as usize,
                script.loc.start + span.end as usize,
            ));
        }
    }
    diagnostics
}

fn expression_values(
    values: &[vize_carton::Box<'_, SimpleExpressionNode<'_>>],
    preserve_all: bool,
) -> String {
    if values.is_empty() {
        return "undefined".to_owned();
    }
    if preserve_all && values.len() > 1 {
        return format!(
            "[{}]",
            values
                .iter()
                .map(|value| expression_value(value))
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    expression_value(values.last().expect("non-empty values"))
}

struct StyleProperty {
    name: String,
    expression: String,
}

fn split_style_object(source: &str) -> Result<Vec<StyleProperty>, &'static str> {
    let allocator = Allocator::default();
    let expression = Parser::new(&allocator, source, SourceType::ts())
        .parse_expression()
        .map_err(|_| "the style expression is not valid JavaScript")?;
    let Expression::ObjectExpression(object) = &expression else {
        return Err(
            "dynamic style strings, arrays, and opaque objects do not expose closed property names",
        );
    };
    let mut names = BTreeSet::new();
    let mut properties = Vec::with_capacity(object.properties.len());
    for entry in &object.properties {
        let ObjectPropertyKind::ObjectProperty(property) = entry else {
            return Err("style object spreads do not expose closed property names");
        };
        if property.computed || property.kind != PropertyKind::Init {
            return Err("computed style keys and accessors do not expose closed property names");
        }
        let authored_name = match &property.key {
            PropertyKey::StaticIdentifier(identifier) => identifier.name.as_str(),
            PropertyKey::StringLiteral(literal) => literal.value.as_str(),
            _ => return Err("style property keys must be identifiers or string literals"),
        };
        let name = css_property_name(authored_name);
        if name.is_empty() || !names.insert(name.clone()) {
            return Err("style property names must be non-empty and unique");
        }
        let span = property.value.span();
        let value = source
            .get(span.start as usize..span.end as usize)
            .ok_or("the style value span did not map back to its expression")?;
        properties.push(StyleProperty {
            name,
            expression: value.to_owned(),
        });
    }
    Ok(properties)
}

fn css_property_name(authored: &str) -> String {
    if authored.starts_with("--") {
        return authored.to_owned();
    }
    let mut css = String::with_capacity(authored.len());
    for character in authored.chars() {
        if character.is_ascii_uppercase() {
            if !css.is_empty() {
                css.push('-');
            }
            css.push(character.to_ascii_lowercase());
        } else {
            css.push(character);
        }
    }
    css
}

fn expression_value(value: &SimpleExpressionNode<'_>) -> String {
    if value.is_static {
        serde_json::to_string(value.content.as_str()).expect("template text is JSON serializable")
    } else {
        value.content.to_string()
    }
}

fn text_expression(values: &[vize_carton::Box<'_, SimpleExpressionNode<'_>>]) -> String {
    if values.is_empty() {
        return "\"\"".to_owned();
    }
    values
        .iter()
        .map(|value| {
            if value.is_static {
                serde_json::to_string(value.content.as_str())
                    .expect("template text is JSON serializable")
            } else {
                format!("__displayValue({})", value.content)
            }
        })
        .collect::<Vec<_>>()
        .join(" + ")
}

fn expression_content(expression: &ExpressionNode<'_>) -> String {
    match expression {
        ExpressionNode::Simple(simple) => simple.content.to_string(),
        ExpressionNode::Compound(compound) => compound.loc.source.to_string(),
    }
}

fn contains_interpolation(children: &[TemplateChildNode<'_>], location: &SourceLocation) -> bool {
    let target = (location.start.offset, location.end.offset);
    children.iter().any(|child| match child {
        TemplateChildNode::Interpolation(interpolation) => {
            let loc = interpolation.content.loc();
            (loc.start.offset, loc.end.offset) == target
                || (interpolation.loc.start.offset, interpolation.loc.end.offset) == target
        }
        TemplateChildNode::Element(element) => contains_interpolation(&element.children, location),
        TemplateChildNode::If(node) => node
            .branches
            .iter()
            .any(|branch| contains_interpolation(&branch.children, location)),
        TemplateChildNode::For(node) => contains_interpolation(&node.children, location),
        _ => false,
    })
}

fn find_if<'a>(
    children: &'a [TemplateChildNode<'a>],
    location: &SourceLocation,
) -> Option<&'a IfNode<'a>> {
    let target = (location.start.offset, location.end.offset);
    for child in children {
        match child {
            TemplateChildNode::If(node)
                if (node.loc.start.offset, node.loc.end.offset) == target =>
            {
                return Some(node);
            }
            TemplateChildNode::Element(element) => {
                if let Some(found) = find_if(&element.children, location) {
                    return Some(found);
                }
            }
            TemplateChildNode::If(node) => {
                for branch in node.branches.iter() {
                    if let Some(found) = find_if(&branch.children, location) {
                        return Some(found);
                    }
                }
            }
            TemplateChildNode::For(node) => {
                if let Some(found) = find_if(&node.children, location) {
                    return Some(found);
                }
            }
            _ => {}
        }
    }
    None
}

fn find_for<'a>(
    children: &'a [TemplateChildNode<'a>],
    location: &SourceLocation,
) -> Option<&'a ForNode<'a>> {
    let target = (location.start.offset, location.end.offset);
    for child in children {
        match child {
            TemplateChildNode::For(node)
                if (node.loc.start.offset, node.loc.end.offset) == target =>
            {
                return Some(node);
            }
            TemplateChildNode::Element(element) => {
                if let Some(found) = find_for(&element.children, location) {
                    return Some(found);
                }
            }
            TemplateChildNode::If(node) => {
                for branch in node.branches.iter() {
                    if let Some(found) = find_for(&branch.children, location) {
                        return Some(found);
                    }
                }
            }
            TemplateChildNode::For(node) => {
                if let Some(found) = find_for(&node.children, location) {
                    return Some(found);
                }
            }
            _ => {}
        }
    }
    None
}

fn find_element<'a>(
    children: &'a [TemplateChildNode<'a>],
    location: &SourceLocation,
) -> Option<&'a ElementNode<'a>> {
    let target = (location.start.offset, location.end.offset);
    for child in children {
        match child {
            TemplateChildNode::Element(element) => {
                if (element.loc.start.offset, element.loc.end.offset) == target {
                    return Some(element);
                }
                if let Some(found) = find_element(&element.children, location) {
                    return Some(found);
                }
            }
            TemplateChildNode::If(node) => {
                for branch in node.branches.iter() {
                    if let Some(found) = find_element(&branch.children, location) {
                        return Some(found);
                    }
                }
            }
            TemplateChildNode::For(node) => {
                if let Some(found) = find_element(&node.children, location) {
                    return Some(found);
                }
            }
            _ => {}
        }
    }
    None
}

fn find_slot_children<'a>(
    component: &'a ElementNode<'a>,
    name: &str,
) -> Option<&'a [TemplateChildNode<'a>]> {
    if component.props.iter().any(|prop| {
        matches!(prop, PropNode::Directive(directive)
            if directive.name.as_str() == "slot" && static_slot_directive_name(directive) == name)
    }) {
        return Some(&component.children);
    }
    if let Some(element) = find_explicit_slot_element(component, name) {
        return Some(&element.children);
    }
    let has_explicit = component.children.iter().any(|child| {
        let TemplateChildNode::Element(element) = child else {
            return false;
        };
        element.tag_type == ElementType::Template
            && element.props.iter().any(|prop| {
                matches!(prop, PropNode::Directive(directive) if directive.name.as_str() == "slot")
            })
    });
    if name == "default" && !has_explicit {
        Some(&component.children)
    } else {
        None
    }
}

fn find_explicit_slot_element<'a>(
    component: &'a ElementNode<'a>,
    name: &str,
) -> Option<&'a ElementNode<'a>> {
    fn search<'a>(
        children: &'a [TemplateChildNode<'a>],
        name: &str,
    ) -> Option<&'a ElementNode<'a>> {
        for child in children {
            match child {
                TemplateChildNode::Element(element)
                    if element.tag_type == ElementType::Template
                        && element.props.iter().any(|prop| {
                            matches!(prop, PropNode::Directive(directive)
                                if directive.name.as_str() == "slot"
                                    && static_slot_directive_name(directive) == name)
                        }) =>
                {
                    return Some(element);
                }
                TemplateChildNode::If(node) => {
                    for branch in node.branches.iter() {
                        if let Some(element) = search(&branch.children, name) {
                            return Some(element);
                        }
                    }
                }
                TemplateChildNode::For(node) => {
                    if let Some(element) = search(&node.children, name) {
                        return Some(element);
                    }
                }
                _ => {}
            }
        }
        None
    }
    search(&component.children, name)
}

fn find_slot_condition<'a>(
    component: &'a ElementNode<'a>,
    name: &str,
) -> Option<&'a ExpressionNode<'a>> {
    fn search<'a>(
        children: &'a [TemplateChildNode<'a>],
        name: &str,
        inherited: Option<&'a ExpressionNode<'a>>,
    ) -> Option<&'a ExpressionNode<'a>> {
        for child in children {
            match child {
                TemplateChildNode::Element(element)
                    if element.tag_type == ElementType::Template
                        && element.props.iter().any(|prop| {
                            matches!(prop, PropNode::Directive(directive)
                                if directive.name.as_str() == "slot"
                                    && static_slot_directive_name(directive) == name)
                        }) =>
                {
                    return inherited.or_else(|| slot_if_expression(element));
                }
                TemplateChildNode::If(node) => {
                    for branch in node.branches.iter() {
                        if let Some(condition) = search(
                            &branch.children,
                            name,
                            branch.condition.as_ref().or(inherited),
                        ) {
                            return Some(condition);
                        }
                    }
                }
                TemplateChildNode::For(node) => {
                    if let Some(condition) = search(&node.children, name, inherited) {
                        return Some(condition);
                    }
                }
                _ => {}
            }
        }
        None
    }
    search(&component.children, name, None)
}

fn slot_if_expression<'a>(element: &'a ElementNode<'a>) -> Option<&'a ExpressionNode<'a>> {
    element.props.iter().find_map(|prop| {
        let PropNode::Directive(directive) = prop else {
            return None;
        };
        (directive.name.as_str() == "if")
            .then_some(directive.exp.as_ref())
            .flatten()
    })
}

fn static_slot_directive_name(directive: &vize_atelier_core::DirectiveNode<'_>) -> String {
    match directive.arg.as_ref() {
        Some(ExpressionNode::Simple(argument)) if argument.is_static => {
            argument.content.to_string()
        }
        _ => "default".to_owned(),
    }
}

fn slot_aliases(pattern: &str, source: &str) -> BTreeMap<String, String> {
    let mut aliases = BTreeMap::new();
    let trimmed = pattern.trim();
    if is_simple_binding_identifier(trimmed) {
        aliases.insert(trimmed.to_owned(), source.to_owned());
        return aliases;
    }
    let object = trimmed
        .strip_prefix('{')
        .and_then(|value| value.strip_suffix('}'))
        .expect("preflight accepts only identifier or flat object slot bindings");
    for field in object
        .split(',')
        .map(str::trim)
        .filter(|field| !field.is_empty())
    {
        let field = field.split('=').next().unwrap_or(field).trim();
        let (property, local) = field
            .split_once(':')
            .map_or((field, field), |(property, local)| {
                (property.trim(), local.trim())
            });
        aliases.insert(local.to_owned(), format!("{source}[{}]", json(property)));
    }
    aliases
}

fn json(value: &str) -> String {
    serde_json::to_string(value).expect("JavaScript strings are JSON serializable")
}

fn camelize_prop(value: &str) -> String {
    let mut result = String::with_capacity(value.len());
    let mut upper = false;
    for character in value.chars() {
        if character == '-' {
            upper = true;
        } else if upper {
            result.extend(character.to_uppercase());
            upper = false;
        } else {
            result.push(character);
        }
    }
    result
}

fn modifier_bits(modifiers: &vize_atelier_vapor::EventModifiers) -> u32 {
    let mut bits = 0u32;
    for modifier in &modifiers.non_keys {
        bits |= match modifier.as_str() {
            "stop" => 1 << 0,
            "prevent" => 1 << 1,
            "self" => 1 << 2,
            "ctrl" => 1 << 3,
            "shift" => 1 << 4,
            "alt" => 1 << 5,
            "meta" => 1 << 6,
            "exact" => 1 << 7,
            "left" => 1 << 8,
            "middle" => 1 << 9,
            "right" => 1 << 10,
            _ => 0,
        };
    }
    if modifiers.options.once {
        bits |= 1 << 11;
    }
    if modifiers.options.capture {
        bits |= 1 << 12;
    }
    if modifiers.options.passive {
        bits |= 1 << 13;
    }
    bits
}

fn is_property(tag: &str, key: &str) -> bool {
    matches!(
        key,
        "value" | "checked" | "selected" | "disabled" | "multiple" | "readonly" | "textContent"
    ) || (tag == "input" && matches!(key, "indeterminate"))
}

fn accepted_for_property(key: &str) -> AcceptedValueKind {
    if matches!(
        key,
        "checked" | "selected" | "disabled" | "multiple" | "readonly" | "indeterminate"
    ) {
        AcceptedValueKind::Bool
    } else {
        AcceptedValueKind::Any
    }
}

fn marker(kind: &str, id: u32) -> String {
    format!("syrinx:v1:{kind}:{id}")
}

fn component_name_from_filename(filename: &str) -> String {
    let stem = filename
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(filename)
        .strip_suffix(".vue")
        .unwrap_or(filename);
    if stem.is_empty() {
        "SyrinxComponent".to_owned()
    } else {
        stem.to_owned()
    }
}

fn canonical_json<T: serde::Serialize>(value: &T) -> String {
    let mut json = serde_json::to_string_pretty(value).expect("artifact model is serializable");
    json.push('\n');
    json
}

fn sha256(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut result = String::with_capacity(64);
    for byte in digest {
        write!(result, "{byte:02x}").unwrap();
    }
    result
}

fn sfc_diagnostic(
    source: &str,
    filename: &str,
    error: vize_atelier_sfc::SfcError,
) -> SyrinxDiagnostic {
    let (start, end) = error
        .loc
        .as_ref()
        .map(|loc| (loc.start, loc.end))
        .unwrap_or((0, source.len()));
    file_diagnostic(
        source,
        filename,
        error.code.as_deref().unwrap_or("SYRINX_SFC_PARSE_ERROR"),
        error.message.as_str(),
        start,
        end,
    )
}

fn template_diagnostic(
    source: &str,
    filename: &str,
    block: &vize_atelier_sfc::BlockLocation,
    code: &str,
    message: &str,
    location: Option<&SourceLocation>,
) -> SyrinxDiagnostic {
    location.map_or_else(
        || file_diagnostic(source, filename, code, message, block.start, block.end),
        |location| location_diagnostic(source, filename, block, code, message, location),
    )
}

fn location_diagnostic(
    source: &str,
    filename: &str,
    block: &vize_atelier_sfc::BlockLocation,
    code: &str,
    message: &str,
    location: &SourceLocation,
) -> SyrinxDiagnostic {
    file_diagnostic(
        source,
        filename,
        code,
        message,
        block.start + location.start.offset as usize,
        block.start + location.end.offset as usize,
    )
}

fn file_diagnostic(
    source: &str,
    filename: &str,
    code: &str,
    message: &str,
    start: usize,
    end: usize,
) -> SyrinxDiagnostic {
    let start = start.min(source.len());
    let end = end.min(source.len());
    let (start_line, start_column) = line_column(source, start);
    let (end_line, end_column) = line_column(source, end);
    SyrinxDiagnostic {
        code: code.to_owned(),
        message: message.to_owned(),
        source: filename.to_owned(),
        start_byte: start as u32,
        end_byte: end as u32,
        start_line,
        start_column,
        end_line,
        end_column,
    }
}

fn line_column(source: &str, offset: usize) -> (u32, u32) {
    let offset = offset.min(source.len());
    let prefix = &source[..offset];
    let line = prefix.bytes().filter(|byte| *byte == b'\n').count() as u32 + 1;
    let column = prefix
        .rfind('\n')
        .map_or(offset, |newline| offset.saturating_sub(newline + 1)) as u32
        + 1;
    (line, column)
}

#[cfg(test)]
mod tests {
    use super::rewrite_alias_references;
    use std::collections::BTreeMap;

    #[test]
    fn alias_rewrite_expands_shorthand_and_respects_nested_parameters() {
        let aliases = BTreeMap::from([("row".to_owned(), "__row.value".to_owned())]);
        assert_eq!(
            rewrite_alias_references("{ row, nested: values.map(row => ({ row })) }", &aliases),
            "{ row: __row.value, nested: values.map(row => ({ row })) }"
        );
    }
}
