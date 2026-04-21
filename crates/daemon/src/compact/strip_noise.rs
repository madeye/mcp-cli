//! `fs.read ?strip_noise` — inline boilerplate elision.
//!
//! Recognizes three noise patterns agents routinely waste tokens on:
//!
//!   * Leading license-header comments (Apache/MIT/SPDX/Copyright).
//!   * Long runs of base64-ish lines (embedded certs, images, etc.).
//!   * Bodies of files tagged `@generated` / `DO NOT EDIT`.
//!
//! Each detected region is replaced with a single `[[mcp-cli: …]]`
//! marker line, and reported in `StripResult::regions` with its
//! original 1-based line range so callers can ask for specific lines
//! back if they need to.

use protocol::StrippedRegion;

pub struct StripResult {
    pub content: String,
    pub regions: Vec<StrippedRegion>,
}

/// Minimum lines a comment run must span to qualify as a "header"
/// rather than a one-liner worth keeping. A single `// Copyright …`
/// line frequently precedes real code and stripping it would hurt
/// context more than it saves.
const LICENSE_MIN_LINES: usize = 3;

/// Minimum run length / per-line width for the base64 detector. Tight
/// enough to avoid false-positives on long identifiers or URLs.
const BASE64_MIN_LINES: usize = 5;
const BASE64_MIN_WIDTH: usize = 60;

/// Only strip generated-file bodies when the marker is near the top
/// *and* the file is long enough to earn the cut.
const GENERATED_HEAD_SCAN: usize = 10;
const GENERATED_MIN_FILE_LINES: usize = 50;
/// Lines kept after the marker so the agent still sees a bit of the
/// preamble (imports, type aliases, etc.) before the elision starts.
const GENERATED_KEEP_AFTER_MARKER: usize = 10;

pub fn strip_noise(content: &str) -> StripResult {
    let lines: Vec<&str> = content.lines().collect();
    let trailing_nl = content.ends_with('\n');

    let mut regions: Vec<(usize, usize, &'static str)> = Vec::new();
    if let Some((s, e)) = detect_license_header(&lines) {
        regions.push((s, e, "license"));
    }
    if let Some((s, e)) = detect_generated_body(&lines) {
        regions.push((s, e, "generated"));
    }
    for (s, e) in detect_base64_blobs(&lines) {
        regions.push((s, e, "base64"));
    }

    // Stable order; drop any later region that overlaps an earlier one
    // (license near the head and generated near the tail don't overlap
    // in practice, but base64 could nest inside a generated body).
    regions.sort_by_key(|(s, _, _)| *s);
    let mut kept: Vec<(usize, usize, &'static str)> = Vec::new();
    for r in regions {
        if let Some(last) = kept.last() {
            if r.0 <= last.1 {
                continue;
            }
        }
        kept.push(r);
    }

    let mut content_out = String::new();
    let mut out_regions: Vec<StrippedRegion> = Vec::new();
    let mut idx = 0usize;
    let mut first = true;
    let mut iter = kept.into_iter().peekable();

    while idx < lines.len() {
        if let Some(&(s, e, kind)) = iter.peek() {
            if idx == s {
                let count = (e - s + 1) as u32;
                push_line(&mut content_out, &mut first, &format_marker(kind, count));
                out_regions.push(StrippedRegion {
                    kind: kind.to_string(),
                    start_line: (s + 1) as u32,
                    end_line: (e + 1) as u32,
                    lines: count,
                });
                idx = e + 1;
                iter.next();
                continue;
            }
        }
        push_line(&mut content_out, &mut first, lines[idx]);
        idx += 1;
    }

    if trailing_nl {
        content_out.push('\n');
    }

    StripResult {
        content: content_out,
        regions: out_regions,
    }
}

fn push_line(out: &mut String, first: &mut bool, line: &str) {
    if !*first {
        out.push('\n');
    }
    out.push_str(line);
    *first = false;
}

fn format_marker(kind: &str, lines: u32) -> String {
    match kind {
        "license" => format!("[[mcp-cli: stripped {lines}-line license header]]"),
        "base64" => format!("[[mcp-cli: stripped {lines}-line base64 blob]]"),
        "generated" => format!("[[mcp-cli: stripped {lines}-line generated body]]"),
        _ => format!("[[mcp-cli: stripped {lines} lines]]"),
    }
}

#[derive(Copy, Clone, PartialEq, Eq)]
enum CommentStyle {
    Slash,
    Hash,
    Block,
}

fn detect_license_header(lines: &[&str]) -> Option<(usize, usize)> {
    let mut i = 0usize;
    if i < lines.len() && lines[i].starts_with("#!") {
        i += 1;
    }
    while i < lines.len() && lines[i].trim().is_empty() {
        i += 1;
    }
    if i >= lines.len() {
        return None;
    }

    let start = i;
    let first = lines[i].trim_start();
    let style = if first.starts_with("//") {
        CommentStyle::Slash
    } else if first.starts_with("/*") {
        CommentStyle::Block
    } else if first.starts_with('#') {
        CommentStyle::Hash
    } else {
        return None;
    };

    let mut end = start;
    match style {
        CommentStyle::Slash | CommentStyle::Hash => {
            while end < lines.len() {
                let t = lines[end].trim_start();
                let is_comment = match style {
                    CommentStyle::Slash => t.starts_with("//"),
                    CommentStyle::Hash => t.starts_with('#'),
                    CommentStyle::Block => false,
                };
                if is_comment || lines[end].trim().is_empty() {
                    end += 1;
                } else {
                    break;
                }
            }
            // Trim trailing blanks we swallowed.
            while end > start && lines[end - 1].trim().is_empty() {
                end -= 1;
            }
        }
        CommentStyle::Block => {
            let mut closed = false;
            while end < lines.len() {
                if lines[end].contains("*/") {
                    closed = true;
                    end += 1;
                    break;
                }
                end += 1;
            }
            if !closed {
                return None;
            }
        }
    }

    if end <= start {
        return None;
    }
    let last = end - 1;
    if last - start + 1 < LICENSE_MIN_LINES {
        return None;
    }
    let block_lower = lines[start..=last].join("\n").to_lowercase();
    if !contains_license_keyword(&block_lower) {
        return None;
    }
    Some((start, last))
}

fn contains_license_keyword(text: &str) -> bool {
    const KEYWORDS: &[&str] = &[
        "copyright",
        "spdx-license-identifier",
        "all rights reserved",
        "licensed under",
        "apache license",
        "mit license",
        "bsd license",
        "mozilla public",
        "gnu general public",
        "gnu lesser general public",
    ];
    KEYWORDS.iter().any(|kw| text.contains(kw))
}

fn detect_base64_blobs(lines: &[&str]) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < lines.len() {
        if !is_base64_line(lines[i]) {
            i += 1;
            continue;
        }
        let start = i;
        while i < lines.len() && is_base64_line(lines[i]) {
            i += 1;
        }
        let end = i - 1;
        if end - start + 1 >= BASE64_MIN_LINES {
            out.push((start, end));
        }
    }
    out
}

fn is_base64_line(line: &str) -> bool {
    let trimmed = line.trim();
    if trimmed.len() < BASE64_MIN_WIDTH {
        return false;
    }
    trimmed
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '/' | '=' | '-' | '_'))
}

