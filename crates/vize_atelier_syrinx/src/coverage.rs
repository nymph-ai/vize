//! Fail-closed coverage measurement for the v3b Vue-to-RSX compiler target.
//!
//! This is a classifier, not a second compiler. It asks the existing Vize
//! Syrinx frontend for the exact authored dynamic sites, parses `<script
//! setup>` with OXC, and reports whether each site fits the initial Rust
//! expression subset, needs a pure residual JavaScript function, or must be
//! rejected. The deterministic report is intended to be committed with the
//! SFC it measures.

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

use crate::{SyrinxCompileFailure, SyrinxCompileOptions, compile_syrinx};

const MIN_RENDER_COMPILED_PERCENT: f64 = 80.0;
const FORMAT: &str = "syrinx-v3b-rsx-coverage-v1";

/// Result for one authored setup binding or compiler-emitted dynamic site.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RsxCoverageClass {
    CompiledRust,
    ResidualJs,
    Rejected,
}

/// Where an authored site came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RsxCoverageSiteKind {
    ScriptBinding,
    TemplateExpression,
}

/// GO/KILL output of the mandatory v3b gating experiment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RsxCoverageDecision {
    Proceed,
    Kill,
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
    /// One unit means one possible JS call in a render. Setup and event-only
    /// sites are zero because they do not create per-frame residual cost.
    pub render_weight: u32,
    pub start_byte: u32,
    pub end_byte: u32,
    pub start_line: u32,
    pub start_column: u32,
    pub end_line: u32,
    pub end_column: u32,
}

/// Aggregate counts used for the gate decision.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RsxCoverageSummary {
    pub authored_sites: u32,
    pub compiled_sites: u32,
    pub residual_sites: u32,
    pub rejected_sites: u32,
    pub render_sites: u32,
    pub compiled_render_sites: u32,
    pub residual_render_sites: u32,
    pub rejected_render_sites: u32,
    pub authored_compiled_percent: f64,
    pub render_compiled_percent: f64,
}

