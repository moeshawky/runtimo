//! JSON-RPC types for the daemon dispatch system.
//!
//! Provides wire-format types for JSON-RPC request/response handling.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Inbound JSON-RPC request over the Unix socket.
///
/// Parsed from one line of JSON. The `id` field is echoed back in the response
/// for request/response correlation.
#[derive(Debug, Deserialize)]
pub struct JsonRpcRequest {
    /// RPC method name (e.g. "run", "dispatch", "status").
    pub method: String,
    /// Method parameters (defaults to JSON null).
    #[serde(default)]
    pub params: Value,
    /// Request identifier for correlation.
    pub id: Value,
}

/// Outbound JSON-RPC response over the Unix socket.
///
/// Contains either a `result` or an `error`, never both.
#[derive(Debug, Serialize)]
pub struct JsonRpcResponse {
    /// Successful response data (absent on error).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    /// Error details (absent on success).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcError>,
    /// Echoed request identifier for correlation.
    pub id: Value,
}

/// JSON-RPC error response body.
#[derive(Debug, Serialize)]
pub struct JsonRpcError {
    /// Error code (JSON-RPC standard: -32700 parse error, -32601 method not found, etc.).
    pub code: i32,
    /// Human-readable error description.
    pub message: String,
}

/// Parameters for the `run` and `dispatch` JSON-RPC methods.
///
/// Contains the capability name, JSON arguments, dry-run flag, optional
/// working directory, and an optional execution timeout in seconds.
/// Timeout defaults to 30s when not specified.
#[derive(Debug, Deserialize)]
pub struct RunParams {
    /// Capability name to execute (e.g., "FileRead", "ShellExec").
    pub capability: String,
    /// JSON arguments for the capability (defaults to empty object).
    #[serde(default)]
    pub args: Value,
    /// If true, capability skips side effects (dry-run mode).
    #[serde(default)]
    pub dry_run: bool,
    /// Optional working directory for relative path resolution.
    #[serde(default)]
    pub working_dir: Option<String>,
    /// Optional execution timeout in seconds (default: 30).
    #[serde(default)]
    pub timeout_secs: Option<u64>,
}

/// Parameters for the `logs` JSON-RPC method.
#[derive(Debug, Deserialize)]
pub struct LogsParams {
    /// Maximum number of log entries to return (default: 10).
    #[serde(default = "default_limit")]
    pub limit: usize,
}

/// Default log entry limit.
fn default_limit() -> usize {
    10
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn test_json_rpc_request_deserialization() {
        let json = serde_json::json!({
            "method": "run",
            "params": {"capability": "FileRead"},
            "id": 1
        });
        let req: JsonRpcRequest = serde_json::from_value(json).unwrap();
        assert_eq!(req.method, "run");
        assert_eq!(req.id, serde_json::Value::from(1));
    }

    #[test]
    fn test_json_rpc_response_result_serialization() {
        let resp = JsonRpcResponse {
            result: Some(serde_json::json!({"success": true})),
            error: None,
            id: serde_json::Value::from(1),
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("success"));
        assert!(!json.contains("error"));
    }

    #[test]
    fn test_json_rpc_response_error_serialization() {
        let resp = JsonRpcResponse {
            result: None,
            error: Some(JsonRpcError {
                code: -32601,
                message: "Method not found".into(),
            }),
            id: serde_json::Value::from(1),
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("Method not found"));
        assert!(json.contains("-32601"));
    }

    #[test]
    fn test_run_params_all_fields_valid() {
        let json = serde_json::json!({
            "capability": "FileRead",
            "args": {"path": "/tmp/test.txt"},
            "dry_run": true,
            "working_dir": "/tmp",
            "timeout_secs": 60
        });
        let params: RunParams = serde_json::from_value(json).unwrap();
        assert_eq!(params.capability, "FileRead");
        assert_eq!(params.args, serde_json::json!({"path": "/tmp/test.txt"}));
        assert!(params.dry_run);
        assert_eq!(params.working_dir.as_deref(), Some("/tmp"));
        assert_eq!(params.timeout_secs, Some(60));
    }

    #[test]
    fn test_run_params_minimal_valid() {
        let json = serde_json::json!({"capability": "ShellExec"});
        let params: RunParams = serde_json::from_value(json).unwrap();
        assert_eq!(params.capability, "ShellExec");
        assert_eq!(params.args, serde_json::Value::Null);
        assert!(!params.dry_run);
        assert!(params.working_dir.is_none());
        assert!(params.timeout_secs.is_none());
    }

    #[test]
    fn test_run_params_missing_capability() {
        let json = serde_json::json!({"args": {"path": "/tmp/test.txt"}});
        let result = serde_json::from_value::<RunParams>(json);
        assert!(result.is_err());
    }

    #[test]
    fn test_logs_params_default_limit() {
        let params: LogsParams = serde_json::from_value(serde_json::json!({})).unwrap();
        assert_eq!(params.limit, 10);
    }

    #[test]
    fn test_logs_params_custom_limit() {
        let params: LogsParams = serde_json::from_value(serde_json::json!({"limit": 5})).unwrap();
        assert_eq!(params.limit, 5);
    }
}