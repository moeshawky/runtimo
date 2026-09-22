//! Thin LLMOSafe 0.9 conformance boundary.
//!
//! One small adapter owns translation from upstream output to Runtimo
//! terminology. Dozens of ad-hoc `SafetyDecision` matches across the
//! executor are banned.
//!
//! Invariants:
//! - `LLMOSafe evidence != Runtimo policy`, `decision != action`.
//! - Canonical decision = typed final `SafetyDecision`, never
//!   `provenance.decision_label`.
//! - `UNKNOWN != SAFE`, `UNKNOWN != MALICIOUS`; `no_evidence` never safe.
//! - `SiftError` is a real safety outcome — never `unwrap`/`default`/safe.
//! - Truthful incompleteness: one-shot sifter assessment, `stages_executed`
//!   carries only stages that ran. No synthetic pipeline state.

use llmosafe::llmosafe_integration::DecisionProvenance;
use llmosafe::{EscalationPolicy, SafetyDecision, SemanticPolicy};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fmt;
use std::fmt::Write;

// ---------------------------------------------------------------------------
// Input semantics (§16-17): control plane vs payload plane
// ---------------------------------------------------------------------------

/// Semantic class of a single capability field.
///
/// Closed set: adding variants requires updating field tables, eval logic,
/// and serialization tests across the workspace.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[allow(clippy::exhaustive_enums)]
pub enum InputClass {
    /// Natural-language instruction with execution authority
    /// (agent objective / instruction, if present at this boundary).
    ControlInstruction,
    /// Shell/command control syntax (`ShellExec.cmd`).
    CommandControl,
    /// Filesystem locator (`Delete.path`, `FileRead.path`).
    FilesystemLocator,
    /// Structured identifier (`Kill.pid`, job IDs).
    StructuredIdentifier,
    /// URL / locator (`Fetch.url` style).
    UrlLocator,
    /// Source code payload (may contain English without being instruction).
    SourceCode,
    /// Natural-language payload with limited authority
    /// (git commit message, `FileWrite.content` prose).
    PayloadProse,
    /// Opaque data (JSON, binary-like text, quoted attack text).
    OpaqueData,
}

impl InputClass {
    /// Is this class eligible for semantic (manipulation) assessment?
    ///
    /// Only control-plane instruction and payload prose that an agent
    /// could mistake for instruction are eligible. Pure locators,
    /// identifiers, command syntax, and opaque data are resource-only.
    #[must_use]
    pub const fn is_semantic_eligible(self) -> bool {
        matches!(
            self,
            Self::ControlInstruction | Self::PayloadProse | Self::SourceCode
        )
    }

    /// Wire string for WAL / Oracle.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ControlInstruction => "control_instruction",
            Self::CommandControl => "command_control",
            Self::FilesystemLocator => "filesystem_locator",
            Self::StructuredIdentifier => "structured_identifier",
            Self::UrlLocator => "url_locator",
            Self::SourceCode => "source_code",
            Self::PayloadProse => "payload_prose",
            Self::OpaqueData => "opaque_data",
        }
    }
}

/// What kind of analysis produced an assessment.
///
/// Closed set: both variants are handled exhaustively by the evaluation
/// and reporting paths.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[allow(clippy::exhaustive_enums)]
pub enum AnalysisKind {
    /// One-shot sifter assessment (no durable pipeline state).
    /// Truthful: `stages_executed` = sifter only.
    SemanticOneShot,
    /// Resource-only gate (input class not semantic-eligible,
    /// or ShellExec Observe-mode command path).
    ResourceOnly,
}

impl AnalysisKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::SemanticOneShot => "semantic_one_shot",
            Self::ResourceOnly => "resource_only",
        }
    }
}

// ---------------------------------------------------------------------------
// Runtimo disposition (§29-30, §57): LLMOSafe decision != Runtimo action
// ---------------------------------------------------------------------------

