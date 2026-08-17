//! Compile-or-error entry point for the renderer-native RSX target.

use crate::{
    RsxCoverageClass, RsxCoverageReport, RsxCoverageSite, SyrinxCompileFailure, SyrinxDiagnostic,
    SyrinxRsxArtifact, SyrinxRsxOptions, compile_syrinx_rsx, measure_rsx_coverage,
};

/// Complete RSX output after every authored site has been proven native Rust.
#[derive(Debug, Clone, PartialEq)]
pub struct SyrinxCheckedArtifact {
    pub rsx: SyrinxRsxArtifact,
    pub classification: RsxCoverageReport,
    pub classification_json: String,
}

/// Classify and emit one SFC, rejecting every site without a native lowering.
pub fn compile_syrinx_checked(
    source: &str,
    options: SyrinxRsxOptions,
) -> Result<SyrinxCheckedArtifact, SyrinxCompileFailure> {
    let classification = measure_rsx_coverage(source, options.clone())?;
    let diagnostics = classification
        .sites
        .iter()
        .filter(|site| site.classification == RsxCoverageClass::Rejected)
        .map(|site| rejected_diagnostic(&classification.source, site))
        .collect::<Vec<_>>();
    if !diagnostics.is_empty() {
        return Err(SyrinxCompileFailure { diagnostics });
    }

    let rsx = compile_syrinx_rsx(source, options)?;
    let classification_json = serde_json::to_string_pretty(&classification)
        .expect("the classification report only contains serializable compiler data");
    Ok(SyrinxCheckedArtifact {
        rsx,
        classification,
        classification_json,
    })
}

fn rejected_diagnostic(source: &str, site: &RsxCoverageSite) -> SyrinxDiagnostic {
    SyrinxDiagnostic {
        code: "SYRINX_RSX_UNLOWERED_EXPRESSION".to_owned(),
        message: format!(
            "'{}' has no native Rust lowering: {}",
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
