//! Daemon dispatch module — re-exports from rpc and jobs modules.
//!
//! This module exists for backward compatibility. New code should import
//! from `crate::rpc` and `crate::jobs` directly.

#[allow(unused_imports)] // backward compatibility re-exports
pub use crate::jobs::{BackgroundJob, BackgroundJobRegistry, MAX_CONCURRENT_JOBS};
#[allow(unused_imports)] // backward compatibility re-exports
pub use crate::rpc::{JsonRpcError, JsonRpcRequest, JsonRpcResponse, LogsParams, RunParams};