//! Fail-closed coverage measurement for the v3b Vue-to-RSX compiler target.
//!
//! This is a classifier, not a second compiler. It asks the renderer-native
//! RSX frontend for the exact authored dynamic sites, parses `<script
//! setup>` with OXC, and reports whether each site fits the initial Rust
//! expression subset or must be rejected. The deterministic report is
//! intended to be committed with the SFC it measures.

use std::collections::{BTreeMap, BTreeSet};

use oxc_allocator::Allocator;
use oxc_ast::ast::{BindingPattern, CallExpression, Expression, Statement};
use oxc_ast_visit::{
    Visit,
    walk::{
        walk_await_expression, walk_call_expression, walk_new_expression, walk_yield_expression,
    },
};
use oxc_parser::Parser;
use oxc_span::{GetSpan, SourceType};
use oxc_syntax::scope::ScopeFlags;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use vize_atelier_sfc::{SfcParseOptions, parse_sfc};

use crate::{SyrinxCompileFailure, SyrinxDiagnostic, SyrinxRsxOptions, compile_syrinx_rsx};

const FORMAT: &str = "syrinx-v3b-rsx-coverage-v2";

/// Result for one authored setup binding or compiler-emitted dynamic site.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RsxCoverageClass {
    CompiledRust,
    Rejected,
}

/// Where an authored site came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RsxCoverageSiteKind {
    ScriptBinding,
    TemplateExpression,
}

/// One source-located classification result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RsxCoverageSite {
    pub kind: RsxCoverageSiteKind,
    pub role: String,
    pub expression: String,
    pub classification: RsxCoverageClass,
    pub reason: String,
    pub dependencies: Vec<String>,
    /// One unit means one render-time expression. Setup and event-only sites
    /// are zero because they do not execute while constructing the render tree.
    pub render_weight: u32,
    pub start_byte: u32,
    pub end_byte: u32,
    pub start_line: u32,
    pub start_column: u32,
    pub end_line: u32,
    pub end_column: u32,
}

/// Aggregate counts for native Rust compilation and explicit rejection.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RsxCoverageSummary {
    pub authored_sites: u32,
    pub compiled_sites: u32,
    pub rejected_sites: u32,
    pub render_sites: u32,
    pub compiled_render_sites: u32,
    pub rejected_render_sites: u32,
    pub authored_compiled_percent: f64,
    pub render_compiled_percent: f64,
}

/// Stable, machine-readable result of v3b native-lowering classification.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RsxCoverageReport {
    pub format: String,
    pub source: String,
    pub source_sha256: String,
    pub compiler_version: String,
    pub compiler_revision: String,
    pub classifier_revision: String,
    pub summary: RsxCoverageSummary,
    pub sites: Vec<RsxCoverageSite>,
}

#[derive(Debug, Clone)]
struct BindingAnalysis {
    site: RsxCoverageSite,
    reactive: bool,
    features: Features,
}

#[derive(Debug, Clone, Default)]
struct Features {
    identifiers: BTreeSet<String>,
    calls: BTreeSet<String>,
    unsupported: BTreeSet<String>,
}

#[derive(Default)]
struct FeatureVisitor {
    features: Features,
}

impl<'a> Visit<'a> for FeatureVisitor {
    fn visit_identifier_reference(&mut self, identifier: &oxc_ast::ast::IdentifierReference<'a>) {
        self.features
            .identifiers
            .insert(identifier.name.to_string());
    }

    fn visit_call_expression(&mut self, call: &CallExpression<'a>) {
        self.features.calls.insert(call_name(&call.callee));
        walk_call_expression(self, call);
    }

    fn visit_await_expression(&mut self, expression: &oxc_ast::ast::AwaitExpression<'a>) {
        self.features.unsupported.insert("await".to_owned());
        walk_await_expression(self, expression);
    }

    fn visit_new_expression(&mut self, expression: &oxc_ast::ast::NewExpression<'a>) {
        self.features.unsupported.insert("new".to_owned());
        walk_new_expression(self, expression);
    }