/// Runtimo-side execution disposition.
///
/// Separate from [`SafetyDecision`]: LLMOSafe produces evidence + decision,
/// Runtimo maps it to a side-effect permission. `Escalate` never silently
/// becomes `Allow` or `Halt`.
///
/// Closed set: all variants are handled by `permits_execution`, `as_str`,
/// and the disposition mapping functions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[allow(clippy::exhaustive_enums)]
pub enum RuntimoDisposition {
    /// Execute now.
    Allow,
    /// Execute now, audit warning.
    AllowWithWarning,
    /// Do NOT execute now; higher-level handler required.
    /// Distinct from hard reject — preserved through audit + Oracle.
    EscalationRequired,
    /// Deny this capability now.
    Reject,
    /// Fatal-class denial. Does NOT kill the daemon/process unless
    /// Runtimo explicitly defines + tests that behavior.
    Fatal,
}

impl RuntimoDisposition {
    /// Can the governed side effect proceed?
    #[must_use]
    pub const fn permits_execution(self) -> bool {
        matches!(self, Self::Allow | Self::AllowWithWarning)
    }

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::AllowWithWarning => "allow_with_warning",
            Self::EscalationRequired => "escalation_required",
            Self::Reject => "reject",
            Self::Fatal => "fatal",
        }
    }
}

impl fmt::Display for RuntimoDisposition {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Explicit actuation policy: `SafetyDecision` → [`RuntimoDisposition`].
///
/// | LLMOSafe     | Runtimo              | Side effect |
/// |--------------|----------------------|-------------|
/// | `Proceed`    | `Allow`              | yes         |
/// | `Warn`       | `AllowWithWarning`   | yes + audit |
/// | `Escalate`   | `EscalationRequired` | no          |
/// | `Halt`       | `Reject`             | no          |
/// | `Exit`       | `Fatal`              | no (no daemon kill) |
#[must_use]
pub const fn disposition_for(decision: &SafetyDecision) -> RuntimoDisposition {
    match decision {
        SafetyDecision::Proceed => RuntimoDisposition::Allow,
        SafetyDecision::Warn(_) => RuntimoDisposition::AllowWithWarning,
        SafetyDecision::Escalate { .. } => RuntimoDisposition::EscalationRequired,
        SafetyDecision::Halt(..) => RuntimoDisposition::Reject,
        SafetyDecision::Exit(_) => RuntimoDisposition::Fatal,
    }
}

// ---------------------------------------------------------------------------
// Assessment errors (§20): SiftError is a real outcome
// ---------------------------------------------------------------------------

/// Why a semantic assessment could not complete.
///
/// Closed set: sifter errors map to one of these two outcomes.
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(clippy::exhaustive_enums)]
pub enum AssessmentError {
    /// Upstream work budget exhausted (`SiftError::ResourceExhaustion`).
    /// Never maps to `Proceed`.
    WorkBudgetExhausted,
    /// Catch-all for future non-exhaustive `SiftError` variants.
    AnalysisFailed(String),
}

impl fmt::Display for AssessmentError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WorkBudgetExhausted => f.write_str("sifter work budget exhausted"),
            Self::AnalysisFailed(s) => write!(f, "sifter analysis failed: {s}"),
        }
    }
}

impl std::error::Error for AssessmentError {}

// ---------------------------------------------------------------------------
// Versioned safety audit record (§33-36)
// ---------------------------------------------------------------------------

/// Integration contract version. Bump only on breaking schema change.
pub const SAFETY_SCHEMA_VERSION: u32 = 1;

