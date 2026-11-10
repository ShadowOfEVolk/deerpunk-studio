//! Build diagnostics (rustc/cargo errors, warnings and notes) as a
//! Problems-panel feed. The runner's build steps invoke cargo with
//! `--message-format=json`: stdout becomes a stream of JSON lines whose
//! compiler messages carry the parsed fields (`level`, `code`, primary
//! `spans`) plus the human-readable `rendered` snippet. Cargo's own
//! progress lines stay on stderr and keep flowing to the console.
//!
//! Parsed [`Diag`]s are pushed into [`DIAG_SINK`]; the UI drains it on its
//! own cadence. The sink is a plain `std::sync::mpsc` pair behind a
//! `LazyLock` — no new dependencies, thread-safe by construction (the
//! runner thread pushes from one side, the UI drains from the other).

use std::sync::{LazyLock, Mutex, mpsc};

use serde::{Deserialize, Serialize};

/// How bad a diagnostic is. rustc levels collapse to UI-relevant buckets:
/// `error` (and ICE headings) → [`DiagSeverity::Error`], `warning` →
/// [`DiagSeverity::Warning`], everything else (`note`, `help`,
/// `failure-note`) → [`DiagSeverity::Note`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DiagSeverity {
    Error,
    Warning,
    Note,
}

/// One compiler diagnostic: headline, location and the full rendering.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Diag {
    pub severity: DiagSeverity,
    pub message: String,
    /// Human-readable rustc rendering (the annotated source snippet), kept
    /// next to the parsed fields for the UI's expandable detail view.
    pub rendered: Option<String>,
    pub file: Option<String>,
    /// 1-based, from the diagnostic's primary span (first span otherwise).
    pub line: Option<u32>,
    pub col: Option<u32>,
    /// rustc lint/Diag id (`unused_variables`, `E0382`), if any.
    pub code: Option<String>,
}

/// Opaque handle to the diagnostics channel; only [`push_diags`] and
/// [`drain_diags`] use it.
pub struct Sink {
    tx: mpsc::Sender<Diag>,
    rx: Mutex<mpsc::Receiver<Diag>>,
}

/// Process-global diagnostics channel: build steps push, the UI drains.
pub static DIAG_SINK: LazyLock<Sink> = LazyLock::new(|| {
    let (tx, rx) = mpsc::channel();
    Sink {
        tx,
        rx: Mutex::new(rx),
    }
});

/// Queue diagnostics from a build step. Unbounded channel, so this never
/// blocks the runner; a disconnected receiver (nobody drains anymore)
/// silently drops the batch.
pub fn push_diags(iter: impl IntoIterator<Item = Diag>) {
    for diag in iter {
        let _ = DIAG_SINK.tx.send(diag);
    }
}

/// Take every diagnostic pushed so far (oldest first); non-blocking.
pub fn drain_diags() -> Vec<Diag> {
    let rx = DIAG_SINK.rx.lock().expect("DIAG_SINK rx poisoned");
    let mut out = Vec::new();
    while let Ok(diag) = rx.try_recv() {
        out.push(diag);
    }
    out
}

/// Parse one cargo stdout JSON line into a [`Diag`]. Returns `None` for
/// everything that is not a compiler message (`build-finished`,
/// `compiler-artifact`, `build-script-executed`, garbage) — cargo's stdout
/// is JSON-only with the JSON message format, but robustness is free.
pub fn parse_compiler_message(line: &str) -> Option<Diag> {
    let value: serde_json::Value = serde_json::from_str(line).ok()?;
    if value.get("reason")?.as_str()? != "compiler-message" {
        return None;
    }
    let message = value.get("message")?;
    let severity = match message.get("level")?.as_str()? {
        // ICE headings arrive as "error: internal compiler error …".
        level if level.starts_with("error") => DiagSeverity::Error,
        "warning" => DiagSeverity::Warning,
        _ => DiagSeverity::Note,
    };
    let text = message.get("message")?.as_str()?.to_string();
    let code = message
        .get("code")
        .and_then(|code| code.get("code"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_string);
    let rendered = message
        .get("rendered")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string);
    let span = message
        .get("spans")
        .and_then(serde_json::Value::as_array)
        .and_then(|spans| {
            spans
                .iter()
                .find(|span| {
                    span.get("is_primary").and_then(serde_json::Value::as_bool) == Some(true)
                })
                .or_else(|| spans.first())
        });
    let (file, line, col) = span
        .map(|span| {
            (
                span.get("file_name")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string),
                span.get("line_start")
                    .and_then(serde_json::Value::as_u64)
                    .map(|l| l as u32),
                span.get("column_start")
                    .and_then(serde_json::Value::as_u64)
                    .map(|c| c as u32),
            )
        })
        .unwrap_or((None, None, None));
    Some(Diag {
        severity,
        message: text,
        rendered,
        file,
        line,
        col,
        code,
    })
}

#[cfg(test)]
mod tests {
    use crate::diagnostics::{Diag, DiagSeverity, drain_diags, parse_compiler_message};