    fn visit_yield_expression(&mut self, expression: &oxc_ast::ast::YieldExpression<'a>) {
        self.features.unsupported.insert("yield".to_owned());
        walk_yield_expression(self, expression);
    }
}

/// Measure whether one ordinary Vue SFC is a viable v3b compiler target.
///
/// The renderer-native RSX frontend remains authoritative for SFC/template
/// parsing and source spans. A source outside that profile fails rather than
/// being assigned an optimistic coverage ratio.
pub fn measure_rsx_coverage(
    source: &str,
    options: SyrinxRsxOptions,
) -> Result<RsxCoverageReport, SyrinxCompileFailure> {
    let filename = options.filename.clone();
    let compiler_revision = options.compiler_revision.clone();
    let artifact = compile_syrinx_rsx(source, options)?;
    let descriptor = parse_sfc(
        source,
        SfcParseOptions {
            filename: filename.clone().into(),
            source_map: true,
            ..Default::default()
        },
    )
    .map_err(|error| coverage_sfc_failure(source, &filename, error))?;

    let mut bindings = analyze_bindings(source, &descriptor);
    classify_bindings(&mut bindings);
    let template_identifiers = artifact
        .expression_hooks
        .iter()
        .flat_map(|hook| expression_features(&hook.expression).identifiers)
        .collect::<BTreeSet<_>>();
    for binding in &mut bindings {
        let role = binding.site.role.as_str();
        let is_external_props = binding.site.expression.starts_with("defineProps");
        if binding.reactive
            && template_identifiers.contains(role)
            && !is_external_props
            && !artifact.compiled_bindings.iter().any(|name| name == role)
        {
            binding.site.classification = RsxCoverageClass::Rejected;
            binding.site.reason =
                "reactive binding has no native Rust lowering for this authored expression"
                    .to_owned();
        }
    }
    let binding_classes = bindings
        .iter()
        .map(|binding| (binding.site.role.clone(), binding.site.classification))
        .collect::<BTreeMap<_, _>>();
    let external_bindings = bindings
        .iter()
        .filter(|binding| binding.site.expression.starts_with("defineProps"))
        .map(|binding| binding.site.role.clone())
        .collect::<BTreeSet<_>>();
    let known_bindings = binding_classes.keys().cloned().collect::<BTreeSet<_>>();
    let mut sites = bindings
        .into_iter()
        .map(|binding| binding.site)
        .collect::<Vec<_>>();
    for hook in &artifact.expression_hooks {
        let expression = hook.expression.trim().to_owned();
        let features = expression_features(&expression);
        let (mut classification, mut reason, dependencies) =
            classify_features(&features, &known_bindings, &binding_classes);
        if classification == RsxCoverageClass::Rejected
            && !hook.field.starts_with("native_")
            && !dependencies.is_empty()
            && dependencies
                .iter()
                .all(|dependency| external_bindings.contains(dependency))
        {
            classification = RsxCoverageClass::CompiledRust;
            reason = "external reactive input is an explicit typed render-model field".to_owned();
        }
        sites.push(RsxCoverageSite {
            kind: RsxCoverageSiteKind::TemplateExpression,
            role: hook.role.clone(),
            expression,
            classification,
            reason,
            dependencies,
            render_weight: render_weight(&hook.role),
            start_byte: hook.start_byte,
            end_byte: hook.end_byte,
            start_line: line_column(source, hook.start_byte as usize).0,
            start_column: line_column(source, hook.start_byte as usize).1,
            end_line: line_column(source, hook.end_byte as usize).0,
            end_column: line_column(source, hook.end_byte as usize).1,
        });
    }
    sites.sort_by(|left, right| {
        left.start_byte
            .cmp(&right.start_byte)
            .then_with(|| site_kind_order(left.kind).cmp(&site_kind_order(right.kind)))
            .then_with(|| left.role.cmp(&right.role))
    });

    let summary = summarize(&sites);
    Ok(RsxCoverageReport {
        format: FORMAT.to_owned(),
        source: filename,
        source_sha256: sha256(source.as_bytes()),
        compiler_version: env!("CARGO_PKG_VERSION").to_owned(),
        compiler_revision,
        classifier_revision: "library-call".to_owned(),
        summary,
        sites,
    })
}

