//! Structured, source-located Syrinx backend diagnostics.

use serde::{Deserialize, Serialize};
use std::fmt;

/// One precise compiler error. Unsupported features are never silent no-ops.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SyrinxDiagnostic {
    pub code: String,
    pub message: String,
    pub source: String,
    pub start_byte: u32,
    pub end_byte: u32,
    pub start_line: u32,
    pub start_column: u32,
    pub end_line: u32,
    pub end_column: u32,
}

/// Compilation failed before any partial artifact was returned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyrinxCompileFailure {
    pub diagnostics: Vec<SyrinxDiagnostic>,
}

impl fmt::Display for SyrinxCompileFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.diagnostics.is_empty() {
            return formatter.write_str("Syrinx compilation failed");
        }
        for (index, diagnostic) in self.diagnostics.iter().enumerate() {
            if index != 0 {
                formatter.write_str("\n")?;
            }
            write!(
                formatter,
                "{}:{}:{} [{}] {}",
                diagnostic.source,
                diagnostic.start_line,
                diagnostic.start_column,
                diagnostic.code,
                diagnostic.message
            )?;
        }
        Ok(())
    }
}

impl std::error::Error for SyrinxCompileFailure {}
