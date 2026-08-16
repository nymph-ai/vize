//! Static compiled/residual split for the v3b RSX target.
//!
//! Residual JavaScript is an optional leaf artifact: one pure function per
//! classified site, explicit arguments in and one value out. It does not own
//! state and cannot name renderer nodes.

use std::collections::BTreeSet;
use std::fmt::Write as _;

use oxc_allocator::Allocator;
use oxc_ast_visit::{
    Visit,
    walk::{walk_arrow_function_expression, walk_function, walk_variable_declarator},
};
use oxc_parser::Parser;
use oxc_span::SourceType;
use oxc_syntax::scope::ScopeFlags;
use serde::{Deserialize, Serialize};

use crate::{
    RsxCoverageClass, RsxCoverageReport, RsxCoverageSite, SyrinxCompileFailure,
    SyrinxCompileOptions, SyrinxDiagnostic, SyrinxRsxArtifact, compile_syrinx_rsx,
    measure_rsx_coverage,
};

/// One exported pure residual function and its closed argument list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResidualExport {
    pub name: String,
    pub role: String,
    pub dependencies: Vec<String>,
    pub expression: String,
    pub start_byte: u32,
    pub end_byte: u32,
}

/// Complete v3b compiler output before the browser bindgen wrapper is added.
#[derive(Debug, Clone, PartialEq)]
pub struct SyrinxHybridArtifact {
    pub rsx: SyrinxRsxArtifact,
    pub classification: RsxCoverageReport,
    pub classification_json: String,
    pub residual_exports: Vec<ResidualExport>,
    pub residual_module: String,
}

/// Classify one SFC, emit its RSX artifact, and emit only the required pure
/// residual functions. Rejected sites fail before any partial output exists.
pub fn compile_syrinx_hybrid(
    source: &str,
    options: SyrinxCompileOptions,
) -> Result<SyrinxHybridArtifact, SyrinxCompileFailure> {
    let classification = measure_rsx_coverage(source, options.clone())?;
    let rejected = classification
        .sites
        .iter()
        .filter(|site| site.classification == RsxCoverageClass::Rejected)
        .map(|site| rejected_diagnostic(&classification.source, site))
        .collect::<Vec<_>>();
    if !rejected.is_empty() {
        return Err(SyrinxCompileFailure {
            diagnostics: rejected,
        });
    }

    let rsx = compile_syrinx_rsx(source, options)?;
    let mut residual_exports = Vec::new();
    for (index, site) in classification
        .sites
        .iter()
        .filter(|site| site.classification == RsxCoverageClass::ResidualJs)
        .enumerate()
    {
        let dependencies = explicit_dependencies(&site.expression)
            .map_err(|reason| residual_emit_failure(&classification.source, site, &reason))?;
        residual_exports.push(ResidualExport {
            name: format!(
                "residual_{}_{}",
                javascript_identifier(&site.role),
                index + 1
            ),
            role: site.role.clone(),
            dependencies,
            expression: site.expression.clone(),
            start_byte: site.start_byte,
            end_byte: site.end_byte,
        });
    }
    let residual_module = emit_residual_module(&residual_exports);
    let residual_allocator = Allocator::default();
    let parsed = Parser::new(
        &residual_allocator,
        &residual_module,
        SourceType::default().with_module(true),
    )
    .parse();
    if parsed.panicked || !parsed.diagnostics.is_empty() {
        let site = classification
            .sites
            .iter()
            .find(|site| site.classification == RsxCoverageClass::ResidualJs)
            .expect("a non-empty invalid residual module has a residual site");
        return Err(residual_emit_failure(
            &classification.source,
            site,
            "the classified expression cannot be emitted as synchronous JavaScript",
        ));
    }
    let classification_json = serde_json::to_string_pretty(&classification)
        .expect("the classification report only contains serializable compiler data");
    Ok(SyrinxHybridArtifact {
        rsx,
        classification,
        classification_json,
        residual_exports,
        residual_module,
    })
}

fn residual_emit_failure(
    source: &str,
    site: &RsxCoverageSite,
    reason: &str,
) -> SyrinxCompileFailure {
    SyrinxCompileFailure {
        diagnostics: vec![SyrinxDiagnostic {
            code: "SYRINX_RSX_RESIDUAL_UNEMITTABLE".to_owned(),
            message: format!("'{}' has no closed residual emit: {reason}", site.role),
            source: source.to_owned(),
            start_byte: site.start_byte,
            end_byte: site.end_byte,
            start_line: site.start_line,
            start_column: site.start_column,
            end_line: site.end_line,
            end_column: site.end_column,
        }],
    }
}

fn rejected_diagnostic(source: &str, site: &RsxCoverageSite) -> SyrinxDiagnostic {
    SyrinxDiagnostic {
        code: "SYRINX_RSX_RESIDUAL_REJECTED".to_owned(),
        message: format!(
            "'{}' cannot become a pure residual function: {}",
            site.role, site.reason
        ),
        source: source.to_owned(),
        start_byte: site.start_byte,
        end_byte: site.end_byte,
        start_line: site.start_line,
        start_column: site.start_column,
        end_line: site.end_line,
        end_column: site.end_column,
    }
}

