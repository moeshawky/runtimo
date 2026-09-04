//! Output rendering bounded from frozen `ResolvedConfig`.
//!
//! Derives defaults from `ResolvedConfig` (Gate 1) and applies CLI flag
//! overrides. No new opinions — only wire types (`human`/`json`/`plain`/`quiet`,
//! `plain`/`markdown`/`box`/`csv`). Color and emoji are never enabled by
//! default; `NO_COLOR` and `!is_terminal` force plain.

use runtimo_core::config::ResolvedConfig;
use std::io::IsTerminal;

use crate::format::{format_table, wall_to_markdown};

/// Valid output formats (wire types).
const VALID_FORMATS: &[&str] = &["human", "json", "plain", "quiet"];
/// Valid table styles (wire types).
const VALID_TABLE_STYLES: &[&str] = &["plain", "markdown", "box", "csv"];

/// Output rendering mode derived from `ResolvedConfig` + CLI flags.
///
/// All fields are wire types; no opinionated defaults beyond the frozen config.
/// Color/emoji default to `false` and are only enabled via explicit flags.
#[derive(Debug, Clone)]
#[allow(clippy::exhaustive_structs)]
pub struct OutputMode {
    /// Effective format: `human` | `json` | `plain` | `quiet`.
    pub format: String,
    /// Whether ANSI color is enabled.
    pub color: bool,
    /// Whether emoji is enabled.
    pub emoji: bool,
    /// Effective table style: `plain` | `markdown` | `box` | `csv`.
    pub table_style: String,
    /// Whether timestamps are enabled.
    pub timestamps: bool,
}

impl OutputMode {
    /// Creates an `OutputMode` from a frozen `ResolvedConfig` and CLI overrides.
    ///
    /// # Parameters
    /// - `resolved`: frozen `ResolvedConfig` (profile + file + env precedence)
    /// - `output`: CLI `--output` value (`human`|`json`|`plain`|`quiet`) if any
    /// - `color`: `--color` flag
    /// - `no_color`: `--no-color` flag (wins over `--color` and `NO_COLOR` env)
    /// - `emoji`: `--emoji` flag
    /// - `no_emoji`: `--no-emoji` flag (wins)
    /// - `table_style`: CLI `--table-style` value if any
    /// - `timestamps`: `--timestamps` flag
    /// - `no_timestamps`: `--no-timestamps` flag (wins)
    ///
    /// # Returns
    /// Resolved `OutputMode` with bounded defaults.
    #[must_use]
    #[allow(clippy::fn_params_excessive_bools)] // 6 bools map to 3 orthogonal CLI flag pairs (color/no_color, emoji/no_emoji, timestamps/no_timestamps); enum refactor would add churn without safety gain
    pub fn from_cli(
        resolved: &ResolvedConfig,
        output: Option<&str>,
        color: bool,
        no_color: bool,
        emoji: bool,
        no_emoji: bool,
        table_style: Option<&str>,
        timestamps: bool,
        no_timestamps: bool,
    ) -> Self {
        // Format: CLI > resolved.output_format
        let format = output.map_or_else(|| resolved.output_format.clone(), |s| s.to_lowercase());
        let format = if VALID_FORMATS.contains(&format.as_str()) {
            format
        } else {
            resolved.output_format.clone()
        };

        // Table style: CLI > resolved.output_renderer (markdown/plain) mapping.
        // Bare `minimal` defaults to `plain` to satisfy "bare run=no markdown".
        // Service/ephemeral honor the renderer.
        let style_from_resolved =
            if resolved.output_renderer == "markdown" && resolved.profile != "minimal" {
                "markdown".to_string()
            } else if resolved.output_renderer == "markdown" {
                // minimal profile: default plain unless explicitly configured via
                // CLI flag. This keeps bare runs plain while still honoring
                // explicit service/ephemeral configs.
                "plain".to_string()
            } else {
                "plain".to_string()
            };
        // If CLI provides table_style, it wins. Otherwise use derived.
        // For minimal with plain default, we still allow markdown when CLI
        // explicitly asks.
        let table_style_val = table_style.map_or(style_from_resolved, |s| s.to_lowercase());
        let table_style_val = if VALID_TABLE_STYLES.contains(&table_style_val.as_str()) {
            table_style_val
        } else {
            "plain".to_string()
        };

        // Color: --no-color wins; NO_COLOR env wins; !tty disables; only
        // --color explicitly enables.
        let color = Self::resolve_color(color, no_color);

        // Emoji: --no-emoji wins; only --emoji enables; never by default.
        let emoji = Self::resolve_emoji(emoji, no_emoji);

        // Timestamps: --no-timestamps wins; only --timestamps enables.
        let timestamps = Self::resolve_timestamps(timestamps, no_timestamps);

        Self {
            format,
            color,
            emoji,
            table_style: table_style_val,
            timestamps,
        }
    }

    /// Returns `true` if output is JSON (pure, pipable to `jq`).
    #[must_use]
    pub fn is_json(&self) -> bool {
        self.format == "json"
    }

    /// Returns `true` if output is quiet (silent except errors).
    #[must_use]
    pub fn is_quiet(&self) -> bool {
        self.format == "quiet"
    }

    /// Returns `true` if output is plain (no markdown).
    #[allow(dead_code)]
    #[must_use]
    pub fn is_plain(&self) -> bool {
        self.format == "plain"
    }