/// Stable, machine-readable result of the v3b gating experiment.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RsxCoverageReport {
    pub format: String,
    pub source: String,
    pub source_sha256: String,
    pub compiler_version: String,
    pub compiler_revision: String,
    pub classifier_revision: String,
    pub minimum_render_compiled_percent: f64,
    pub decision: RsxCoverageDecision,
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
/// The existing Syrinx frontend remains authoritative for SFC/template
/// parsing and source spans. A source outside that profile fails rather than
/// being assigned an optimistic coverage ratio.
pub fn measure_rsx_coverage(
    source: &str,
    options: SyrinxCompileOptions,
) -> Result<RsxCoverageReport, SyrinxCompileFailure> {
    let filename = options.filename.clone();
    let artifacts = compile_syrinx(source, options)?;
    let descriptor = parse_sfc(
        source,
        SfcParseOptions {
            filename: filename.clone().into(),
            source_map: true,
            ..Default::default()
        },
    )
    .expect("compile_syrinx already accepted this SFC");

    let mut bindings = analyze_bindings(source, &descriptor);
    classify_bindings(&mut bindings);
    let binding_classes = bindings
        .iter()
        .map(|binding| (binding.site.role.clone(), binding.site.classification))
        .collect::<BTreeMap<_, _>>();
    let known_bindings = binding_classes.keys().cloned().collect::<BTreeSet<_>>();
    let reactive_bindings = bindings
        .iter()
        .filter(|binding| binding.reactive)
        .map(|binding| binding.site.role.clone())
        .collect::<BTreeSet<_>>();

    let spans = artifacts
        .source_map
        .spans
        .iter()
        .map(|span| (span.id, span))
        .collect::<BTreeMap<_, _>>();
    let mut sites = bindings
        .into_iter()
        .map(|binding| binding.site)
        .collect::<Vec<_>>();
    for (role, span_id) in &artifacts.source_map.guest_expression_spans {
        let span = spans
            .get(span_id)
            .expect("every guest expression span must be registered");
        let expression = source
            .get(span.start_byte as usize..span.end_byte as usize)
            .unwrap_or("")
            .trim()
            .to_owned();
        let features = expression_features(&expression);
        let (classification, reason, dependencies) = classify_features(
            &features,
            &known_bindings,
            &reactive_bindings,
            &binding_classes,
            false,
        );
        sites.push(RsxCoverageSite {
            kind: RsxCoverageSiteKind::TemplateExpression,
            role: role.clone(),
            expression,
            classification,
            reason,
            dependencies,
            render_weight: render_weight(role),
            start_byte: span.start_byte,
            end_byte: span.end_byte,
            start_line: span.start_line,
            start_column: span.start_column,
            end_line: span.end_line,
            end_column: span.end_column,
        });
    }
    sites.sort_by(|left, right| {
        left.start_byte
            .cmp(&right.start_byte)
            .then_with(|| site_kind_order(left.kind).cmp(&site_kind_order(right.kind)))
            .then_with(|| left.role.cmp(&right.role))
    });

    let summary = summarize(&sites);
    let decision = if summary.render_sites > 0
        && summary.render_compiled_percent >= MIN_RENDER_COMPILED_PERCENT
        && summary.rejected_sites == 0
    {
        RsxCoverageDecision::Proceed
    } else {
        RsxCoverageDecision::Kill
    };
    Ok(RsxCoverageReport {
        format: FORMAT.to_owned(),
        source: filename,
        source_sha256: sha256(source.as_bytes()),
        compiler_version: env!("CARGO_PKG_VERSION").to_owned(),
        compiler_revision: artifacts.manifest.vize_revision,
        classifier_revision: "library-call".to_owned(),
        minimum_render_compiled_percent: MIN_RENDER_COMPILED_PERCENT,
        decision,
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
    let reactive = bindings
        .iter()
        .filter(|binding| binding.reactive)
        .map(|binding| binding.site.role.clone())
        .collect::<BTreeSet<_>>();
    let mut classes = BTreeMap::new();
    for _ in 0..=bindings.len() {
        let before = classes.clone();
        for binding in bindings.iter_mut() {
            let (classification, reason, dependencies) = classify_features(
                &binding.features,
                &known,
                &reactive,
                &classes,
                binding.reactive,
            );
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
    reactive_bindings: &BTreeSet<String>,
    binding_classes: &BTreeMap<String, RsxCoverageClass>,
    own_reactive: bool,
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
    let residual_dependency = dependencies
        .iter()
        .any(|dependency| binding_classes.get(dependency) == Some(&RsxCoverageClass::ResidualJs));
    let needs_residual =
        !features.unsupported.is_empty() || !unknown_calls.is_empty() || residual_dependency;
    if needs_residual {
        let closes_over_reactive = own_reactive
            || dependencies
                .iter()
                .any(|dependency| reactive_bindings.contains(dependency));
        if closes_over_reactive {
            return (
                RsxCoverageClass::Rejected,
                "residual JavaScript would close over reactive state instead of receiving explicit arguments"
                    .to_owned(),
                dependencies,
            );
        }
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
            reasons.push(format!("unlowered pure call: {}", unknown_calls.join(", ")));
        }
        if residual_dependency {
            reasons.push("depends on a pure residual binding".to_owned());
        }
        return (
            RsxCoverageClass::ResidualJs,
            reasons.join("; "),
            dependencies,
        );
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
    if role.starts_with("binding:")
        || role.starts_with("if:")
        || role.starts_with("list:")
        || role.starts_with("child:")
        || role.starts_with("slot:")
    {
        1
    } else {
        0
    }
}

fn summarize(sites: &[RsxCoverageSite]) -> RsxCoverageSummary {
    let authored_sites = sites.len() as u32;
    let compiled_sites = count_sites(sites, RsxCoverageClass::CompiledRust);
    let residual_sites = count_sites(sites, RsxCoverageClass::ResidualJs);
    let rejected_sites = count_sites(sites, RsxCoverageClass::Rejected);
    let render_sites = sites.iter().map(|site| site.render_weight).sum::<u32>();
    let compiled_render_sites = weighted_sites(sites, RsxCoverageClass::CompiledRust);
    let residual_render_sites = weighted_sites(sites, RsxCoverageClass::ResidualJs);
    let rejected_render_sites = weighted_sites(sites, RsxCoverageClass::Rejected);
    RsxCoverageSummary {
        authored_sites,
        compiled_sites,
        residual_sites,
        rejected_sites,
        render_sites,
        compiled_render_sites,
        residual_render_sites,
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

    fn options() -> SyrinxCompileOptions {
        SyrinxCompileOptions {
            filename: "TimeTravelTable.vue".to_owned(),
            component_name: Some("TimeTravelTable".to_owned()),
            component_id: 100,
            protocol_schema_sha256: "a".repeat(64),
            ..Default::default()
        }
    }

    #[test]
    fn table_is_a_compiled_majority_with_no_residual_render_calls() {
        let report = measure_rsx_coverage(TABLE, options()).unwrap();
        assert_eq!(report.decision, RsxCoverageDecision::Proceed);
        assert_eq!(report.summary.render_compiled_percent, 100.0);
        assert_eq!(report.summary.residual_render_sites, 0);
        assert_eq!(report.summary.rejected_sites, 0);
        assert!(report.sites.iter().any(|site| site.role == "rootClass"));
        assert!(
            report
                .sites
                .iter()
                .any(|site| site.role.starts_with("list:"))
        );
    }

    #[test]
    fn pure_unknown_calls_are_residual_but_reactive_closures_are_rejected() {
        let pure = expression_features("Intl.NumberFormat('en').format(value)");
        let known = BTreeSet::from(["value".to_owned()]);
        let (class, _, dependencies) =
            classify_features(&pure, &known, &BTreeSet::new(), &BTreeMap::new(), false);
        assert_eq!(class, RsxCoverageClass::ResidualJs);
        assert_eq!(dependencies, ["value"]);

        let (class, reason, _) = classify_features(&pure, &known, &known, &BTreeMap::new(), false);
        assert_eq!(class, RsxCoverageClass::Rejected);
        assert!(reason.contains("reactive state"));
    }
}
