//! Serializable, renderer-neutral Syrinx compiler artifacts.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Semantic protocol version required by a generated bundle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProtocolVersion {
    pub major: u16,
    pub minor: u16,
    pub patch: u16,
}

/// Compiler-requested ceilings checked by the host before mount.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProtocolLimits {
    pub max_batch_operations: u32,
    pub max_components: u32,
    pub max_sites_per_component: u32,
    pub max_list_items: u32,
    pub max_string_bytes: u32,
    pub max_blob_bytes: u32,
    pub max_live_instances: u32,
    pub max_scopes_per_instance: u32,
    pub max_pending_requests: u32,
    pub max_component_depth: u32,
    pub max_value_nodes: u32,
    pub max_value_depth: u32,
    pub max_templates: u32,
    pub max_source_spans: u32,
    pub max_stylesheets: u32,
    pub max_string_entries: u32,
}

impl Default for ProtocolLimits {
    fn default() -> Self {
        Self {
            max_batch_operations: 65_536,
            max_components: 4_096,
            max_sites_per_component: 65_536,
            max_list_items: 1_048_576,
            max_string_bytes: 16_777_216,
            max_blob_bytes: 67_108_864,
            max_live_instances: 65_536,
            max_scopes_per_instance: 1_048_576,
            max_pending_requests: 65_536,
            max_component_depth: 256,
            max_value_nodes: 1_048_576,
            max_value_depth: 64,
            max_templates: 65_536,
            max_source_spans: 1_048_576,
            max_stylesheets: 4_096,
            max_string_entries: 1_048_576,
        }
    }
}

/// One exact source range in the original SFC.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SourceSpan {
    pub id: u32,
    pub source: String,
    pub start_byte: u32,
    pub end_byte: u32,
    pub start_line: u32,
    pub start_column: u32,
    pub end_line: u32,
    pub end_column: u32,
}

/// Immutable markup parsed and owned by the Rust host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TemplateDefinition {
    pub id: u32,
    pub markup: String,
    pub source_span: u32,
}

/// Host mutation category for one reactive output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum BindingKind {
    Text,
    Class,
    Attribute,
    Style,
    Property,
}

/// Value-shape authorization emitted beside one binding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum AcceptedValueKind {
    Any,
    Text,
    Bool,
    Signed,
    Unsigned,
    Float,
    Bytes,
    List,
    Record,
}

/// One compiler-declared reactive output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BindingSite {
    pub id: u32,
    pub template: u32,
    pub marker: String,
    pub name: Option<String>,
    pub kind: BindingKind,
    pub accepted_value: AcceptedValueKind,
    pub source_span: u32,
}

/// One compiler-declared normalized event listener.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HandlerSite {
    pub id: u32,
    pub template: u32,
    pub marker: String,
    pub event_name: String,
    pub modifier_bits: u32,
    pub source_span: u32,
}

/// One immutable branch of a conditional site.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BranchDefinition {
    pub index: u16,
    pub template: u32,
}

/// One `v-if`/`v-else-if`/`v-else` site.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IfSite {
    pub id: u32,
    pub template: u32,
    pub marker: String,
    pub branches: Vec<BranchDefinition>,
    pub source_span: u32,
}