fn analyze_bindings(
    source: &str,
    descriptor: &vize_atelier_sfc::SfcDescriptor<'_>,
) -> Vec<BindingAnalysis> {
    let Some(script) = descriptor.script_setup.as_ref() else {
        return Vec::new();
    };
    let content = script.content.as_ref();
    let source_type = match script.lang.as_deref() {
        Some("ts") => SourceType::ts(),
        Some("tsx") => SourceType::tsx(),
        Some("jsx") => SourceType::jsx(),
        _ => SourceType::default(),
    }
    .with_module(true);
    let allocator = Allocator::default();
    let parsed = Parser::new(&allocator, content, source_type).parse();
    debug_assert!(!parsed.panicked && parsed.diagnostics.is_empty());
    let offset = script.loc.start;
    let mut bindings = Vec::new();
    for statement in &parsed.program.body {
        match statement {
            Statement::VariableDeclaration(declaration) => {
                for declarator in &declaration.declarations {
                    let BindingPattern::BindingIdentifier(identifier) = &declarator.id else {
                        continue;
                    };
                    let Some(initializer) = declarator.init.as_ref() else {
                        continue;
                    };
                    let span = initializer.span();
                    let start = offset + span.start as usize;
                    let end = offset + span.end as usize;
                    let expression = content
                        .get(span.start as usize..span.end as usize)
                        .unwrap_or("")
                        .trim()
                        .to_owned();
                    let features = visit_expression(initializer);
                    let reactive = features
                        .calls
                        .iter()
                        .any(|call| matches!(call.as_str(), "defineProps" | "ref" | "computed"));
                    bindings.push(BindingAnalysis {
                        site: source_site(source, identifier.name.as_str(), expression, start, end),
                        reactive,
                        features,
                    });
                }
            }
            Statement::FunctionDeclaration(function) => {
                let Some(identifier) = function.id.as_ref() else {
                    continue;
                };
                let mut visitor = FeatureVisitor::default();
                visitor.visit_function(function, ScopeFlags::empty());
                let start = offset + function.span.start as usize;
                let end = offset + function.span.end as usize;
                bindings.push(BindingAnalysis {
                    site: source_site(
                        source,
                        identifier.name.as_str(),
                        content
                            .get(function.span.start as usize..function.span.end as usize)
                            .unwrap_or("")
                            .trim()
                            .to_owned(),
                        start,
                        end,
                    ),
                    reactive: false,
                    features: visitor.features,
                });
            }
            _ => {}
        }
    }
    bindings
}

fn classify_bindings(bindings: &mut [BindingAnalysis]) {
    let known = bindings
        .iter()
        .map(|binding| binding.site.role.clone())
        .collect::<BTreeSet<_>>();
    let mut classes = BTreeMap::new();
    for _ in 0..=bindings.len() {
        let before = classes.clone();
        for binding in bindings.iter_mut() {
            let (classification, reason, dependencies) =
                classify_features(&binding.features, &known, &classes);
            binding.site.classification = classification;
            binding.site.reason = reason;
            binding.site.dependencies = dependencies;
            classes.insert(binding.site.role.clone(), classification);
        }
        if classes == before {
            break;
        }
    }
}