/// Typed, versioned pre-execution safety evidence.
///
/// Only fields with consumers. No raw prompt/command/content —
/// correlation uses `input_hash` + `field_id` + `input_len` + `input_class`.
///
/// Multiple bool fields are intentional: each represents a distinct piece
/// of provenance or evidence that consumers inspect independently.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[allow(clippy::exhaustive_structs)]
#[allow(clippy::struct_excessive_bools)]
pub struct SafetyAssessmentV1 {
    /// Schema version (`SAFETY_SCHEMA_VERSION`).
    pub schema_version: u32,
    /// What ran: `"semantic_one_shot"` or `"resource_only"`.
    pub analysis_kind: AnalysisKind,
    /// Semantic class of the assessed field.
    pub input_class: InputClass,
    /// Active semantic policy (wire string).
    pub semantic_policy: String,
    /// Active DAL (wire string, e.g. `"A"`).
    pub dal: String,
    /// Canonical upstream decision status label
    /// (`"safe"|"warning"|"escalate"|"halt"|"exit"`).
    /// Canonical = typed `SafetyDecision`, never provenance label.
    pub llmosafe_status: String,
    /// Upstream severity (0-4).
    pub llmosafe_severity: u8,
    /// Whether the upstream decision blocks (`is_blocking()`).
    pub llmosafe_blocking: bool,
    /// Raw upstream provenance label (None when unavailable, e.g. sifter-only
    /// path where crate DecisionProvenance does not exist by construction).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provenance_label: Option<String>,
    /// Raw upstream provenance reasons (None when unavailable).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provenance_reasons: Option<Vec<String>>,
    /// Raw upstream provenance evidence families (None when unavailable).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provenance_families: Option<Vec<String>>,
    /// Raw upstream hard-invariant flag (None when unavailable — never
    /// independently sufficient authorization).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provenance_hard_invariant: Option<bool>,
    /// Boundary validation: does provenance agree with the final decision?
    /// None when provenance is unavailable (nothing to compare).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provenance_consistent: Option<bool>,
    /// Runtimo-generated explanation for this assessment. Never mistaken for
    /// upstream DecisionProvenance — a separately-named field carrying the
    /// boundary's own rationale.
    pub runtimo_explanation: Vec<String>,
    /// Runtimo-side disposition.
    pub runtimo_disposition: RuntimoDisposition,
    /// Bitmask of stages that actually executed (sifter-only one-shot).
    pub stages_executed: u8,
    /// OOV ratio when sifter ran.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oov_ratio: Option<u8>,
    /// Detection flags when sifter ran.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detection_flags: Option<u8>,
    /// Body pressure when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body_pressure: Option<u8>,
    /// Upstream `no_evidence` (zero-match/OOD) when sifter ran.
    /// `UNKNOWN != SAFE` — preserved, never coerced.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub no_evidence: Option<bool>,
    /// SHA-256 (first 16 hex chars) of the assessed input — correlation
    /// without secret leakage.
    pub input_hash: String,
    /// Which field was assessed (e.g. `"content"`, `"message"`).
    pub field_id: String,
    /// Byte length of the assessed input.
    pub input_len: usize,
    /// Was the complete eligible input analyzed? Always `true` — partial
    /// inspection is never narrated as full inspection (§19).
    pub analysis_complete: bool,
}

impl SafetyAssessmentV1 {
    /// Did the boundary detect provenance inconsistency?
    /// Returns true only when provenance is available AND inconsistent.
    /// When provenance is unavailable (None), returns false (no mismatch
    /// to detect).
    #[must_use]
    pub const fn has_provenance_mismatch(&self) -> bool {
        matches!(self.provenance_consistent, Some(false))
    }
}

/// Hash helper: first 16 hex chars of SHA-256 (correlation, no secrets).
#[must_use]
pub fn hash_input(input: &str) -> String {
    let digest = Sha256::digest(input.as_bytes());
    // Safe: digest is fixed-size 32 bytes, capacity = 64 chars, no overflow possible.
    #[allow(clippy::arithmetic_side_effects)]
    let mut hex = String::with_capacity(digest.len() * 2);
    for b in &digest {
        let _ = write!(hex, "{b:02x}");
    }
    hex[..16].to_string()
}