    /// Returns `true` if output is human-readable.
    #[must_use]
    pub fn is_human(&self) -> bool {
        self.format == "human"
    }

    /// Resolves color flag with invariants: `NO_COLOR` and `!tty` force off,
    /// `--no-color` wins.
    fn resolve_color(color: bool, no_color: bool) -> bool {
        if no_color {
            return false;
        }
        if std::env::var_os("NO_COLOR").is_some() {
            return false;
        }
        if !std::io::stdout().is_terminal() {
            return false;
        }
        if color {
            return true;
        }
        false
    }

    /// Resolves emoji flag: only explicit `--emoji` enables; `--no-emoji` wins.
    fn resolve_emoji(emoji: bool, no_emoji: bool) -> bool {
        if no_emoji {
            return false;
        }
        if emoji {
            return true;
        }
        false
    }

    /// Resolves timestamps flag.
    fn resolve_timestamps(timestamps: bool, no_timestamps: bool) -> bool {
        if no_timestamps {
            return false;
        }
        if timestamps {
            return true;
        }
        false
    }

    /// Returns the emoji icon for a job status, gated by `self.emoji`.
    ///
    /// When emoji is disabled, returns empty string. Handles both friendly
    /// (`completed`) and wire (`job_completed`) forms.
    #[must_use]
    pub fn status_icon(&self, status: &str) -> &'static str {
        if !self.emoji {
            return "";
        }
        match status {
            "running" | "started" | "job_started" => "🔄 ",
            "completed" | "job_completed" => "✅ ",
            "failed" | "job_failed" => "❌ ",
            _ => "❓ ",
        }
    }

    /// Renders text with markdown/ANSI gating.
    ///
    /// - When `self.is_json()` or `self.is_quiet()` the caller should bypass.
    /// - When `self.table_style == "markdown"` and `self.is_human()` the text
    ///   is passed through `wall_to_markdown`; otherwise returned verbatim.
    /// - When `self.color` is false, ANSI codes are stripped (we never emit
    ///   them in that case).
    #[must_use]
    pub fn render_text(&self, text: &str) -> String {
        if self.table_style == "markdown" && self.is_human() {
            wall_to_markdown(text)
        } else {
            text.to_string()
        }
    }

    /// Renders a table using `format_table` with the current `table_style`.
    ///
    /// Delegates to `crate::format::format_table(headers, rows, &self.table_style)`.
    #[must_use]
    pub fn render_table(&self, headers: &[&str], rows: &[Vec<String>]) -> String {
        format_table(headers, rows, &self.table_style)
    }

    /// Optionally wraps text in ANSI codes when `self.color` is true.
    ///
    /// When color is disabled, returns `text` unchanged (no ANSI).
    #[allow(dead_code)]
    #[must_use]
    pub fn paint(&self, text: &str, ansi_code: &str) -> String {
        if self.color {
            format!("\x1b[{ansi_code}m{text}\x1b[0m")
        } else {
            text.to_string()
        }
    }

    /// Creates a clone with an overridden format (e.g. forcing `json` from
    /// subcommand `--json` flag).
    #[must_use]
    pub fn with_format(&self, fmt: &str) -> Self {
        let mut c = self.clone();
        if VALID_FORMATS.contains(&fmt) {
            c.format = fmt.to_string();
        }
        c
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use runtimo_core::config::ResolvedConfig;

    fn minimal_resolved() -> ResolvedConfig {
        ResolvedConfig {
            profile: "minimal".to_string(),
            dal: "E".to_string(),
            wal_mode: "always".to_string(),
            backup_enabled: true,
            output_format: "human".to_string(),
            output_renderer: "markdown".to_string(),
            blocklist_enabled: true,
            session_max: 100,
            session_timeout: 3600,
            session_on_limit: "stop".to_string(),
            telemetry_enabled: true,
        }
    }

    #[test]
    fn bare_defaults_no_color_no_emoji_no_markdown() {
        let r = minimal_resolved();
        let m = OutputMode::from_cli(&r, None, false, false, false, false, None, false, false);
        assert_eq!(m.format, "human");
        assert!(!m.color);
        assert!(!m.emoji);
        // bare minimal should be plain despite resolved markdown
        assert_eq!(m.table_style, "plain");
    }

    #[test]
    fn emoji_enables_icon() {
        let r = minimal_resolved();
        let m = OutputMode::from_cli(&r, None, false, false, true, false, None, false, false);
        assert!(m.emoji);
        assert_eq!(m.status_icon("completed"), "✅ ");
        let m2 = OutputMode::from_cli(&r, None, false, false, false, false, None, false, false);
        assert_eq!(m2.status_icon("completed"), "");
    }

    #[test]
    fn no_color_wins_over_color() {
        let r = minimal_resolved();
        let m = OutputMode::from_cli(&r, None, true, true, false, false, None, false, false);
        assert!(!m.color);
    }

    #[test]
    fn output_json_passthrough() {
        let r = minimal_resolved();
        let m = OutputMode::from_cli(
            &r,
            Some("json"),
            false,
            false,
            false,
            false,
            None,
            false,
            false,
        );
        assert!(m.is_json());
    }

    #[test]
    fn table_style_via_cli() {
        let r = minimal_resolved();
        let m = OutputMode::from_cli(
            &r,
            None,
            false,
            false,
            false,
            false,
            Some("csv"),
            false,
            false,
        );
        assert_eq!(m.table_style, "csv");
    }
}
