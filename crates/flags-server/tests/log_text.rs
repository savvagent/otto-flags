//! No `tracing` event above debug carries an error's text (#29, #35).
//!
//! Database, platform, and serde text can carry SQL, schema names, response
//! bodies, URLs, and request-body values, so the rule is: a text-free reason
//! (and code or status) at info and above, the text only at debug. Most of the
//! sites that log a failure are reached only when a database or the platform
//! fails at runtime, so instead of a test per site this scans the source of
//! every workspace crate for the field forms that carry text.

use std::path::{Path, PathBuf};

/// Field forms that put an error's or a remote party's text in an event.
const TEXT_FIELDS: &[&str] = &["error = %", "error = ?", "reason = %", "%reason"];

/// Known exceptions, by file and the exact field: the platform's per-event
/// rejection reason is its answer to a successful call, kept for operators
/// and stored in `usage_outbox_rejected`.
const ALLOWED: &[(&str, &str)] = &[("flags-core/src/usage.rs", "reason = %r.reason")];

#[test]
fn no_event_above_debug_logs_error_text() {
    let crates = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates dir")
        .to_path_buf();
    let mut files = Vec::new();
    for entry in std::fs::read_dir(&crates).expect("read crates") {
        let src = entry.expect("entry").path().join("src");
        if src.is_dir() {
            rust_files(&src, &mut files);
        }
    }
    assert!(files.len() > 10, "found too few sources: {files:?}");

    let mut found = Vec::new();
    for file in &files {
        let source = std::fs::read_to_string(file).expect("read source");
        let name = file
            .strip_prefix(&crates)
            .expect("under crates")
            .to_string_lossy()
            .replace('\\', "/");
        for args in events_above_debug(&source) {
            let flat = args.split_whitespace().collect::<Vec<_>>().join(" ");
            let allowed = ALLOWED
                .iter()
                .any(|(f, field)| name == *f && flat.contains(field));
            if !allowed && TEXT_FIELDS.iter().any(|t| flat.contains(t)) {
                found.push(format!("{name}: {flat}"));
            }
        }
    }
    assert!(
        found.is_empty(),
        "log the text only at debug, a reason above it:\n{}",
        found.join("\n")
    );
}

#[test]
fn the_scan_sees_wrapped_calls_and_ignores_debug() {
    let source = r#"
        tracing::warn!(
            reason,
            error = %e,
            "wrapped (with a paren in the message)"
        );
        tracing::debug!(error = %e, "fine");
        tracing::info!(rows = n, "fine");
    "#;
    let events = events_above_debug(source);
    assert_eq!(events.len(), 2, "{events:?}");
    assert!(events[0].contains("error = %e"));
}

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).expect("read dir") {
        let path = entry.expect("entry").path();
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// The argument text of every `tracing::{error,warn,info}!(…)` call, up to its
/// matching parenthesis; parentheses inside string literals do not count.
fn events_above_debug(source: &str) -> Vec<String> {
    let mut events = Vec::new();
    for macro_name in ["tracing::error!(", "tracing::warn!(", "tracing::info!("] {
        let mut from = 0;
        while let Some(at) = source[from..].find(macro_name) {
            let start = from + at + macro_name.len();
            let end = matching_paren(&source[start..]).map_or(source.len(), |n| start + n);
            events.push(source[start..end].to_string());
            from = end;
        }
    }
    events
}

/// The byte offset of the `)` closing an already-open parenthesis.
fn matching_paren(s: &str) -> Option<usize> {
    let (mut depth, mut in_str, mut escaped) = (1usize, false, false);
    for (i, c) in s.char_indices() {
        if in_str {
            match c {
                _ if escaped => escaped = false,
                '\\' => escaped = true,
                '"' => in_str = false,
                _ => {}
            }
            continue;
        }
        match c {
            '"' => in_str = true,
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
    }
    None
}