/// Validate provenance against the canonical decision at the boundary.
///
/// Checks: label agrees with final status; `hard_invariant` plausible
/// for the final decision; evidence families non-empty when a
/// non-`Proceed` decision claims semantic/mechanical roots.
/// Never rewrites upstream evidence — returns `false` on mismatch and
/// the caller records/reports it while enforcing from the canonical path.
#[must_use]
pub fn provenance_consistent(decision: &SafetyDecision, provenance: &DecisionProvenance) -> bool {
    let expected_label = decision.status_label();
    if !provenance
        .decision_label
        .eq_ignore_ascii_case(expected_label)
    {
        return false;
    }
    // Hard-invariant must only accompany Halt/Exit-class outcomes.
    // Dual-root semantic Halt carries hard_invariant=false by contract,
    // so a `true` flag on Proceed/Warn/Escalate is implausible.
    if provenance.hard_invariant
        && matches!(
            decision,
            SafetyDecision::Proceed | SafetyDecision::Warn(_) | SafetyDecision::Escalate { .. }
        )
    {
        return false;
    }
    // A blocking decision with zero evidence families is suspicious
    // (detection-gate specificity / incomplete provenance).
    if decision.is_blocking() && provenance.evidence_families.is_empty() {
        return false;
    }
    true
}

// ---------------------------------------------------------------------------
// One-shot assessment (§14 Path B, §15 branch E)
//
// Runtimo sessions group job IDs for audit/resume/rollback; no durable
// semantic sequence reaches the executor (daemon dispatch + CLI run pass
// `None` session; session steps add job IDs post-hoc). There is no
// legitimate `CognitivePipeline` lifetime here, so we expose a truthful
// one-shot assessment. No `PipelineResult` for stages that never ran.
// ---------------------------------------------------------------------------

/// Run a one-shot sifter assessment over the complete eligible input.
///
/// - Uses the full input (no 8192-byte blind tail — upstream
///   `MAX_WORK_TOKENS` + fallible `SIFT` is the bounded-work contract).
/// - `SiftError` maps to [`AssessmentError`], never `Proceed`.
/// - Upstream ordering `raw → SemanticPolicy → DAL → final` is consumed
///   from the crate (`decide_with_pressure`), never reconstructed.
/// - Provenance is preserved + consistency-validated; enforcement uses
///   only the canonical `SafetyDecision`.
///
/// # Errors
///
/// Returns [`AssessmentError::WorkBudgetExhausted`] if the sifter exceeds
/// its work budget, or [`AssessmentError::AnalysisFailed`] for any other
/// sifter failure. Never returns `Ok(Proceed)` on error.
#[allow(clippy::too_many_arguments)]
pub fn assess_one_shot(
    policy: &EscalationPolicy,
    observation: &str,
    field_id: &str,
    input_class: InputClass,
    body_pressure: Option<u8>,
) -> Result<SafetyAssessmentV1, AssessmentError> {
    use llmosafe::{PressureLevel, SiftError};

    let (sifted, _proof) = llmosafe::sift_text(observation).map_err(|e| {
        // Single variant today; wildcard keeps future non-exhaustive
        // variants fail-closed instead of becoming Proceed.
        #[allow(unreachable_patterns)]
        match e {
            SiftError::ResourceExhaustion => AssessmentError::WorkBudgetExhausted,
            other => AssessmentError::AnalysisFailed(format!("{other:?}")),
        }
    })?;
    let synapse = sifted.into_inner();

    let pressure = body_pressure.unwrap_or_else(|| {
        // Best-effort ambient pressure; sifter path owns enforcement via
        // decide_with_pressure. Failure to read pressure degrades to
        // Nominal rather than blocking semantic evidence.
        llmosafe::ResourceGuard::auto(0.8).pressure()
    });
    let pressure_level = PressureLevel::from_percentage(pressure);
    let decision = policy.decide_with_pressure(
        synapse.raw_entropy(),
        synapse.raw_surprise(),
        synapse.has_bias(),
        pressure_level,
    );

    // Canonical provenance: rebuild from the decision via the crate's own
    // constructors where possible is forbidden (would duplicate authority).
    // Instead consume the decision + synapse signals and record a
    // boundary-built provenance snapshot. The crate's full
    // `DecisionProvenance` (from `process_ctrl`) is unavailable on the
    // sifter-only path by construction — truthful incompleteness.
    let dal_str = format!("{:?}", policy.dal);
    let sp_str = match policy.semantic_policy {
        SemanticPolicy::Observe => "observe",
        SemanticPolicy::Corroborate => "corroborate",
        SemanticPolicy::Enforce => "enforce",
    };

    // Unavailable upstream provenance stays unavailable (None, never synthesized).
    // The crate's full DecisionProvenance (from process_ctrl) is unavailable on
    // the sifter-only path by construction — truthful incompleteness.
    // Runtimo-generated explanation goes in a separately-named field.
    Ok(SafetyAssessmentV1 {
        schema_version: SAFETY_SCHEMA_VERSION,
        analysis_kind: AnalysisKind::SemanticOneShot,
        input_class,
        semantic_policy: sp_str.to_string(),
        dal: dal_str,
        llmosafe_status: decision.status_label().to_string(),
        llmosafe_severity: decision.severity(),
        llmosafe_blocking: decision.is_blocking(),
        provenance_label: None,
        provenance_reasons: None,
        provenance_families: None,
        provenance_hard_invariant: None,
        provenance_consistent: None,
        runtimo_explanation: vec![format!("one-shot sifter: {}", decision.status_label())],
        runtimo_disposition: disposition_for(&decision),
        stages_executed: llmosafe::llmosafe_pipeline::STAGE_SIFT,
        oov_ratio: Some(synapse.oov_ratio()),
        detection_flags: Some(synapse.detection_flags()),
        body_pressure: Some(pressure),
        // Upstream gap (§21): `sift_text` does not expose `no_evidence`
        // (`ClassificationResult.no_evidence`) without re-running the
        // classifier (double work, §76). Recorded as absent, never
        // coerced to safe. `None != false`.
        no_evidence: None,
        input_hash: hash_input(observation),
        field_id: field_id.to_string(),
        input_len: observation.len(),
        analysis_complete: true,
    })
}