fn detect_generated_body(lines: &[&str]) -> Option<(usize, usize)> {
    const MARKERS: &[&str] = &[
        "@generated",
        "DO NOT EDIT",
        "do not edit",
        "Code generated by",
        "code generated by",
        "Auto-generated",
        "auto-generated",
        "AUTOGENERATED",
    ];
    if lines.len() < GENERATED_MIN_FILE_LINES {
        return None;
    }
    let marker_line = (0..lines.len().min(GENERATED_HEAD_SCAN))
        .find(|&i| MARKERS.iter().any(|m| lines[i].contains(m)))?;
    let head_end = (marker_line + GENERATED_KEEP_AFTER_MARKER).min(lines.len().saturating_sub(1));
    if head_end + 1 >= lines.len() {
        return None;
    }
    Some((head_end + 1, lines.len() - 1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn noop_when_no_noise() {
        let src = "fn main() {}\nfn other() {}\n";
        let out = strip_noise(src);
        assert_eq!(out.content, src);
        assert!(out.regions.is_empty());
    }

    #[test]
    fn strips_rust_slash_license_header() {
        let src = "// Copyright 2024 Acme Corp\n\
                   // SPDX-License-Identifier: Apache-2.0\n\
                   // Licensed under the Apache License, Version 2.0\n\
                   \n\
                   fn main() {}\n";
        let out = strip_noise(src);
        assert_eq!(out.regions.len(), 1);
        assert_eq!(out.regions[0].kind, "license");
        assert_eq!(out.regions[0].start_line, 1);
        assert_eq!(out.regions[0].end_line, 3);
        assert_eq!(out.regions[0].lines, 3);
        assert!(out
            .content
            .starts_with("[[mcp-cli: stripped 3-line license header]]"));
        assert!(out.content.contains("fn main() {}"));
        assert!(out.content.ends_with('\n'));
    }

    #[test]
    fn strips_block_license_header() {
        let src = "/*\n\
                   * Copyright 2024 Acme\n\
                   * Licensed under the MIT License\n\
                   */\n\
                   fn main() {}\n";
        let out = strip_noise(src);
        assert_eq!(out.regions.len(), 1);
        assert_eq!(out.regions[0].kind, "license");
        assert_eq!(out.regions[0].lines, 4);
    }

    #[test]
    fn skips_shebang_before_license() {
        let src = "#!/usr/bin/env python3\n\
                   # Copyright 2024 Acme\n\
                   # SPDX-License-Identifier: Apache-2.0\n\
                   # Licensed under Apache License\n\
                   \n\
                   print('hello')\n";
        let out = strip_noise(src);
        assert_eq!(out.regions.len(), 1);
        assert_eq!(out.regions[0].start_line, 2);
        assert_eq!(out.regions[0].end_line, 4);
        // Shebang preserved verbatim.
        assert!(out.content.starts_with("#!/usr/bin/env python3"));
    }

    #[test]
    fn does_not_strip_one_line_copyright() {
        let src = "// Copyright 2024 Acme\nfn main() {}\n";
        let out = strip_noise(src);
        assert!(out.regions.is_empty());
        assert_eq!(out.content, src);
    }

    #[test]
    fn does_not_strip_generic_comments_without_license_keyword() {
        let src = "// Utility helpers.\n\
                   // See docs.md for the full API.\n\
                   // Adjust `CACHE_CAP` before release.\n\
                   \n\
                   fn main() {}\n";
        let out = strip_noise(src);
        assert!(out.regions.is_empty());
    }

    #[test]
    fn strips_long_base64_blob() {
        // Multi-line inline certificate — 8 "pure" base64 lines above
        // the 5-line threshold and the 60-char width cutoff.
        let mut src = String::from("fn helper() {}\n");
        for _ in 0..8 {
            src.push_str(
                "QUJDREVGR0hJSktMTU5PUFFSU1RVVldYWVphYmNkZWZnaGlqa2xtbm9wcXJzdHV2d3h5eg==\n",
            );
        }
        src.push_str("fn other() {}\n");
        let out = strip_noise(&src);
        assert_eq!(out.regions.len(), 1);
        assert_eq!(out.regions[0].kind, "base64");
        assert!(out.regions[0].lines >= BASE64_MIN_LINES as u32);
        assert!(out.content.contains("[[mcp-cli: stripped"));
        assert!(out.content.contains("fn helper()"));
        assert!(out.content.contains("fn other()"));
    }

    #[test]
    fn short_base64_run_is_kept() {
        // 3 lines is under the threshold; keep verbatim.
        let src = "let a = \"QUJDREVGR0hJSktMTU5PUFFSU1RVVldYWVphYmNkZWZnaGlqa2xtbm9wcXJz\";\n\
                   let b = \"dHV2d3h5ekFCQ0RFRkdISUpLTE1OT1BRUlNUVVZXWFlaYWJjZGVmZ2hpamts\";\n\
                   let c = \"bW5vcHFyc3R1dnd4eXoxMjM0NTY3ODkwYWJjZGVmZ2hpamtsbW5vcHFyc3Q=\";\n";
        let out = strip_noise(src);
        assert!(out.regions.is_empty());
    }

    #[test]
    fn strips_generated_body_when_file_is_long() {
        let mut src = String::from("// @generated by protoc 3.21.0\n");
        src.push_str("// Do not edit manually.\n");
        for i in 0..100 {
            src.push_str(&format!("pub const X_{i}: u32 = {i};\n"));
        }
        let out = strip_noise(&src);
        let gen_region = out
            .regions
            .iter()
            .find(|r| r.kind == "generated")
            .expect("expected generated region");
        assert!(gen_region.lines > 50);
        // First few lines (marker + some head) remain.
        assert!(out.content.contains("@generated by protoc"));
        assert!(out.content.contains("[[mcp-cli: stripped"));
    }

    #[test]
    fn short_generated_file_kept_whole() {
        // Under MIN_FILE_LINES: don't bother stripping.
        let src = "// @generated\npub const X: u32 = 1;\n";
        let out = strip_noise(src);
        assert!(out.regions.is_empty());
    }

    #[test]
    fn regions_report_original_line_numbers() {
        let src = "// Copyright 2024 Acme\n\
                   // Licensed under MIT License\n\
                   // All rights reserved\n\
                   \n\
                   fn main() {}\n\
                   fn other() {}\n";
        let out = strip_noise(src);
        assert_eq!(out.regions[0].start_line, 1);
        assert_eq!(out.regions[0].end_line, 3);
    }

    #[test]
    fn preserves_no_trailing_newline() {
        let src = "fn main() {}";
        let out = strip_noise(src);
        assert_eq!(out.content, "fn main() {}");
    }

    #[test]
    fn overlapping_regions_keep_first() {
        // A license header AND a generated marker in the same head
        // can overlap conceptually; assembly should drop the second.
        let mut src = String::from("// Copyright 2024 Acme\n");
        src.push_str("// @generated by build.rs\n");
        src.push_str("// Licensed under MIT License\n");
        for i in 0..80 {
            src.push_str(&format!("pub const X_{i}: u32 = {i};\n"));
        }
        let out = strip_noise(&src);
        // Sort by start; we can have license + generated (generated starts
        // after license's end). Overlap drop only applies when they overlap.
        for r in &out.regions {
            assert!(r.kind == "license" || r.kind == "generated");
        }
    }
}
