//! Output formatting — wall-of-text to Markdown.
//!
//! Transforms raw terminal output into structured Markdown. Handles heading
//! detection, list continuation, paragraph grouping, and whitespace cleanup.
//!
//! This module operates on byte slices with bounds-checked indexing and
//! explicit arithmetic for position tracking — both patterns are intentional
//! and safe in this context.

#![allow(clippy::arithmetic_side_effects, clippy::indexing_slicing)]

use std::fmt::Write;

/// Render a table with headers and rows in the given style.
///
/// `headers` and each row's cells are rendered in four styles:
/// - `plain`: fixed-width columns padded with spaces, header separator line.
/// - `markdown`: GitHub Flavored Markdown `| header |` with `| --- |` separator.
/// - `box`: ASCII box `+---+` borders.
/// - `csv`: comma-separated with quoting for cells containing `,`, `"`, or newline.
///
/// Unknown style falls back to `plain`.
///
/// # Parameters
/// - `headers`: column headers
/// - `rows`: table body, each inner `Vec<String>` is a row
/// - `style`: one of `plain`|`markdown`|`box`|`csv` (case-insensitive)
///
/// # Returns
/// Rendered table string; empty when both headers and rows are empty.
#[must_use]
pub fn format_table(headers: &[&str], rows: &[Vec<String>], style: &str) -> String {
    if headers.is_empty() && rows.is_empty() {
        return String::new();
    }
    match style.to_lowercase().as_str() {
        "markdown" => format_table_markdown(headers, rows),
        "box" => format_table_box(headers, rows),
        "csv" => format_table_csv(headers, rows),
        _ => format_table_plain(headers, rows),
    }
}

/// Plain table: padded columns, header separator.
fn format_table_plain(headers: &[&str], rows: &[Vec<String>]) -> String {
    let cols = headers.len();
    if cols == 0 {
        return rows
            .iter()
            .map(|r| r.join("  "))
            .collect::<Vec<_>>()
            .join("\n");
    }
    let mut widths: Vec<usize> = headers.iter().map(|h| h.len()).collect();
    for row in rows {
        for (i, cell) in row.iter().enumerate().take(cols) {
            if i < widths.len() && cell.len() > widths[i] {
                widths[i] = cell.len();
            }
        }
    }
    let mut out = String::new();
    // Header
    for (i, h) in headers.iter().enumerate() {
        if i > 0 {
            out.push_str("  ");
        }
        let w = widths.get(i).copied().unwrap_or(0);
        let _ = write!(out, "{h:<w$}", w = w);
    }
    out.push('\n');
    // Separator
    for (i, w) in widths.iter().enumerate() {
        if i > 0 {
            out.push_str("  ");
        }
        out.push_str(&"-".repeat(*w));
    }
    if !rows.is_empty() {
        out.push('\n');
    }
    for (ri, row) in rows.iter().enumerate() {
        for i in 0..cols {
            if i > 0 {
                out.push_str("  ");
            }
            let cell = row.get(i).map_or("", String::as_str);
            let w = widths.get(i).copied().unwrap_or(0);
            let _ = write!(out, "{cell:<w$}", w = w);
        }
        if ri + 1 < rows.len() {
            out.push('\n');
        }
    }
    out
}

/// Markdown table: `| h |` / `| --- |` / `| c |`.
fn format_table_markdown(headers: &[&str], rows: &[Vec<String>]) -> String {
    if headers.is_empty() {
        return rows
            .iter()
            .map(|r| format!("| {} |", r.join(" | ")))
            .collect::<Vec<_>>()
            .join("\n");
    }
    let mut out = String::new();
    let _ = write!(out, "| {} |", headers.join(" | "));
    out.push('\n');
    let _ = write!(
        out,
        "| {} |",
        headers
            .iter()
            .map(|_| "---")
            .collect::<Vec<_>>()
            .join(" | ")
    );
    for row in rows {
        out.push('\n');
        let cells: Vec<String> = (0..headers.len())
            .map(|i| row.get(i).cloned().unwrap_or_default())
            .collect();
        let _ = write!(out, "| {} |", cells.join(" | "));
    }
    out
}