    /// Captured verbatim from `cargo build --message-format=json` on an
    /// unused-variable warning (cargo 1.99.1): the exact wrap shape the
    /// runner parses — `reason: compiler-message` with the rustc diagnostic
    /// (level, code, spans, rendered) inside `message`.
    const WARNING_LINE: &str = r#"{"reason":"compiler-message","package_id":"path+file:///private/tmp/diagfix#0.1.0","manifest_path":"/private/tmp/diagfix/Cargo.toml","target":{"kind":["bin"],"crate_types":["bin"],"name":"diagfix","src_path":"/private/tmp/diagfix/src/main.rs","edition":"2021","doc":true,"doctest":false,"test":true},"message":{"rendered":"warning: unused variable: `unused_x`\n --> src/main.rs:2:9\n  |\n2 |     let unused_x = 5;\n  |         ^^^^^^^^ help: if this is intentional, prefix it with an underscore: `_unused_x`\n  |\n  = note: `#[warn(unused_variables)]` (part of `#[warn(unused)]`) on by default\n\n","$message_type":"diagnostic","children":[{"children":[],"code":null,"level":"note","message":"`#[warn(unused_variables)]` (part of `#[warn(unused)]`) on by default","spans":[],"rendered":null}],"code":{"code":"unused_variables","explanation":null},"level":"warning","message":"unused variable: `unused_x`","spans":[{"file_name":"src/main.rs","byte_start":25,"byte_end":33,"line_start":2,"line_end":2,"column_start":9,"column_end":17,"is_primary":true,"text":[{"highlight_start":9,"highlight_end":17,"text":"    let unused_x = 5;"}],"label":null,"suggested_replacement":null,"suggestion_applicability":null,"expansion":null}]}}"#;

    #[test]
    fn parses_real_cargo_warning_line() {
        let diag = parse_compiler_message(WARNING_LINE).expect("warning line must parse");
        assert_eq!(diag.severity, DiagSeverity::Warning);
        assert_eq!(diag.message, "unused variable: `unused_x`");
        assert_eq!(diag.file.as_deref(), Some("src/main.rs"));
        assert_eq!(diag.line, Some(2));
        assert_eq!(diag.col, Some(9));
        assert_eq!(diag.code.as_deref(), Some("unused_variables"));
        let rendered = diag.rendered.expect("rendered present");
        assert!(rendered.contains("unused variable: `unused_x`"));
        assert!(rendered.contains("--> src/main.rs:2:9"));
    }

    #[test]
    fn non_compiler_lines_are_skipped() {
        // The rest of cargo's stdout stream.
        assert!(parse_compiler_message(r#"{"reason":"build-finished","success":true}"#).is_none());
        assert!(parse_compiler_message(
            r#"{"reason":"compiler-artifact","target":{"kind":["bin"]}}"#
        )
        .is_none());
        // And whatever a broken tool manages to print there.
        assert!(parse_compiler_message("").is_none());
        assert!(parse_compiler_message("not json at all").is_none());
    }

    #[test]
    fn severity_buckets() {
        let make = |level: &str| {
            format!(
                r#"{{"reason":"compiler-message","message":{{"level":"{level}","message":"m","spans":[]}}}}"#
            )
        };
        assert_eq!(
            parse_compiler_message(&make("error")).map(|d| d.severity),
            Some(DiagSeverity::Error)
        );
        // ICE headings carry the words in the level string itself.
        assert_eq!(
            parse_compiler_message(&make("error: internal compiler error: crashed in nf")).map(
                |d| d.severity
            ),
            Some(DiagSeverity::Error)
        );
        assert_eq!(
            parse_compiler_message(&make("warning")).map(|d| d.severity),
            Some(DiagSeverity::Warning)
        );
        // note / help / failure-note all collapse to Note.
        assert_eq!(
            parse_compiler_message(&make("note")).map(|d| d.severity),
            Some(DiagSeverity::Note)
        );
        assert_eq!(
            parse_compiler_message(&make("help")).map(|d| d.severity),
            Some(DiagSeverity::Note)
        );
        assert_eq!(
            parse_compiler_message(&make("failure-note")).map(|d| d.severity),
            Some(DiagSeverity::Note)
        );
    }

    #[test]
    fn primary_span_wins_over_first() {
        // Use-after-move style layout: a plain labeled span first, the
        // primary one second.
        let line = r#"{"reason":"compiler-message","message":{"level":"error","message":"use of moved value","spans":[{"file_name":"src/first.rs","line_start":1,"column_start":1,"is_primary":false},{"file_name":"src/real.rs","line_start":7,"column_start":4,"is_primary":true}],"code":{"code":"E0382"}}}"#;
        let diag = parse_compiler_message(line).expect("must parse");
        assert_eq!(diag.file.as_deref(), Some("src/real.rs"));
        assert_eq!(diag.line, Some(7));
        assert_eq!(diag.col, Some(4));
        assert_eq!(diag.code.as_deref(), Some("E0382"));
        assert!(diag.rendered.is_none()); // json-diagnostic-short-ish messages keep it optional
    }

    #[test]
    fn message_without_spans_has_no_location() {
        let line = r#"{"reason":"compiler-message","message":{"level":"error","message":"linking with `cc` failed: exit status 1"}}"#;
        let diag = parse_compiler_message(line).expect("must parse");
        assert_eq!(diag.file, None);
        assert_eq!(diag.line, None);
        assert_eq!(diag.col, None);
        assert_eq!(diag.severity, DiagSeverity::Error);
    }

    #[test]
    fn sink_roundtrip() {
        let diag = Diag {
            severity: DiagSeverity::Warning,
            message: "sink check".into(),
            rendered: None,
            file: Some("src/lib.rs".into()),
            line: Some(3),
            col: Some(5),
            code: None,
        };
        crate::diagnostics::push_diags([diag.clone()]);
        let drained = crate::diagnostics::drain_diags();
        assert!(drained.contains(&diag));
        // The same batch must not come back twice; other tests do not push.
        assert!(!crate::diagnostics::drain_diags().contains(&diag));
        let _ = drain_diags();
    }
}