fn emit_residual_module(exports: &[ResidualExport]) -> String {
    let mut module = String::from(
        "// @generated by vize_atelier_syrinx. Pure args-in/value-out residuals only.\n",
    );
    if exports.is_empty() {
        module.push_str("export {};\n");
        return module;
    }
    for export in exports {
        writeln!(
            module,
            "// {}:{}-{}",
            export.role, export.start_byte, export.end_byte
        )
        .unwrap();
        writeln!(
            module,
            "export function {}({}) {{",
            export.name,
            export.dependencies.join(", ")
        )
        .unwrap();
        writeln!(module, "  return ({});", export.expression).unwrap();
        module.push_str("}\n");
    }
    module
}

struct DependencyVisitor {
    names: BTreeSet<String>,
    local_scopes: Vec<BTreeSet<String>>,
}

impl Default for DependencyVisitor {
    fn default() -> Self {
        Self {
            names: BTreeSet::new(),
            local_scopes: vec![BTreeSet::new()],
        }
    }
}

impl DependencyVisitor {
    fn is_local(&self, name: &str) -> bool {
        self.local_scopes
            .iter()
            .rev()
            .any(|scope| scope.contains(name))
    }

    fn add_pattern(&mut self, pattern: &oxc_ast::ast::BindingPattern<'_>) {
        match pattern {
            oxc_ast::ast::BindingPattern::BindingIdentifier(identifier) => {
                self.local_scopes
                    .last_mut()
                    .expect("dependency visitor always has a scope")
                    .insert(identifier.name.to_string());
            }
            oxc_ast::ast::BindingPattern::ObjectPattern(object) => {
                for property in &object.properties {
                    self.add_pattern(&property.value);
                }
                if let Some(rest) = &object.rest {
                    self.add_pattern(&rest.argument);
                }
            }
            oxc_ast::ast::BindingPattern::ArrayPattern(array) => {
                for element in array.elements.iter().flatten() {
                    self.add_pattern(element);
                }
                if let Some(rest) = &array.rest {
                    self.add_pattern(&rest.argument);
                }
            }
            oxc_ast::ast::BindingPattern::AssignmentPattern(assignment) => {
                self.add_pattern(&assignment.left);
            }
        }
    }
}

impl<'a> Visit<'a> for DependencyVisitor {
    fn visit_identifier_reference(&mut self, identifier: &oxc_ast::ast::IdentifierReference<'a>) {
        if !self.is_local(identifier.name.as_str()) {
            self.names.insert(identifier.name.to_string());
        }
    }

    fn visit_arrow_function_expression(
        &mut self,
        arrow: &oxc_ast::ast::ArrowFunctionExpression<'a>,
    ) {
        self.local_scopes.push(BTreeSet::new());
        for parameter in &arrow.params.items {
            self.add_pattern(&parameter.pattern);
        }
        if let Some(rest) = &arrow.params.rest {
            self.add_pattern(&rest.rest.argument);
        }
        walk_arrow_function_expression(self, arrow);
        self.local_scopes.pop();
    }

    fn visit_function(&mut self, function: &oxc_ast::ast::Function<'a>, flags: ScopeFlags) {
        self.local_scopes.push(BTreeSet::new());
        if let Some(identifier) = &function.id {
            self.local_scopes
                .last_mut()
                .expect("function scope exists")
                .insert(identifier.name.to_string());
        }
        for parameter in &function.params.items {
            self.add_pattern(&parameter.pattern);
        }
        if let Some(rest) = &function.params.rest {
            self.add_pattern(&rest.rest.argument);
        }
        walk_function(self, function, flags);
        self.local_scopes.pop();
    }

    fn visit_variable_declarator(&mut self, declarator: &oxc_ast::ast::VariableDeclarator<'a>) {
        walk_variable_declarator(self, declarator);
        self.add_pattern(&declarator.id);
    }
}

fn explicit_dependencies(source: &str) -> Result<Vec<String>, String> {
    let allocator = Allocator::default();
    let parser = Parser::new(&allocator, source, SourceType::ts().with_module(true));
    let expression = parser
        .parse_expression()
        .map_err(|_| "the expression's free variables cannot be enumerated".to_owned())?;
    let mut visitor = DependencyVisitor::default();
    visitor.visit_expression(&expression);
    Ok(visitor
        .names
        .into_iter()
        .filter(|name| !is_pure_global(name))
        .collect())
}

fn is_pure_global(name: &str) -> bool {
    matches!(
        name,
        "Array"
            | "BigInt"
            | "Boolean"
            | "Infinity"
            | "JSON"
            | "Map"
            | "Math"
            | "NaN"
            | "Number"
            | "Object"
            | "RegExp"
            | "Set"
            | "String"
            | "Symbol"
            | "undefined"
    )
}

fn javascript_identifier(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    for (index, character) in value.chars().enumerate() {
        let valid = character.is_ascii_alphanumeric() || matches!(character, '_' | '$');
        if index == 0 && character.is_ascii_digit() {
            output.push('_');
        }
        output.push(if valid { character } else { '_' });
    }
    if output.is_empty() {
        "site".to_owned()
    } else {
        output
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dependency_enumeration_is_sorted_and_excludes_properties_and_pure_globals() {
        assert_eq!(
            explicit_dependencies("format(row.label, index + Math.max(offset, 0))").unwrap(),
            ["format", "index", "offset", "row"]
        );
    }

    #[test]
    fn emitted_module_has_no_runtime_when_no_residual_exists() {
        assert_eq!(
            emit_residual_module(&[]),
            "// @generated by vize_atelier_syrinx. Pure args-in/value-out residuals only.\nexport {};\n"
        );
    }
}
