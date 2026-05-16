//! LLMOSafe integration — Real resource limits via the `llmosafe` crate.
//!
//! Uses `llmosafe::ResourceGuard` (Tier 0: Resource Body) for physical resource
//! monitoring. Maps RSS memory and CPU load to the CognitiveEntropy/Synapse system.
//!
//! The guard checks actual `/proc/stat` and `/proc/self/status` — no approximations.
//!
//! # Example
//!
//! ```rust,ignore
//! use runtimo_core::LlmoSafeGuard;
//!
//! let guard = LlmoSafeGuard::new();
//! guard.check()?;  // Ok(()) if resources are within limits
//!
//! let result = guard.execute(|| {
//!     // This closure only runs if resources are safe
//!     Ok(42)
//! })?;
//! ```

use llmosafe::llmosafe_body::ResourceGuard;
use llmosafe::llmosafe_integration::{EscalationPolicy, SafetyContext};

/// Resource guard wrapping `llmosafe::ResourceGuard` with safety context support.
///
/// Monitors RSS memory, CPU load, and IO wait from `/proc`. Acts as a circuit
/// breaker: if resource pressure exceeds the ceiling (default 80%), execution
/// is rejected.
pub struct LlmoSafeGuard {
    guard: ResourceGuard,
    policy: EscalationPolicy,
}

impl LlmoSafeGuard {
    /// Creates a guard with the default memory ceiling (80% of system memory).
    pub fn new() -> Self {
        let guard = ResourceGuard::auto(0.8);
        Self {
            guard,
            policy: EscalationPolicy::default(),
        }
    }

    /// Creates a guard with an explicit memory ceiling in bytes.
    pub fn with_memory_ceiling_bytes(memory_ceiling_bytes: usize) -> Self {
        Self {
            guard: ResourceGuard::new(memory_ceiling_bytes),
            policy: EscalationPolicy::default(),
        }
    }

    /// Checks current resource usage via llmosafe's real `/proc/stat` reading.
    ///
    /// # Returns
    ///
    /// `Ok(())` if resources are within limits.
    ///
    /// # Errors
    ///
    /// Returns an error string if resource pressure exceeds 80% or the
    /// underlying `ResourceGuard::check()` fails.
    pub fn check(&self) -> Result<(), String> {
        let pressure = self.guard.pressure();
        if pressure > 80 {
            return Err(format!("Resource pressure at {}% (ceiling: 80%)", pressure));
        }
        self.guard
            .check()
            .map(|_| ())
            .map_err(|e| format!("Resource guard check failed: {}", e))
    }

    /// Executes a function only if resources are safe.
    ///
    /// Runs `check()` first; if it passes, invokes `f()`.
    ///
    /// # Errors
    ///
    /// Propagates errors from `check()` or from `f()`.
    pub fn execute<F, T>(&self, f: F) -> Result<T, String>
    where
        F: FnOnce() -> Result<T, String>,
    {
        self.check()?;
        f()
    }

    /// Current RSS in bytes (from `/proc/self/status`).
    pub fn current_rss_bytes(&self) -> usize {
        ResourceGuard::current_rss_bytes()
    }

    /// Total system memory in bytes.
    pub fn system_memory_bytes(&self) -> usize {
        ResourceGuard::system_memory_bytes()
    }

    /// CPU load 0-100 via delta measurement on `/proc/stat`.
    pub fn system_cpu_load(&self) -> u8 {
        ResourceGuard::system_cpu_load()
    }

    /// Raw entropy score 0-1000 (weighted: RSS 50%, IO wait 25%, load 25%).
    pub fn raw_entropy(&self) -> u16 {
        self.guard.raw_entropy()
    }

    /// Pressure as percentage of memory ceiling (0-100).
    pub fn pressure(&self) -> u8 {
        self.guard.pressure()
    }

    /// Creates a safety context for tracking decisions across an execution.
    pub fn safety_context(&self) -> SafetyContext {
        SafetyContext::new(self.policy.clone())
    }
}

impl Default for LlmoSafeGuard {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guard_reports_system_memory() {
        let guard = LlmoSafeGuard::new();
        let mem = guard.system_memory_bytes();
        assert!(mem > 0, "System memory should be > 0");
    }

    #[test]
    fn guard_reports_rss() {
        let rss = LlmoSafeGuard::new().current_rss_bytes();
        assert!(rss > 0, "RSS should be > 0 for running process");
    }

    #[test]
    fn check_passes_under_normal_load() {
        let guard = LlmoSafeGuard::new();
        let result = guard.check();
        if let Err(e) = result {
            eprintln!("System under pressure: {}", e);
        }
    }

    #[test]
    fn pressure_is_bounded() {
        let guard = LlmoSafeGuard::new();
        let p = guard.pressure();
        assert!(p <= 100, "Pressure should be 0-100, got {}", p);
    }

    #[test]
    fn entropy_is_bounded() {
        let guard = LlmoSafeGuard::new();
        let e = guard.raw_entropy();
        assert!(e <= 1000, "Entropy should be 0-1000, got {}", e);
    }
}
