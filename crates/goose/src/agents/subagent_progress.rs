/// Out-of-band progress rendezvous for delegated subagents.
///
/// When the `delegate` tool is called from Claude Code (via the ACP path), the
/// two participating processes — `goose mcp summon` (writer) and the ACP server
/// inside the main `goose` TUI process (reader) — communicate through a small
/// ndjson progress file written to the user's XDG state directory.
///
/// # File lifecycle
/// * The writer creates `<progress_dir>/<correlation_key>.<unix_ms>.ndjson` and
///   appends one JSON line per event.
/// * The reader polls the directory for a matching file, tails it, and sends
///   accumulated progress text to the TUI as non-terminal `ToolCallUpdate`s.
/// * On delegate completion the writer appends a `{"event":"done"}` line.  The
///   reader stops tailing as soon as it sees that line (or after 30 min).
/// * Both sides sweep files older than 1 h when they start a new delegate.
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use tracing::debug;

// ── paths ────────────────────────────────────────────────────────────────────

/// Returns (and creates) `$XDG_STATE_HOME/goose/subagent-progress`.
pub fn progress_dir() -> PathBuf {
    let base = std::env::var("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            dirs::home_dir()
                .unwrap_or_else(|| PathBuf::from("/tmp"))
                .join(".local/state")
        });
    let dir = base.join("goose/subagent-progress");
    let _ = std::fs::create_dir_all(&dir);
    dir
}

// ── correlation key ───────────────────────────────────────────────────────────

/// A stable 16-hex-char identifier derived from the delegate args subset
/// `{source, instructions, parameters}`.
///
/// Field order in the source JSON is irrelevant — we normalise via BTreeMap
/// and skip null/absent fields before hashing.
pub fn correlation_key(args: &serde_json::Value) -> String {
    let mut canonical: BTreeMap<&str, &serde_json::Value> = BTreeMap::new();

    for key in ["source", "instructions", "parameters"] {
        if let Some(v) = args.get(key) {
            if !v.is_null() {
                canonical.insert(key, v);
            }
        }
    }

    // Serialise with sorted keys (BTreeMap gives us that for free).
    let json = serde_json::to_string(&canonical).unwrap_or_default();
    let hash = Sha256::digest(json.as_bytes());
    crate::utils::bytes_to_hex(&hash[..8]) // 16 hex chars
}

// ── ndjson helpers ────────────────────────────────────────────────────────────

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Returns the filename stem `<key>.<unix_ms>` for a new progress file.
pub fn progress_filename(key: &str) -> String {
    format!("{}.{}.ndjson", key, now_ms())
}

// ── cleanup ───────────────────────────────────────────────────────────────────

/// Remove progress files older than 1 h.  Errors are silently ignored.
pub fn sweep_old_files(dir: &std::path::Path) {
    let cutoff = now_ms().saturating_sub(3600 * 1000);
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            // Extract the timestamp from the filename `<key>.<ts>.ndjson`.
            if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                if let Some(ts_str) = stem.rsplit('.').next() {
                    if let Ok(ts) = ts_str.parse::<u64>() {
                        if ts < cutoff {
                            let _ = std::fs::remove_file(&path);
                        }
                    }
                }
            }
        }
    }
}

// ── event types ───────────────────────────────────────────────────────────────

#[derive(Debug, serde::Serialize, serde::Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum ProgressEvent {
    Start {
        ts: u64,
        #[serde(skip_serializing_if = "Option::is_none")]
        subagent_session_id: Option<String>,
    },
    Tool {
        ts: u64,
        tool_name: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        summary: Option<String>,
    },
    Done {
        ts: u64,
    },
}

impl ProgressEvent {
    pub fn start(subagent_session_id: Option<String>) -> Self {
        Self::Start {
            ts: now_ms(),
            subagent_session_id,
        }
    }

    pub fn tool(tool_name: String, summary: Option<String>) -> Self {
        Self::Tool {
            ts: now_ms(),
            tool_name,
            summary,
        }
    }

    pub fn done() -> Self {
        Self::Done { ts: now_ms() }
    }
}

// ── writer ────────────────────────────────────────────────────────────────────

/// Appends ndjson progress events to a per-delegate file.
///
/// All writes are best-effort; IO errors are logged at `debug` level and
/// never propagate to the caller.
pub struct ProgressWriter {
    path: PathBuf,
}

impl ProgressWriter {
    /// Open (create) a new progress file for the given correlation key.
    /// Sweeps old files as a side-effect.
    pub fn new(key: &str) -> Self {
        let dir = progress_dir();
        sweep_old_files(&dir);
        let filename = progress_filename(key);
        let path = dir.join(filename);
        Self { path }
    }

    fn append(&self, event: &ProgressEvent) {
        match serde_json::to_string(event) {
            Ok(line) => {
                use std::io::Write;
                match std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&self.path)
                {
                    Ok(mut f) => {
                        let _ = writeln!(f, "{}", line);
                    }
                    Err(e) => debug!("subagent_progress: write error: {e}"),
                }
            }
            Err(e) => debug!("subagent_progress: serialise error: {e}"),
        }
    }

    pub fn write_start(&self, subagent_session_id: Option<String>) {
        self.append(&ProgressEvent::start(subagent_session_id));
    }

    pub fn write_tool(&self, tool_name: &str, summary: Option<&str>) {
        self.append(&ProgressEvent::tool(
            tool_name.to_string(),
            summary.map(|s| s.to_string()),
        ));
    }

    pub fn write_done(&self) {
        self.append(&ProgressEvent::done());
    }
}

// ── reader / tailer ───────────────────────────────────────────────────────────

