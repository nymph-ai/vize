// Serializable artifacts and emitted modules intentionally use owned standard
// strings and ordinary formatting at the public compiler boundary.
#![allow(
    clippy::disallowed_macros,
    clippy::disallowed_methods,
    clippy::disallowed_types
)]

//! First-class Vize backend for Syrinx ComponentPlan guests.
//!
//! This crate consumes Vapor IR before stock JavaScript DOM emit. It emits
//! immutable host templates plus a DOM-less JavaScript guest; it never parses,
//! translates, or emulates `@vue/runtime-vapor` calls.

mod compiler;
mod coverage;
mod diagnostic;
mod guest;
mod hybrid;
mod model;
mod rsx;

pub use compiler::{
    SyrinxCompileOptions, SyrinxComponentLink, SyrinxProgramSource, compile_syrinx,
    compile_syrinx_program,
};
pub use coverage::{
    RsxCoverageClass, RsxCoverageDecision, RsxCoverageReport, RsxCoverageSite, RsxCoverageSiteKind,
    RsxCoverageSummary, measure_rsx_coverage,
};
pub use diagnostic::{SyrinxCompileFailure, SyrinxDiagnostic};
pub use hybrid::{ResidualExport, SyrinxHybridArtifact, compile_syrinx_hybrid};
pub use model::*;
pub use rsx::{RsxExpressionHook, SyrinxRsxArtifact, compile_syrinx_rsx};