fn classify_features(
    features: &Features,
    known_bindings: &BTreeSet<String>,
    binding_classes: &BTreeMap<String, RsxCoverageClass>,
) -> (RsxCoverageClass, String, Vec<String>) {
    let dependencies = features
        .identifiers
        .intersection(known_bindings)
        .cloned()
        .collect::<Vec<_>>();
    let browser_globals = features
        .identifiers
        .iter()
        .filter(|name| {
            matches!(
                name.as_str(),
                "document" | "window" | "globalThis" | "HTMLElement" | "Node"
            ) && !known_bindings.contains(*name)
        })
        .cloned()
        .collect::<Vec<_>>();
    if !browser_globals.is_empty() {
        return (
            RsxCoverageClass::Rejected,
            format!(
                "imperative browser identity is forbidden: {}",
                browser_globals.join(", ")
            ),
            dependencies,
        );
    }
    if dependencies
        .iter()
        .any(|dependency| binding_classes.get(dependency) == Some(&RsxCoverageClass::Rejected))
    {
        return (
            RsxCoverageClass::Rejected,
            "depends on a rejected setup binding".to_owned(),
            dependencies,
        );
    }

    let unknown_calls = features
        .calls
        .iter()
        .filter(|call| {
            !matches!(
                call.as_str(),
                "computed" | "defineEmits" | "defineProps" | "ref"
            ) && !known_bindings.contains(*call)
        })
        .cloned()
        .collect::<Vec<_>>();
    if !features.unsupported.is_empty() || !unknown_calls.is_empty() {
        let mut reasons = Vec::new();
        if !features.unsupported.is_empty() {
            reasons.push(format!(
                "unsupported Rust syntax: {}",
                features
                    .unsupported
                    .iter()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        if !unknown_calls.is_empty() {
            reasons.push(format!("unlowered call: {}", unknown_calls.join(", ")));
        }
        return (RsxCoverageClass::Rejected, reasons.join("; "), dependencies);
    }
    (
        RsxCoverageClass::CompiledRust,
        "inside the initial Rust expression and control-flow subset".to_owned(),
        dependencies,
    )
}

fn visit_expression(expression: &Expression<'_>) -> Features {
    let mut visitor = FeatureVisitor::default();
    visitor.visit_expression(expression);
    visitor.features
}

fn expression_features(source: &str) -> Features {
    let allocator = Allocator::default();
    let parser = Parser::new(&allocator, source, SourceType::ts().with_module(true));
    if let Ok(expression) = parser.parse_expression() {
        return visit_expression(&expression);
    }
    let allocator = Allocator::default();
    let parsed = Parser::new(&allocator, source, SourceType::ts().with_module(true)).parse();
    if parsed.panicked || !parsed.diagnostics.is_empty() {
        return Features {
            unsupported: BTreeSet::from(["unparseable expression".to_owned()]),
            ..Default::default()
        };
    }
    let mut visitor = FeatureVisitor::default();
    visitor.visit_program(&parsed.program);
    visitor.features
}

fn call_name(callee: &Expression<'_>) -> String {
    match callee {
        Expression::Identifier(identifier) => identifier.name.to_string(),
        Expression::StaticMemberExpression(member) => {
            format!(
                "{}.{}",
                expression_root(&member.object),
                member.property.name
            )
        }
        Expression::ComputedMemberExpression(member) => {
            format!("{}.[computed]", expression_root(&member.object))
        }
        _ => "<dynamic>".to_owned(),
    }
}

fn expression_root<'a>(expression: &'a Expression<'a>) -> &'a str {
    match expression {
        Expression::Identifier(identifier) => identifier.name.as_str(),
        _ => "<expression>",
    }
}

fn source_site(
    source: &str,
    role: &str,
    expression: String,
    start: usize,
    end: usize,
) -> RsxCoverageSite {
    let (start_line, start_column) = line_column(source, start);
    let (end_line, end_column) = line_column(source, end);
    RsxCoverageSite {
        kind: RsxCoverageSiteKind::ScriptBinding,
        role: role.to_owned(),
        expression,
        classification: RsxCoverageClass::CompiledRust,
        reason: String::new(),
        dependencies: Vec::new(),
        render_weight: 0,
        start_byte: start as u32,
        end_byte: end as u32,
        start_line,
        start_column,
        end_line,
        end_column,
    }
}

fn render_weight(role: &str) -> u32 {
    u32::from(
        matches!(
            role,
            "text" | "condition" | "list" | "slot" | "style" | "dynamic-attributes"
        ) || role.starts_with("attribute:")
            || role.starts_with("binding:")
            || role.starts_with("if:")
            || role.starts_with("list:")
            || role.starts_with("child:")
            || role.starts_with("slot:"),
    )
}

fn summarize(sites: &[RsxCoverageSite]) -> RsxCoverageSummary {
    let authored_sites = sites.len() as u32;
    let compiled_sites = count_sites(sites, RsxCoverageClass::CompiledRust);
    let rejected_sites = count_sites(sites, RsxCoverageClass::Rejected);
    let render_sites = sites.iter().map(|site| site.render_weight).sum::<u32>();
    let compiled_render_sites = weighted_sites(sites, RsxCoverageClass::CompiledRust);
    let rejected_render_sites = weighted_sites(sites, RsxCoverageClass::Rejected);
    RsxCoverageSummary {
        authored_sites,
        compiled_sites,
        rejected_sites,
        render_sites,
        compiled_render_sites,
        rejected_render_sites,
        authored_compiled_percent: percentage(compiled_sites, authored_sites),
        render_compiled_percent: percentage(compiled_render_sites, render_sites),
    }
}

fn count_sites(sites: &[RsxCoverageSite], class: RsxCoverageClass) -> u32 {
    sites
        .iter()
        .filter(|site| site.classification == class)
        .count() as u32
}

fn weighted_sites(sites: &[RsxCoverageSite], class: RsxCoverageClass) -> u32 {
    sites
        .iter()
        .filter(|site| site.classification == class)
        .map(|site| site.render_weight)
        .sum()
}

fn percentage(numerator: u32, denominator: u32) -> f64 {
    if denominator == 0 {
        0.0
    } else {
        f64::from(numerator) * 100.0 / f64::from(denominator)
    }
}

fn site_kind_order(kind: RsxCoverageSiteKind) -> u8 {
    match kind {
        RsxCoverageSiteKind::ScriptBinding => 0,
        RsxCoverageSiteKind::TemplateExpression => 1,
    }
}

fn coverage_sfc_failure(
    source: &str,
    filename: &str,
    error: vize_atelier_sfc::SfcError,
) -> SyrinxCompileFailure {
    let (start, end) = error
        .loc
        .as_ref()
        .map_or((0, source.len()), |loc| (loc.start, loc.end));
    let (start_line, start_column) = line_column(source, start);
    let (end_line, end_column) = line_column(source, end);
    SyrinxCompileFailure {
        diagnostics: vec![SyrinxDiagnostic {
            code: error.code.map_or_else(
                || "SYRINX_RSX_SFC_PARSE".to_owned(),
                |code| code.to_string(),
            ),
            message: error.message.to_string(),
            source: filename.to_owned(),
            start_byte: start as u32,
            end_byte: end as u32,
            start_line,
            start_column,
            end_line,
            end_column,
        }],
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

fn sha256(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

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
    <tr v-for="(row, index) in props.rows" :key="row.id" @mouseenter="hovered = row.id">
      <td @click="choose(row.id)">{{ metaLine(row, index) }}</td>
    </tr>
    <aside v-if="selected">{{ selected }}</aside>
  </section>
</template>"#;

    fn options() -> SyrinxRsxOptions {
        SyrinxRsxOptions {
            filename: "TimeTravelTable.vue".to_owned(),
            component_name: Some("TimeTravelTable".to_owned()),
            ..Default::default()
        }
    }

    #[test]
    fn table_is_fully_compiled_with_no_rejections() {
        let report = measure_rsx_coverage(TABLE, options()).unwrap();
        assert_eq!(report.summary.render_compiled_percent, 100.0);
        assert_eq!(report.summary.rejected_sites, 0);
        assert!(report.sites.iter().any(|site| site.role == "rootClass"));
        assert!(
            report
                .sites
                .iter()
                .any(|site| site.role == "list" || site.role.starts_with("list:"))
        );
    }

    #[test]
    fn unknown_calls_are_rejected_even_when_they_are_pure() {
        let pure = expression_features("Intl.NumberFormat('en').format(value)");
        let known = BTreeSet::from(["value".to_owned()]);
        let (class, _, dependencies) = classify_features(&pure, &known, &BTreeMap::new());
        assert_eq!(class, RsxCoverageClass::Rejected);
        assert_eq!(dependencies, ["value"]);
    }
}