/// Finds the newest progress file whose name starts with `<key>.` and whose
/// last-modified time is >= `not_before_ms`.
///
/// Returns `None` if no such file exists yet.
pub fn find_progress_file(dir: &std::path::Path, key: &str, not_before_ms: u64) -> Option<PathBuf> {
    let prefix = format!("{}.", key);
    let mut best: Option<(u64, PathBuf)> = None;

    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if !name.starts_with(&prefix) {
                continue;
            }
            // Extract timestamp from filename.
            if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                if let Some(ts_str) = stem.rsplit('.').next() {
                    if let Ok(ts) = ts_str.parse::<u64>() {
                        if ts >= not_before_ms
                            && best.as_ref().map(|(bt, _)| ts > *bt).unwrap_or(true)
                        {
                            best = Some((ts, path));
                        }
                    }
                }
            }
        }
    }

    best.map(|(_, p)| p)
}

/// Converts accumulated progress events into human-readable lines.
///
/// Each `Tool` event becomes `"→ <name>: <summary>"` (or `"→ <name>"` when
/// there is no summary).
pub fn format_progress_lines(events: &[ProgressEvent]) -> String {
    let mut lines = Vec::new();
    for ev in events {
        if let ProgressEvent::Tool {
            tool_name, summary, ..
        } = ev
        {
            let line = match summary {
                Some(s) if !s.is_empty() => format!("→ {}: {}", tool_name, s),
                _ => format!("→ {}", tool_name),
            };
            lines.push(line);
        }
    }
    lines.join("\n")
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::BufRead;

    #[test]
    fn correlation_key_is_stable() {
        let args = serde_json::json!({
            "instructions": "do the thing",
            "source": "my-recipe",
            "parameters": {"env": "prod"},
        });
        let key1 = correlation_key(&args);
        // Same content, different insertion order.
        let args2 = serde_json::json!({
            "parameters": {"env": "prod"},
            "instructions": "do the thing",
            "source": "my-recipe",
        });
        let key2 = correlation_key(&args2);
        assert_eq!(key1, key2, "key must be order-insensitive");
        assert_eq!(key1.len(), 16, "key must be 16 hex chars");
    }

    #[test]
    fn correlation_key_ignores_extra_fields() {
        let base = serde_json::json!({ "instructions": "hello" });
        let extra = serde_json::json!({
            "instructions": "hello",
            "extensions": ["developer"],
            "provider": "anthropic",
            "async": false,
        });
        assert_eq!(
            correlation_key(&base),
            correlation_key(&extra),
            "only source/instructions/parameters affect the key"
        );
    }

    #[test]
    fn correlation_key_with_parameters_present_and_absent() {
        let with_params = serde_json::json!({
            "instructions": "task",
            "parameters": {"k": "v"},
        });
        let without_params = serde_json::json!({ "instructions": "task" });
        // They must differ.
        assert_ne!(
            correlation_key(&with_params),
            correlation_key(&without_params)
        );
    }

    #[test]
    fn correlation_key_skips_null_parameters() {
        let explicit_null = serde_json::json!({
            "instructions": "task",
            "parameters": null,
        });
        let absent = serde_json::json!({ "instructions": "task" });
        // null is treated as absent.
        assert_eq!(correlation_key(&explicit_null), correlation_key(&absent));
    }

    #[test]
    fn writer_reader_round_trip() {
        let dir = tempfile::tempdir().expect("tempdir");
        let key = "testkey1234567890abcd";

        // Manually create the file as the writer would.
        let ts = now_ms();
        let filename = format!("{}.{}.ndjson", key, ts);
        let file_path = dir.path().join(&filename);

        let writer = ProgressWriter {
            path: file_path.clone(),
        };
        writer.write_start(Some("session-abc".into()));
        writer.write_tool("shell", Some("kubectl get pods"));
        writer.write_tool("text_editor", None);
        writer.write_done();

        // Read and parse all events.
        let f = std::fs::File::open(&file_path).unwrap();
        let events: Vec<ProgressEvent> = std::io::BufReader::new(f)
            .lines()
            .map_while(Result::ok)
            .filter_map(|l| serde_json::from_str(&l).ok())
            .collect();

        assert_eq!(events.len(), 4);
        assert!(matches!(events[0], ProgressEvent::Start { .. }));
        assert!(matches!(
            &events[1],
            ProgressEvent::Tool { tool_name, .. } if tool_name == "shell"
        ));
        assert!(matches!(events[3], ProgressEvent::Done { .. }));

        // Verify formatting.
        let text = format_progress_lines(&events);
        assert!(text.contains("→ shell: kubectl get pods"));
        assert!(text.contains("→ text_editor"));
    }

    #[test]
    fn find_progress_file_picks_newest() {
        let dir = tempfile::tempdir().expect("tempdir");
        let key = "aabbccdd11223344";
        let not_before = now_ms();

        // Write two files with different timestamps — the newer one should win.
        std::fs::write(dir.path().join(format!("{}.100.ndjson", key)), "").unwrap();
        std::fs::write(dir.path().join(format!("{}.200.ndjson", key)), "").unwrap();

        // Both have timestamps < not_before, so nothing found.
        let result = find_progress_file(dir.path(), key, not_before);
        assert!(result.is_none());

        // Write a file with a future-ish timestamp.
        let ts = not_before + 1000;
        let name = format!("{}.{}.ndjson", key, ts);
        std::fs::write(dir.path().join(&name), "").unwrap();

        let result = find_progress_file(dir.path(), key, not_before);
        assert!(result.is_some());
        assert_eq!(result.unwrap().file_name().unwrap().to_str().unwrap(), name);
    }
}