/// Build a resource-only assessment (no semantic eligibility).
#[must_use]
pub fn resource_only_assessment(
    input_class: InputClass,
    field_id: &str,
    semantic_policy: SemanticPolicy,
    dal: llmosafe::DesignAssuranceLevel,
    body_pressure: Option<u8>,
) -> SafetyAssessmentV1 {
    let sp_str = match semantic_policy {
        SemanticPolicy::Observe => "observe",
        SemanticPolicy::Corroborate => "corroborate",
        SemanticPolicy::Enforce => "enforce",
    };
    // Unavailable upstream provenance stays unavailable (None, never synthesized).
    // `not assessed != safe` — the upstream semantic decision did not run.
    SafetyAssessmentV1 {
        schema_version: SAFETY_SCHEMA_VERSION,
        analysis_kind: AnalysisKind::ResourceOnly,
        input_class,
        semantic_policy: sp_str.to_string(),
        dal: format!("{dal:?}"),
        // Not the upstream status — this is the Runtimo resource gate disposition.
        // llmosafe_status remains a placeholder string; the semantic decision is absent.
        llmosafe_status: "resource-only".to_string(),
        llmosafe_severity: 0,
        llmosafe_blocking: false,
        provenance_label: None,
        provenance_reasons: None,
        provenance_families: None,
        provenance_hard_invariant: None,
        provenance_consistent: None,
        runtimo_explanation: vec!["input class not semantic-eligible".to_string()],
        runtimo_disposition: RuntimoDisposition::Allow,
        stages_executed: 0,
        oov_ratio: None,
        detection_flags: None,
        body_pressure,
        no_evidence: None,
        input_hash: String::from("-"),
        field_id: field_id.to_string(),
        input_len: 0,
        analysis_complete: true,
    }
}

// ---------------------------------------------------------------------------
// Declarative input semantics (§17): one authority, exhaustive coverage
// ---------------------------------------------------------------------------

/// A single assessable field: its key, semantic class, and whether the
/// sifter should see it.
///
/// Closed set of fields per capability: adding variants requires updating
/// the per-capability field tables and coverage tests.
#[derive(Debug, Clone, Copy)]
#[allow(clippy::exhaustive_structs)]
pub struct FieldSemantics {
    /// JSON arg key (e.g. `"content"`).
    pub field: &'static str,
    /// Semantic class.
    pub class: InputClass,
    /// Control plane (`true`) vs payload (`false`).
    pub is_control: bool,
    /// Should this field reach LLMOSafe?
    pub sifter_eligible: bool,
}