/// Box table: `+---+` borders with `| cell |`.
fn format_table_box(headers: &[&str], rows: &[Vec<String>]) -> String {
    let cols = headers.len();
    if cols == 0 {
        return String::new();
    }
    let mut widths: Vec<usize> = headers.iter().map(|h| h.len()).collect();
    for row in rows {
        for (i, cell) in row.iter().enumerate().take(cols) {
            if i < widths.len() && cell.len() > widths[i] {
                widths[i] = cell.len();
            }
        }
    }
    let border = {
        let mut b = String::from("+");
        for w in &widths {
            b.push_str(&"-".repeat(w + 2));
            b.push('+');
        }
        b
    };
    let mut out = String::new();
    out.push_str(&border);
    out.push('\n');
    // Header row
    out.push('|');
    for (i, h) in headers.iter().enumerate() {
        let w = widths.get(i).copied().unwrap_or(0);
        let _ = write!(out, " {h:<w$} |", w = w);
    }
    out.push('\n');
    out.push_str(&border);
    out.push('\n');
    for row in rows {
        out.push('|');
        for i in 0..cols {
            let cell = row.get(i).map_or("", String::as_str);
            let w = widths.get(i).copied().unwrap_or(0);
            let _ = write!(out, " {cell:<w$} |", w = w);
        }
        out.push('\n');
    }
    out.push_str(&border);
    out
}

/// CSV table: comma-separated, quoted when needed.
fn format_table_csv(headers: &[&str], rows: &[Vec<String>]) -> String {
    let mut out = String::new();
    if !headers.is_empty() {
        out.push_str(
            &headers
                .iter()
                .map(|h| csv_escape(h))
                .collect::<Vec<_>>()
                .join(","),
        );
        if !rows.is_empty() {
            out.push('\n');
        }
    }
    for (ri, row) in rows.iter().enumerate() {
        let cells: Vec<String> = row.iter().map(|c| csv_escape(c)).collect();
        out.push_str(&cells.join(","));
        if ri + 1 < rows.len() {
            out.push('\n');
        }
    }
    out
}

fn csv_escape(s: &str) -> String {
    if s.contains(',') || s.contains('"') || s.contains('\n') {
        let escaped = s.replace('"', "\"\"");
        format!("\"{escaped}\"")
    } else {
        s.to_string()
    }
}

/// Convert raw wall-of-text into Markdown.
///
/// Detects headings (all-caps lines, `Title:` patterns), bullet/numbered
/// lists with multi-line continuation, and groups remaining lines into
/// paragraphs. Empty input or whitespace-only returns `""`.
#[allow(clippy::indexing_slicing)]
#[must_use]
pub fn wall_to_markdown(text: &str) -> String {
    if text.trim().is_empty() {
        return String::new();
    }

    let lines: Vec<&str> = text.lines().collect();
    let mut output: Vec<String> = Vec::new();
    let mut i = 0;

    while i < lines.len() {
        let line = lines[i].trim();

        if line.is_empty() {
            output.push(String::new());
            i += 1;
            continue;
        }

        // Heading detection
        if is_heading(line) {
            output.push(heading_line(line));
            i += 1;
            continue;
        }

        // List detection
        if is_list_start(lines[i]) {
            let mut items = vec![lines[i].to_string()];
            i += 1;
            // Multi-line continuation
            while i < lines.len() {
                let next = lines[i].trim();
                if next.is_empty() || is_heading(next) || is_list_start(lines[i]) {
                    break;
                }
                items.push(format!("  {}", next));
                i += 1;
            }
            output.extend(items);
            continue;
        }

        // Paragraph
        let mut para: Vec<&str> = Vec::new();
        while i < lines.len() {
            let current = lines[i].trim();
            if current.is_empty() || is_heading(current) || is_list_start(lines[i]) {
                break;
            }
            para.push(current);
            i += 1;
        }
        if !para.is_empty() {
            output.push(para.join(" "));
            output.push(String::new());
        }
    }

    // Join and normalize
    let mut md = output.join("\n");
    md = md.trim().to_string();
    // Collapse multiple blank lines
    let mut result = String::with_capacity(md.len());
    let mut blanks = 0;
    for line in md.lines() {
        if line.trim().is_empty() {
            blanks += 1;
            if blanks <= 2 {
                result.push('\n');
            }
        } else {
            blanks = 0;
            result.push_str(line);
            result.push('\n');
        }
    }
    md = result.trim().to_string();

    // Top-level title: if first line looks like a title, promote to h1
    // Only if there isn't already a heading at the start
    if let Some(first) = md.lines().next() {
        let stripped = first.trim_start_matches('#').trim();
        if !stripped.is_empty()
            && !first.starts_with('#')
            && (stripped.ends_with(':')
                || stripped
                    .chars()
                    .all(|c| c.is_uppercase() || c.is_whitespace()))
            && stripped.len() < 80
        {
            md = format!("# {}\n\n{}", stripped.trim_end_matches(':'), md);
        }
    }

    md
}

