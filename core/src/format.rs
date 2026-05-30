//! Output formatting — wall-of-text to Markdown.
//!
//! Transforms raw terminal output into structured Markdown. Handles heading
//! detection, list continuation, paragraph grouping, and whitespace cleanup.
//!
//! # Example
//!
//! ```rust,ignore
//! use runtimo_core::format::wall_to_markdown;
//!
//! let md = wall_to_markdown("SYSTEM:\ncpu: amd\nram: 16G");
//! assert!(md.contains("# SYSTEM"));
//! ```

/// Convert raw wall-of-text into Markdown.
///
/// Detects headings (all-caps lines, `Title:` patterns), bullet/numbered
/// lists with multi-line continuation, and groups remaining lines into
/// paragraphs. Empty input or whitespace-only returns `""`.
#[allow(clippy::indexing_slicing)]
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