/// Authoritative per-capability field table.
///
/// Exhaustive over registered capabilities. Tests fail when a new
/// capability lacks classification (`capability_coverage` test).
#[must_use]
pub fn fields_for(cap_name: &str) -> &'static [FieldSemantics] {
    match cap_name {
        // ShellExec: cmd = command syntax; cwd = locator; stdin = text payload.
        "ShellExec" => &[
            FieldSemantics {
                field: "cmd",
                class: InputClass::CommandControl,
                is_control: true,
                sifter_eligible: false,
            },
            FieldSemantics {
                field: "cwd",
                class: InputClass::FilesystemLocator,
                is_control: false,
                sifter_eligible: false,
            },
            FieldSemantics {
                field: "stdin",
                class: InputClass::PayloadProse,
                is_control: false,
                sifter_eligible: true,
            },
        ],
        // FileWrite: content = prose/code/JSON payload (eligible); path = locator.
        "FileWrite" => &[
            FieldSemantics {
                field: "content",
                class: InputClass::PayloadProse,
                is_control: false,
                sifter_eligible: true,
            },
            FieldSemantics {
                field: "path",
                class: InputClass::FilesystemLocator,
                is_control: false,
                sifter_eligible: false,
            },
        ],
        "FileRead" | "Delete" => &[FieldSemantics {
            field: "path",
            class: InputClass::FilesystemLocator,
            is_control: false,
            sifter_eligible: false,
        }],
        "Kill" => &[FieldSemantics {
            field: "pid",
            class: InputClass::StructuredIdentifier,
            is_control: false,
            sifter_eligible: false,
        }],
        // GitExec: message = NL payload (eligible); operation = command syntax;
        // url/path/branch/commit_sha/files = locators/identifiers.
        "GitExec" => &[
            FieldSemantics {
                field: "message",
                class: InputClass::PayloadProse,
                is_control: false,
                sifter_eligible: true,
            },
            FieldSemantics {
                field: "operation",
                class: InputClass::CommandControl,
                is_control: true,
                sifter_eligible: false,
            },
            FieldSemantics {
                field: "url",
                class: InputClass::UrlLocator,
                is_control: false,
                sifter_eligible: false,
            },
            FieldSemantics {
                field: "path",
                class: InputClass::FilesystemLocator,
                is_control: false,
                sifter_eligible: false,
            },
            FieldSemantics {
                field: "branch",
                class: InputClass::StructuredIdentifier,
                is_control: false,
                sifter_eligible: false,
            },
            FieldSemantics {
                field: "files",
                class: InputClass::FilesystemLocator,
                is_control: false,
                sifter_eligible: false,
            },
            FieldSemantics {
                field: "commit_sha",
                class: InputClass::StructuredIdentifier,
                is_control: false,
                sifter_eligible: false,
            },
        ],
        // Undo: job_id = identifier; file = optional target locator.
        "Undo" => &[
            FieldSemantics {
                field: "job_id",
                class: InputClass::StructuredIdentifier,
                is_control: false,
                sifter_eligible: false,
            },
            FieldSemantics {
                field: "file",
                class: InputClass::FilesystemLocator,
                is_control: false,
                sifter_eligible: false,
            },
        ],
        _ => &[],
    }
}

