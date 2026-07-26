//! Verification gate contracts (§9.2). Every code-mutating turn's commit
//! carries a typed verdict — never an absence. Verdicts bind workspace
//! revisions; aggregate coverage is NEVER interchangeable with solo `verified`.

use crate::ids::{ArtifactRef, WorkspaceRevision};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "verdict", rename_all = "snake_case")]
pub enum VerifyVerdict {
    /// Gate green at revision R for this change alone.
    Verified { revision: WorkspaceRevision },
    /// Passed as part of a cohort aggregate — re-verify to use alone.
    IncludedInAggregate { revision: WorkspaceRevision },
    /// Ended during overlap; covered by the cohort's combined gate later.
    Deferred,
    /// Nothing verifiable (docs-only) or hook-waived. Recorded, never silent.
    Waived { reason: String },
    /// No adapter for the changed domain.
    Unverified,
    /// Verifier unavailable/timeout — honest, never fabricated green.
    Incomplete { reason: String },
    /// Model ended citing out-of-scope reds; delta attached via report.
    AcknowledgedRed,
    /// Cycle cap exhausted; remaining delta attached via report.
    ErroredWithReport,
    /// Environment-caused failure → workspace health item.
    FailedEnv { item: String },
    /// This turn made no verifiable changes and no gate ran.
    NotApplicable,
}

/// One normalized diagnostic (compression ladder input).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Diagnostic {
    pub file: String,
    pub line: u32,
    #[serde(default)]
    pub col: u32,
    pub severity: Severity,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    pub message: String,
    pub tool: String,
    /// Strict fingerprint: path+code+message+range.
    pub fingerprint: String,
    /// Move-tolerant fingerprint: code+template+path+anchor.
    pub fingerprint_tolerant: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Error,
    Warning,
    Info,
}

/// The gate's compact report (what reaches the model on red; one line on green).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GateReport {
    pub verdict: VerifyVerdict,
    /// New (baseline-diffed) errors, capped per the ladder.
    pub new_errors: Vec<Diagnostic>,
    #[serde(default)]
    pub new_warnings: Vec<Diagnostic>,
    /// Pre-existing issues: a count, never itemized.
    pub preexisting: u32,
    pub cycles_used: u8,
    pub duration_ms: u64,
    /// Full raw log in the CAS ({ui,durable} — never prompt).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw_log: Option<ArtifactRef>,
    /// Reserved: format/test sub-step outcomes (post-0.1 tiers).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tests: Option<serde_json::Value>,
}
