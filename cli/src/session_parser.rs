//! Session prompt parser — JSONL + markdown ```runtimo fenced blocks.
//!
//! Parses a task prompt file into a bounded list of capability steps.
//! Supports two input shapes:
//! - Plain JSONL: one `{"capability":"...","args":{...}}` per non-empty line
//! - Markdown with ````runtimo` fenced blocks: JSONL inside the fences is
//!   extracted; content outside fences is ignored.
//!
//! Invariants enforced:
//! - File size < 1 MiB (1_048_576 bytes) — prevents unbounded prompt growth
//! - At least one step must be present (`steps.len() > 0`)
//! - Every `capability` must exist in the provided [`CapabilityRegistry`]
//! - `prompt_file` path is validated via [`validate_path`] to reject traversal
//!   (`..`, NUL, control chars) before the file is opened.

use runtimo_core::validation::{validate_path, PathContext};
use runtimo_core::CapabilityRegistry;
use serde_json::Value;
use std::path::{Path, PathBuf};

/// Maximum prompt file size (1 MiB).
const MAX_PROMPT_BYTES: usize = 1_048_576;

/// A single parsed execution step from a prompt file.
///
/// Each step maps directly to one capability invocation with its JSON args.
#[derive(Debug, Clone)]
#[allow(clippy::exhaustive_structs)]
pub struct ParsedStep {
    /// Capability name (e.g. `"FileRead"`, `"ShellExec"`).
    pub capability: String,
    /// JSON arguments for the capability.
    pub args: Value,
}

/// Parses a prompt file into a list of [`ParsedStep`].
///
/// # Parameters
/// - `path`: prompt file to read (validated for traversal via [`validate_path`])
/// - `registry`: capability registry used to validate that each `capability` exists
///
/// # Returns
/// `Ok(Vec<ParsedStep>)` with at least one step, or `Err(String)` with a
/// human-readable reason (size violation, traversal, empty, unknown capability,
/// malformed JSON).
///
/// # Errors
/// Returns `Err` when:
/// - Path validation fails (empty, traversal `..`, NUL, control chars)
/// - File cannot be read or exceeds 1 MiB
/// - No steps are found (`steps == 0`)
/// - Any JSON line lacks `capability` or has an unknown capability
pub fn parse_prompt_file(
    path: &Path,
    registry: &CapabilityRegistry,
) -> Result<Vec<ParsedStep>, String> {
    // Validate prompt_file path for traversal before opening.
    // We add the prompt file's parent and the current working directory as
    // allowed context prefixes so that a task.md in the cwd (or /tmp/task.md)
    // passes prefix checks without weakening traversal detection.
    validate_prompt_path(path)?;

    let content = std::fs::read_to_string(path)
        .map_err(|e| format!("Failed to read prompt file '{}': {}", path.display(), e))?;

    if content.len() > MAX_PROMPT_BYTES {
        return Err(format!(
            "Prompt file too large: {} bytes (max {} bytes / 1 MiB)",
            content.len(),
            MAX_PROMPT_BYTES
        ));
    }

    let steps = extract_steps(&content, registry)?;

    if steps.is_empty() {
        return Err(
            "Prompt file contains no steps (need at least one {\"capability\",\"args\"} object)"
                .to_string(),
        );
    }

    Ok(steps)
}

/// Validates `path` via [`validate_path`] with traversal rejection.
///
/// Adds the file's parent directory and current working directory as contextual
/// allowed prefixes so that legitimate `task.md` files inside the cwd are not
/// rejected by the global path whitelist, while `..` traversal remains blocked
/// by the prefix-independent checks in [`validate_path`].
fn validate_prompt_path(path: &Path) -> Result<PathBuf, String> {
    let path_str = path.to_string_lossy().to_string();

    // Collect contextual allowed prefixes.
    // Owned Vec<String> avoids an unbounded leak — PathContext now owns its
    // prefixes (no 'static lifetime leak). Traversal checks in `validate_path`
    // reject `..`, NUL, and control chars before prefix matching, so contextual
    // prefixes do not weaken security.
    let mut contextual: Vec<String> = Vec::new();
    if let Ok(cwd) = std::env::current_dir() {
        contextual.push(cwd.to_string_lossy().to_string());
    }
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            // Canonicalize parent if it exists to handle relative parents.
            if let Ok(canon) = parent.canonicalize() {
                contextual.push(canon.to_string_lossy().to_string());
            } else {
                contextual.push(parent.to_string_lossy().to_string());
            }
        }
    }

    let ctx = PathContext {
        allowed_prefixes: contextual,
        require_exists: true,
        require_file: true,
    };
    validate_path(&path_str, &ctx)
}