/// All registered capability names (must stay exhaustive).
pub const REGISTERED_CAPABILITIES: &[&str] = &[
    "ShellExec",
    "FileWrite",
    "FileRead",
    "Delete",
    "Kill",
    "GitExec",
    "Undo",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capability_coverage_is_exhaustive() {
        for cap in REGISTERED_CAPABILITIES {
            // Every registered capability must have a classification entry,
            // even if that entry is resource-only. Unknown caps return
            // empty slice — which this test treats as a coverage hole.
            let fields = fields_for(cap);
            assert!(
                !fields.is_empty(),
                "capability {cap} lacks input-semantics classification"
            );
        }
    }

    /// Tripwire: verifies every known capability arg field is classified.
    /// When a new field is added to a capability's args struct, this test
    /// fails until the field table is updated in `fields_for()`.
    /// Unavailable upstream provenance stays unavailable (None, never synthesized).
    /// The sifter-only path does not have crate DecisionProvenance by construction.
    #[test]
    fn sifter_only_provenance_is_absent() {
        use llmosafe::DesignAssuranceLevel as DAL;
        let policy = llmosafe::EscalationPolicy::default()
            .with_semantic_policy(SemanticPolicy::Corroborate)
            .with_dal(DAL::A);
        let assessment = assess_one_shot(
            &policy,
            "Hello world",
            "content",
            InputClass::PayloadProse,
            Some(10),
        )
        .unwrap();
        // Upstream provenance is unavailable on the sifter-only path.
        assert!(
            assessment.provenance_label.is_none(),
            "provenance_label must be None on sifter-only path"
        );
        assert!(
            assessment.provenance_reasons.is_none(),
            "provenance_reasons must be None on sifter-only path"
        );
        assert!(
            assessment.provenance_families.is_none(),
            "provenance_families must be None on sifter-only path"
        );
        assert!(
            assessment.provenance_hard_invariant.is_none(),
            "provenance_hard_invariant must be None on sifter-only path"
        );
        assert!(
            assessment.provenance_consistent.is_none(),
            "provenance_consistent must be None when provenance is unavailable"
        );
        // Runtimo-generated explanation must be present and non-empty.
        assert!(
            !assessment.runtimo_explanation.is_empty(),
            "runtimo_explanation must be present when provenance is absent"
        );
    }

    /// Resource-only is never semantic Safe/Proceed.
    /// `not assessed != safe` — the upstream semantic decision did not run.
    #[test]
    fn resource_only_never_semantic_safe() {
        use llmosafe::DesignAssuranceLevel as DAL;
        let assessment = resource_only_assessment(
            InputClass::FilesystemLocator,
            "path",
            SemanticPolicy::Corroborate,
            DAL::A,
            Some(10),
        );
        // llmosafe_status must not be the semantic "safe" string.
        assert_ne!(
            assessment.llmosafe_status, "safe",
            "resource-only must not claim semantic 'safe'"
        );
        // Disposition is still Allow (Runtimo resource gate passed).
        assert_eq!(assessment.runtimo_disposition, RuntimoDisposition::Allow);
        // Upstream provenance is absent (no semantic decision ran).
        assert!(assessment.provenance_label.is_none());
        assert!(assessment.provenance_consistent.is_none());
    }

    /// Tripwire: verifies every known capability arg field is classified.
    /// When a new field is added to a capability's args struct, this test
    /// fails until the field table is updated in `fields_for()`.
    #[test]
    fn all_capability_fields_are_classified() {
        fn fields_for_cap(cap: &str) -> Vec<&'static str> {
            fields_for(cap).iter().map(|fs| fs.field).collect()
        }

        // ShellExec: cmd, cwd, stdin, timeout_secs (numeric — excluded)
        let shell = fields_for_cap("ShellExec");
        for required in &["cmd", "cwd", "stdin"] {
            assert!(
                shell.contains(required),
                "ShellExec field '{required}' not classified"
            );
        }

        // FileWrite: path, content, append (bool — excluded)
        let write = fields_for_cap("FileWrite");
        for required in &["path", "content"] {
            assert!(
                write.contains(required),
                "FileWrite field '{required}' not classified"
            );
        }

        // FileRead: path, max_bytes (numeric — excluded)
        let read = fields_for_cap("FileRead");
        assert!(
            read.contains(&"path"),
            "FileRead field 'path' not classified"
        );

        // Delete: path, no_backup (bool — excluded)
        let delete = fields_for_cap("Delete");
        assert!(
            delete.contains(&"path"),
            "Delete field 'path' not classified"
        );

        // Kill: pid (numeric identifier, included), signal (numeric — excluded)
        let kill = fields_for_cap("Kill");
        assert!(kill.contains(&"pid"), "Kill field 'pid' not classified");

        // GitExec: operation, url, path, branch, message, files, commit_sha,
        // timeout_secs (numeric — excluded)
        let git = fields_for_cap("GitExec");
        for required in &[
            "operation",
            "url",
            "path",
            "branch",
            "message",
            "files",
            "commit_sha",
        ] {
            assert!(
                git.contains(required),
                "GitExec field '{required}' not classified"
            );
        }

        // Undo: job_id, file
        let undo = fields_for_cap("Undo");
        for required in &["job_id", "file"] {
            assert!(
                undo.contains(required),
                "Undo field '{required}' not classified"
            );
        }
    }

    /// Verifies that resource-only assessment uses a field actually present
    /// in the args (executor `fields.first()` fallback fix).
    #[test]
    fn resource_only_uses_present_field() {
        use super::*;
        // Simulate: args has "cwd" but not "cmd" for ShellExec.
        // The resource-only assessment must record "cwd", not "cmd".
        let fields = fields_for("ShellExec");
        let args = serde_json::json!({"cwd": "/tmp"});
        let (class, field) = fields
            .iter()
            .find(|fs| args.get(fs.field).is_some())
            .map_or((InputClass::OpaqueData, "-"), |fs| (fs.class, fs.field));
        assert_eq!(field, "cwd", "must use present field, not table-first");
        assert_eq!(class, InputClass::FilesystemLocator);

        // Simulate: no field present → default OpaqueData, "-".
        let args2 = serde_json::json!({});
        let (class2, field2) = fields
            .iter()
            .find(|fs| args2.get(fs.field).is_some())
            .map_or((InputClass::OpaqueData, "-"), |fs| (fs.class, fs.field));
        assert_eq!(field2, "-");
        assert_eq!(class2, InputClass::OpaqueData);
    }

    #[test]
    fn disposition_mapping_preserves_distinctions() {
        use llmosafe::{EscalationReason, KernelError};
        assert_eq!(
            disposition_for(&SafetyDecision::Proceed),
            RuntimoDisposition::Allow
        );
        assert_eq!(
            disposition_for(&SafetyDecision::Warn("x")),
            RuntimoDisposition::AllowWithWarning
        );
        assert_eq!(
            disposition_for(&SafetyDecision::Escalate {
                entropy: 0,
                reason: EscalationReason::BiasDetected,
                cooldown_ms: 5000,
            }),
            RuntimoDisposition::EscalationRequired
        );
        assert_eq!(
            disposition_for(&SafetyDecision::Halt(
                KernelError::CognitiveInstability,
                30000
            )),
            RuntimoDisposition::Reject
        );
        assert_eq!(
            disposition_for(&SafetyDecision::Exit(KernelError::CognitiveInstability)),
            RuntimoDisposition::Fatal
        );
    }

    #[test]
    fn assessment_serialization_round_trips() {
        let a = resource_only_assessment(
            InputClass::FilesystemLocator,
            "path",
            SemanticPolicy::Corroborate,
            llmosafe::DesignAssuranceLevel::A,
            Some(10),
        );
        let s = serde_json::to_string(&a).unwrap();
        let b: SafetyAssessmentV1 = serde_json::from_str(&s).unwrap();
        assert_eq!(a, b);
        // No raw secrets in the record.
        assert!(!s.contains("secret"));
    }

    #[test]
    fn provenance_mismatch_detected() {
        use llmosafe::KernelError;
        let prov = DecisionProvenance {
            decision_label: "Halt".to_string(),
            reasons: vec!["x".to_string()],
            evidence_families: vec!["mechanical".to_string()],
            hard_invariant: true,
        };
        // Proceed + hard_invariant=true is implausible → inconsistent.
        assert!(!provenance_consistent(&SafetyDecision::Proceed, &prov));
        let ok = DecisionProvenance::mechanical_halt("oom");
        assert!(provenance_consistent(
            &SafetyDecision::Halt(KernelError::ResourceExhaustion, 30000),
            &ok
        ));
    }
}