fn is_heading(line: &str) -> bool {
    let line = line.trim();
    if line.is_empty() {
        return false;
    }
    line.ends_with(':') && line.len() < 90
        || line
            .chars()
            .all(|c| c.is_uppercase() || c.is_whitespace() || c.is_ascii_digit())
            && line.len() > 1
        || starts_with_number_dot(line)
        || is_title_case(line)
}

fn heading_line(line: &str) -> String {
    let line = line.trim();
    let level = if line
        .chars()
        .all(|c| c.is_uppercase() || c.is_whitespace() || c.is_ascii_digit() || c == '.')
    {
        "#"
    } else {
        "##"
    };
    format!("{} {}", level, line.trim_end_matches(':'))
}

#[allow(clippy::indexing_slicing)]
fn starts_with_number_dot(line: &str) -> bool {
    let bytes = line.as_bytes();
    let mut i = 0;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
    }
    i > 0 && i < bytes.len() && bytes[i] == b'.' && i + 1 < bytes.len() && bytes[i + 1] == b' '
}

fn is_title_case(line: &str) -> bool {
    let line = line.trim();
    if line.len() < 3 || line.len() > 70 {
        return false;
    }
    #[allow(clippy::indexing_slicing)] // is_ascii_uppercase needs first byte only
    let bytes = line.as_bytes();
    if !bytes[0].is_ascii_uppercase() {
        return false;
    }
    bytes.iter().all(|&b| {
        b.is_ascii_alphanumeric() || b == b' ' || b == b'&' || b == b'/' || b == b'\\' || b == b'-'
    })
}

fn is_list_start(line: &str) -> bool {
    let trimmed = line.trim_start();
    // Bullet: - * or •
    if trimmed.starts_with("- ") || trimmed.starts_with("* ") || trimmed.starts_with("• ") {
        return true;
    }
    // Numbered: 1. 1) etc
    let bytes = trimmed.as_bytes();
    let mut i = 0;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
    }
    i > 0
        && i < bytes.len()
        && (bytes[i] == b'.' || bytes[i] == b')')
        && i + 1 < bytes.len()
        && bytes[i + 1] == b' '
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_input() {
        assert_eq!(wall_to_markdown(""), "");
        assert_eq!(wall_to_markdown("   \n  \n"), "");
    }

    #[test]
    fn heading_detection() {
        let md = wall_to_markdown("SYSTEM:\ncpu: amd");
        assert!(md.contains("# SYSTEM"));
        assert!(md.contains("cpu: amd"));
    }

    #[test]
    fn bullet_list() {
        let md = wall_to_markdown("- item 1\n- item 2");
        assert!(md.contains("- item 1"));
        assert!(md.contains("- item 2"));
    }

    #[test]
    fn numbered_list() {
        let md = wall_to_markdown("1. first\n2. second");
        assert!(md.contains("1. first"));
        assert!(md.contains("2. second"));
    }

    #[test]
    fn paragraph_grouping() {
        let md = wall_to_markdown("line one\nline two\n\nnew para");
        assert!(md.contains("line one line two"));
        assert!(md.contains("new para"));
    }

    #[test]
    fn multi_line_list_item() {
        let md = wall_to_markdown("- item one\n  continuation\n  more\n- item two");
        assert!(md.contains("- item one\n  continuation\n  more"));
        assert!(md.contains("- item two"));
    }

    #[test]
    fn real_world_telemetry() {
        let input = "============================================================\n RUNTIMO TELEMETRY [12345]\n============================================================\n\n--- SYSTEM ---\n CPU   : AMD EPYC\n RAM   : 30Gi total, 8Gi free\n\n--- SERVICES ---\n Services: none detected";
        let md = wall_to_markdown(input);
        assert!(!md.is_empty());
        assert!(md.contains("RUNTIMO TELEMETRY"));
    }
}