/// Extracts steps from raw file content.
///
/// If ````runtimo` fences are present, extracts JSONL from inside each fence;
/// otherwise treats the entire file as JSONL.
#[allow(clippy::arithmetic_side_effects)] // idx+1 bounded by lines count (<1MiB file, enumerate index); truncation not possible for display line numbers
fn extract_steps(content: &str, registry: &CapabilityRegistry) -> Result<Vec<ParsedStep>, String> {
    let jsonl_source = if content.contains("```runtimo") {
        extract_fenced_content(content)
    } else {
        content.to_string()
    };

    let mut steps = Vec::new();
    for (idx, line) in jsonl_source.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        // Skip markdown fences if they leaked through.
        if trimmed.starts_with("```") {
            continue;
        }
        let val: Value = serde_json::from_str(trimmed).map_err(|e| {
            format!(
                "Invalid JSON on line {}: {} (line: {})",
                idx + 1,
                e,
                truncate_line(trimmed)
            )
        })?;

        let cap = val
            .get("capability")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                format!(
                    "Missing 'capability' field on line {} (line: {})",
                    idx + 1,
                    truncate_line(trimmed)
                )
            })?
            .to_string();

        if registry.get(&cap).is_none() {
            return Err(format!(
                "Unknown capability '{}' on line {} (line: {})",
                cap,
                idx + 1,
                truncate_line(trimmed)
            ));
        }

        let args = val
            .get("args")
            .cloned()
            .unwrap_or_else(|| Value::Object(serde_json::Map::default()));

        steps.push(ParsedStep {
            capability: cap,
            args,
        });
    }

    Ok(steps)
}

/// Extracts the concatenated content of all ````runtimo` … ` ``` fences.
fn extract_fenced_content(content: &str) -> String {
    let mut out = String::new();
    let mut in_fence = false;
    for line in content.lines() {
        let trimmed = line.trim();
        if !in_fence && trimmed.starts_with("```runtimo") {
            in_fence = true;
            continue;
        }
        if in_fence && trimmed.starts_with("```") {
            in_fence = false;
            out.push('\n');
            continue;
        }
        if in_fence {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

fn truncate_line(s: &str) -> String {
    if s.len() <= 160 {
        s.to_string()
    } else {
        format!("{}...[truncated]", &s[..160])
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)] // test code uses unwrap/indexing for brevity — allowed with comment per hygiene policy
mod tests {
    use super::*;
    use runtimo_core::{capabilities::FileRead, CapabilityRegistry};

    fn reg() -> CapabilityRegistry {
        let mut r = CapabilityRegistry::new();
        r.register(FileRead);
        // Register minimal set for tests - FileRead and FileWrite/ShellExec
        // Use FileWrite via core's validation; we avoid filesystem side-effects here
        // by only testing parsing, not execution.
        r.register(runtimo_core::capabilities::ShellExec);
        r
    }

    #[test]
    fn parses_jsonl() {
        let tmp = std::env::temp_dir().join("runtimo_parser_test_jsonl.md");
        let _ = std::fs::remove_file(&tmp);
        std::fs::write(
            &tmp,
            "{\"capability\":\"FileRead\",\"args\":{\"path\":\"/tmp/x\"}}\n{\"capability\":\"ShellExec\",\"args\":{\"cmd\":\"echo hi\"}}\n",
        )
        .unwrap();
        let steps = parse_prompt_file(&tmp, &reg()).unwrap();
        assert_eq!(steps.len(), 2);
        assert_eq!(steps[0].capability, "FileRead");
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn parses_fenced_block() {
        let tmp = std::env::temp_dir().join("runtimo_parser_test_fenced.md");
        let _ = std::fs::remove_file(&tmp);
        std::fs::write(
            &tmp,
            "# Task\n\n```runtimo\n{\"capability\":\"ShellExec\",\"args\":{\"cmd\":\"echo hi\"}}\n```\n",
        )
        .unwrap();
        let steps = parse_prompt_file(&tmp, &reg()).unwrap();
        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0].capability, "ShellExec");
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn rejects_unknown_capability() {
        let tmp = std::env::temp_dir().join("runtimo_parser_test_unknown.md");
        std::fs::write(&tmp, "{\"capability\":\"Nope\",\"args\":{}}\n").unwrap();
        let err = parse_prompt_file(&tmp, &reg()).unwrap_err();
        assert!(err.contains("Unknown capability"));
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn rejects_traversal() {
        let fake = std::path::Path::new("/tmp/../etc/passwd");
        let err = validate_prompt_path(fake).unwrap_err();
        assert!(err.contains("traversal"), "got: {}", err);
    }

    #[test]
    fn rejects_empty_steps() {
        let tmp = std::env::temp_dir().join("runtimo_parser_test_empty.md");
        std::fs::write(&tmp, "# just a comment\n\n").unwrap();
        let err = parse_prompt_file(&tmp, &reg()).unwrap_err();
        assert!(err.contains("no steps"), "got: {}", err);
        let _ = std::fs::remove_file(&tmp);
    }
}