/// One keyed `v-for` site.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KeyedListSite {
    pub id: u32,
    pub template: u32,
    pub marker: String,
    pub item_template: u32,
    pub source_span: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChildSite {
    pub id: u32,
    pub template: u32,
    pub marker: String,
    pub child_definition: u32,
    pub source_span: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SlotDefinition {
    pub id: u32,
    pub name: String,
    pub template: u32,
    pub source_span: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SlotOutlet {
    pub id: u32,
    pub template: u32,
    pub marker: String,
    pub name: String,
    pub fallback_template: Option<u32>,
    pub source_span: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CapabilityTarget {
    pub id: u32,
    pub template: u32,
    pub marker: String,
    pub allowed_query_bits: u64,
    pub allowed_action_bits: u64,
    pub source_span: u32,
}

/// Root component topology consumed by the host before guest mount.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ComponentDefinition {
    pub id: u32,
    pub name: String,
    pub template: u32,
    pub bindings: Vec<BindingSite>,
    pub handlers: Vec<HandlerSite>,
    pub if_sites: Vec<IfSite>,
    pub list_sites: Vec<KeyedListSite>,
    pub child_sites: Vec<ChildSite>,
    pub slot_definitions: Vec<SlotDefinition>,
    pub slot_outlets: Vec<SlotOutlet>,
    pub capability_targets: Vec<CapabilityTarget>,
    pub lifecycle_bits: u32,
    pub source_span: u32,
}

/// One scoped or global stylesheet.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Stylesheet {
    pub id: u32,
    pub owner_component: u32,
    pub scope_token: Option<String>,
    pub css: String,
    pub source_span: u32,
}

/// Static ComponentPlan artifact. No expression carries a node identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ComponentPlanV1 {
    pub format: String,
    pub protocol: ProtocolVersion,
    pub compiler_name: String,
    pub compiler_version: String,
    pub schema_sha256: String,
    pub guest_module_sha256: String,
    pub required_capability_bits: u64,
    pub limits: ProtocolLimits,
    pub root_component: u32,
    pub source_spans: Vec<SourceSpan>,
    pub templates: Vec<TemplateDefinition>,
    pub components: Vec<ComponentDefinition>,
    pub stylesheets: Vec<Stylesheet>,
}

/// Content hashes and exact runtime/compiler pairing constraints.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SyrinxManifestV1 {
    pub format: String,
    pub component_id: u32,
    pub component_name: String,
    pub protocol: ProtocolVersion,
    pub protocol_schema_sha256: String,
    pub compiler_name: String,
    pub compiler_version: String,
    pub vize_version: String,
    pub vize_revision: String,
    pub vue_reactivity_version: String,
    pub guest_runtime_module: String,
    pub guest_runtime_version: String,
    pub guest_abi_version: u32,
    pub required_capability_bits: u64,
    pub limits: ProtocolLimits,
    pub source_sha256: String,
    /// Compiler-owned string IDs used by replay input replacement.
    pub input_names: BTreeMap<u32, String>,
    pub artifact_sha256: BTreeMap<String, String>,
}

/// Generated-to-authored linkage retained independently from the plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SyrinxSourceMapV1 {
    pub format: String,
    pub source: String,
    pub source_sha256: String,
    pub spans: Vec<SourceSpan>,
    pub binding_spans: BTreeMap<u32, u32>,
    pub handler_spans: BTreeMap<u32, u32>,
    pub if_spans: BTreeMap<u32, u32>,
    pub list_spans: BTreeMap<u32, u32>,
    /// Authored spans for expressions retained in `guest.mjs`, keyed by a
    /// stable logical role such as `binding:3:read` or `list:1:key`.
    pub guest_expression_spans: BTreeMap<String, u32>,
}

/// The five deterministic files emitted for one ordinary SFC.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyrinxArtifacts {
    pub plan: ComponentPlanV1,
    pub manifest: SyrinxManifestV1,
    pub source_map: SyrinxSourceMapV1,
    pub plan_json: String,
    pub guest_module: String,
    pub css: String,
    pub manifest_json: String,
    pub source_map_json: String,
}

impl SyrinxArtifacts {
    /// Canonical output names and byte-deterministic contents for artifact writers.
    pub fn files(&self) -> [(&'static str, &str); 5] {
        [
            ("component.plan.json", self.plan_json.as_str()),
            ("guest.mjs", self.guest_module.as_str()),
            ("style.css", self.css.as_str()),
            ("manifest.json", self.manifest_json.as_str()),
            ("source-map.json", self.source_map_json.as_str()),
        ]
    }
}
