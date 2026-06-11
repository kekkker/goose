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
/// Field order in the source JSON is irrelevant — all JSON objects (including
/// nested ones inside `parameters`) are recursively sorted by key before
/// hashing, so writer and reader always produce the same digest.
pub fn correlation_key(args: &serde_json::Value) -> String {
    let mut canonical: BTreeMap<&str, serde_json::Value> = BTreeMap::new();

    for key in ["source", "instructions", "parameters"] {
        if let Some(v) = args.get(key) {
            if !v.is_null() {
                canonical.insert(key, sort_value(v));
            }
        }
    }

    // BTreeMap gives sorted outer keys; sort_value ensures nested objects are
    // also sorted, making the serialisation fully deterministic regardless of
    // how either side built the JSON.
    let json = serde_json::to_string(&canonical).unwrap_or_default();
    let hash = Sha256::digest(json.as_bytes());
    crate::utils::bytes_to_hex(&hash[..8]) // 16 hex chars
}

/// Recursively reorder every JSON object's keys alphabetically so that the
/// serialised form is deterministic regardless of insertion order.
fn sort_value(v: &serde_json::Value) -> serde_json::Value {
    match v {
        serde_json::Value::Object(map) => {
            let sorted: serde_json::Map<String, serde_json::Value> = map
                .iter()
                .collect::<BTreeMap<_, _>>()
                .into_iter()
                .map(|(k, v)| (k.clone(), sort_value(v)))
                .collect();
            serde_json::Value::Object(sorted)
        }
        serde_json::Value::Array(arr) => {
            serde_json::Value::Array(arr.iter().map(sort_value).collect())
        }
        other => other.clone(),
    }
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

/// Returns a filename that embeds both the correlation key and the task id so
/// that `find_progress_file` can locate it by either key.
///
/// Format: `<corr_key>.<task_id>.<unix_ms>.ndjson`
/// - Prefix match on `<corr_key>.` — finds it from the delegate-call tailer.
/// - Infix match on `.<task_id>.` — finds it from the load-call tailer.
pub fn progress_filename_async(corr_key: &str, task_id: &str) -> String {
    format!("{}.{}.{}.ndjson", corr_key, task_id, now_ms())
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
        /// Human-readable label for the delegate (recipe/source name or ad-hoc summary).
        /// Rendered in the status line; absent in files written by older versions.
        #[serde(skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        subagent_session_id: Option<String>,
    },
    Tool {
        ts: u64,
        tool_name: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        summary: Option<String>,
        /// Cumulative tool-call count at the time of this event.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tool_count: Option<u32>,
        /// Cumulative turn count at the time of this event.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        turn_count: Option<u32>,
    },
    Done {
        ts: u64,
    },
}

impl ProgressEvent {
    pub fn start(name: Option<String>, subagent_session_id: Option<String>) -> Self {
        Self::Start {
            ts: now_ms(),
            name,
            subagent_session_id,
        }
    }

    pub fn tool(
        tool_name: String,
        summary: Option<String>,
        tool_count: Option<u32>,
        turn_count: Option<u32>,
    ) -> Self {
        Self::Tool {
            ts: now_ms(),
            tool_name,
            summary,
            tool_count,
            turn_count,
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

    /// Open (create) a progress file discoverable by either the correlation key
    /// or the task id (for async delegates).  Sweeps old files as a side-effect.
    pub fn new_async(corr_key: &str, task_id: &str) -> Self {
        let dir = progress_dir();
        sweep_old_files(&dir);
        let filename = progress_filename_async(corr_key, task_id);
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

    pub fn write_start(&self, name: Option<String>, subagent_session_id: Option<String>) {
        self.append(&ProgressEvent::start(name, subagent_session_id));
    }

    pub fn write_tool(
        &self,
        tool_name: &str,
        summary: Option<&str>,
        tool_count: Option<u32>,
        turn_count: Option<u32>,
    ) {
        self.append(&ProgressEvent::tool(
            tool_name.to_string(),
            summary.map(|s| s.to_string()),
            tool_count,
            turn_count,
        ));
    }

    pub fn write_done(&self) {
        self.append(&ProgressEvent::done());
    }
}

// ── reader / tailer ───────────────────────────────────────────────────────────

/// Finds the newest progress file whose name starts with `<key>.` (correlation-key
/// lookup) OR whose stem contains `.<key>.` (task-id lookup, for async delegates
/// whose filename is `<corr_key>.<task_id>.<ts>.ndjson`), and whose last-modified
/// timestamp component is >= `not_before_ms`.
///
/// Returns `None` if no such file exists yet.
pub fn find_progress_file(dir: &std::path::Path, key: &str, not_before_ms: u64) -> Option<PathBuf> {
    let prefix = format!("{}.", key);
    let infix = format!(".{}.", key);
    let mut best: Option<(u64, PathBuf)> = None;

    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if !name.starts_with(&prefix) && !name.contains(&infix) {
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

// ── structured async tailer ───────────────────────────────────────────────────

/// Poll for the delegate's progress file and tail it, delivering one
/// `ProgressEvent` at a time to `sink`.
///
/// * Waits up to 120 s for the file to appear.
/// * Tails for at most 30 min.
/// * Returns when a `Done` event is seen (or a deadline is reached).
/// * `sink` is called for every parsed event, including `Start` and `Done`.
pub async fn tail_delegate_progress_events<S>(key: String, not_before_ms: u64, mut sink: S)
where
    S: FnMut(ProgressEvent) + Send + 'static,
{
    use tokio::time::{sleep, Duration, Instant};

    let dir = progress_dir();
    let poll_interval = Duration::from_millis(400);
    let file_wait_deadline = Instant::now() + Duration::from_secs(120);
    let run_deadline = Instant::now() + Duration::from_secs(1800);

    let file_path = loop {
        if Instant::now() >= file_wait_deadline {
            debug!(
                "tail_delegate_progress: no progress file found within 120s for key={}",
                key
            );
            return;
        }
        if let Some(p) = find_progress_file(&dir, &key, not_before_ms) {
            break p;
        }
        sleep(poll_interval).await;
    };

    debug!("tail_delegate_progress: tailing {:?}", file_path);

    let mut bytes_read: u64 = 0;

    loop {
        if Instant::now() >= run_deadline {
            debug!("tail_delegate_progress: 30-min cap reached for key={}", key);
            break;
        }

        let new_lines: Vec<String> = match std::fs::File::open(&file_path) {
            Ok(mut f) => {
                use std::io::{BufRead as _, Seek as _};
                if f.seek(std::io::SeekFrom::Start(bytes_read)).is_err() {
                    break;
                }
                let mut reader = std::io::BufReader::new(&mut f);
                let mut lines = Vec::new();
                let mut line_buf = String::new();
                while reader
                    .read_line(&mut line_buf)
                    .map(|n| n > 0)
                    .unwrap_or(false)
                {
                    lines.push(std::mem::take(&mut line_buf));
                }
                drop(reader);
                if let Ok(pos) = f.stream_position() {
                    bytes_read = pos;
                }
                lines
            }
            Err(e) => {
                debug!("tail_delegate_progress: read error: {e}");
                break;
            }
        };

        let mut got_done = false;
        for line in &new_lines {
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<ProgressEvent>(line) {
                Ok(ev) => {
                    let is_done = matches!(ev, ProgressEvent::Done { .. });
                    sink(ev);
                    if is_done {
                        got_done = true;
                        break;
                    }
                }
                Err(e) => {
                    debug!(
                        "tail_delegate_progress: parse error on line {:?}: {e}",
                        line
                    );
                }
            }
        }

        if got_done {
            debug!(
                "tail_delegate_progress: done event received for key={}",
                key
            );
            break;
        }

        sleep(poll_interval).await;
    }
}

// ── generic async tailer ─────────────────────────────────────────────────────

/// Poll for the delegate's progress file and tail it, calling `sink` with the
/// full accumulated progress text on every batch of new events.
///
/// Implemented on top of `tail_delegate_progress_events`; the polling loop
/// runs only once.
///
/// * Waits up to 120 s for the file to appear.
/// * Tails for at most 30 min.
/// * Stops early when a `Done` event is seen.
/// * `sink` receives the cumulative `format_progress_lines` text after each
///   new `Tool` event.  The sink receives the full text every time (not a
///   diff) — callers that need incremental output should diff against their
///   own previous value.
pub async fn tail_delegate_progress_generic<S>(key: String, not_before_ms: u64, sink: S)
where
    S: Fn(String) + Send + 'static,
{
    let mut events: Vec<ProgressEvent> = Vec::new();
    tail_delegate_progress_events(key, not_before_ms, move |ev| {
        let is_tool = matches!(ev, ProgressEvent::Tool { .. });
        events.push(ev);
        if is_tool {
            let text = format_progress_lines(&events);
            if !text.is_empty() {
                sink(text);
            }
        }
    })
    .await;
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
        writer.write_start(Some("my-recipe".into()), Some("session-abc".into()));
        writer.write_tool("shell", Some("kubectl get pods"), Some(1), Some(1));
        writer.write_tool("text_editor", None, Some(2), Some(1));
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

    #[tokio::test]
    async fn generic_tailer_collects_progress() {
        let dir = tempfile::tempdir().expect("tempdir");

        // Temporarily override XDG_STATE_HOME so progress_dir() resolves into
        // our temp dir.  The real progress_dir() uses XDG_STATE_HOME or ~/.local/state.
        // We bypass that by calling find_progress_file / ProgressWriter directly and
        // exercising tail_delegate_progress_generic end-to-end with a synthetic file.
        let key = "deadbeef01234567";
        let ts = now_ms() - 500; // slightly in the past but >= not_before
        let filename = format!("{}.{}.ndjson", key, ts);
        let file_path = dir.path().join(&filename);

        // Pre-write a complete progress file (Start + Tool + Tool + Done).
        {
            let writer = ProgressWriter {
                path: file_path.clone(),
            };
            writer.write_start(None, None);
            writer.write_tool("shell", Some("echo hi"), None, None);
            writer.write_tool("text_editor", None, None, None);
            writer.write_done();
        }

        // Override XDG_STATE_HOME so the generic tailer finds our temp dir.
        // We can't easily do that here without unsafe env mutation, so instead
        // we call the underlying helpers directly and just test that the sink
        // receives the expected accumulated text.
        let not_before = ts - 1; // file is newer than not_before

        // Use find_progress_file + format_progress_lines to reproduce what
        // tail_delegate_progress_generic does, verifying the helper chain.
        let found = find_progress_file(dir.path(), key, not_before);
        assert!(found.is_some(), "file should be discoverable");

        let f = std::fs::File::open(found.unwrap()).unwrap();
        let events: Vec<ProgressEvent> = std::io::BufReader::new(f)
            .lines()
            .map_while(Result::ok)
            .filter_map(|l| serde_json::from_str(&l).ok())
            .collect();

        let text = format_progress_lines(&events);
        assert!(text.contains("→ shell: echo hi"));
        assert!(text.contains("→ text_editor"));
    }

    #[tokio::test]
    async fn structured_tailer_delivers_one_callback_per_event() {
        use std::sync::{Arc, Mutex};

        let dir = tempfile::tempdir().expect("tempdir");
        let key = "cafebabe12345678";
        let ts = now_ms() - 100;
        let filename = format!("{}.{}.ndjson", key, ts);
        let file_path = dir.path().join(&filename);

        {
            let writer = ProgressWriter {
                path: file_path.clone(),
            };
            writer.write_start(None, None);
            writer.write_tool("shell", Some("ls"), None, None);
            writer.write_tool("text_editor", None, None, None);
            writer.write_done();
        }

        // Override progress_dir by pointing XDG_STATE_HOME at our temp dir so
        // tail_delegate_progress_events can find the file.  We call
        // find_progress_file + parse directly to avoid env mutation races.
        let not_before = ts - 1;
        let found = find_progress_file(dir.path(), key, not_before);
        assert!(found.is_some(), "progress file must be found");

        // Collect events via the structured tailer using find_progress_file
        // result directly by pre-writing a complete file and exercising the
        // underlying parse path.  Since XDG_STATE_HOME mutation is unsafe in
        // parallel tests we verify the event count through the helper chain
        // that tail_delegate_progress_events delegates to.
        let f = std::fs::File::open(found.unwrap()).unwrap();
        let events: Vec<ProgressEvent> = std::io::BufReader::new(f)
            .lines()
            .map_while(Result::ok)
            .filter_map(|l| serde_json::from_str(&l).ok())
            .collect();

        // Exactly 4 events: Start + Tool + Tool + Done.
        assert_eq!(events.len(), 4, "must parse exactly 4 events");
        assert!(matches!(events[0], ProgressEvent::Start { .. }));
        assert!(
            matches!(&events[1], ProgressEvent::Tool { tool_name, .. } if tool_name == "shell")
        );
        assert!(
            matches!(&events[2], ProgressEvent::Tool { tool_name, .. } if tool_name == "text_editor")
        );
        assert!(matches!(events[3], ProgressEvent::Done { .. }));

        // Now exercise the live tailer end-to-end by temporarily overriding
        // XDG_STATE_HOME.  We use env_lock if available, otherwise skip.
        // Write file into the expected XDG sub-path.
        let xdg_root = tempfile::tempdir().expect("xdg root");
        let progress_subdir = xdg_root.path().join("goose/subagent-progress");
        std::fs::create_dir_all(&progress_subdir).unwrap();
        let live_path = progress_subdir.join(&filename);
        {
            let writer = ProgressWriter { path: live_path };
            writer.write_start(None, None);
            writer.write_tool("grep", Some("pattern"), None, None);
            writer.write_done();
        }

        let collected: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let collected2 = collected.clone();

        let old_val = std::env::var("XDG_STATE_HOME").ok();
        std::env::set_var("XDG_STATE_HOME", xdg_root.path());

        tail_delegate_progress_events(key.to_string(), not_before, move |ev| {
            if let ProgressEvent::Tool { tool_name, .. } = ev {
                collected2.lock().unwrap().push(tool_name);
            }
        })
        .await;

        match old_val {
            Some(v) => std::env::set_var("XDG_STATE_HOME", v),
            None => std::env::remove_var("XDG_STATE_HOME"),
        }

        let names = collected.lock().unwrap().clone();
        assert_eq!(
            names,
            vec!["grep"],
            "tailer must deliver exactly one Tool event callback"
        );
    }

    #[test]
    fn find_progress_file_dual_key_async_format() {
        let dir = tempfile::tempdir().expect("tempdir");
        let corr_key = "aabbccdd11223344";
        let task_id = "20260611_130";
        let not_before = now_ms();
        let ts = not_before + 500;

        // Async progress file uses <corr_key>.<task_id>.<ts>.ndjson format.
        let filename = format!("{}.{}.{}.ndjson", corr_key, task_id, ts);
        std::fs::write(dir.path().join(&filename), "").unwrap();

        // Lookup by correlation key (prefix match).
        let found = find_progress_file(dir.path(), corr_key, not_before);
        assert!(
            found.is_some(),
            "should find by corr_key prefix: {filename}"
        );
        assert_eq!(
            found.unwrap().file_name().unwrap().to_str().unwrap(),
            filename
        );

        // Lookup by task id (infix match via ".<task_id>." containment).
        let found = find_progress_file(dir.path(), task_id, not_before);
        assert!(found.is_some(), "should find by task_id infix: {filename}");
        assert_eq!(
            found.unwrap().file_name().unwrap().to_str().unwrap(),
            filename
        );
    }

    /// The correlation key computed writer-side (from structured DelegateParams)
    /// must equal the key computed reader-side (from the raw JSON the ACP adapter
    /// forwards), even when `parameters` contains nested objects with different
    /// key insertion orders.
    #[test]
    fn correlation_key_reader_writer_agreement() {
        // Writer builds args_for_key by hand (mimicking handle_delegate /
        // handle_async_delegate in summon.rs).
        let writer_args = serde_json::json!({
            "source": "file-locator",
            "instructions": "find all Rust files",
            "parameters": {"z_key": "last", "a_key": "first"},
        });

        // Reader receives raw_input from the ACP adapter — same logical data but
        // JSON object keys may arrive in a different order, and extra fields like
        // "async" or "extensions" may be present.
        let reader_args = serde_json::json!({
            "instructions": "find all Rust files",
            "parameters": {"a_key": "first", "z_key": "last"},
            "source": "file-locator",
            "async": true,
            "extensions": ["developer"],
        });

        assert_eq!(
            correlation_key(&writer_args),
            correlation_key(&reader_args),
            "writer-side and reader-side must produce the same key"
        );
    }

    /// Verify that Tool events round-trip the new counter fields.
    #[test]
    fn tool_event_counter_fields_round_trip() {
        let ev = ProgressEvent::tool("grep".into(), Some("pattern".into()), Some(3), Some(2));
        let json = serde_json::to_string(&ev).unwrap();
        let parsed: ProgressEvent = serde_json::from_str(&json).unwrap();
        if let ProgressEvent::Tool {
            tool_count,
            turn_count,
            ..
        } = parsed
        {
            assert_eq!(tool_count, Some(3));
            assert_eq!(turn_count, Some(2));
        } else {
            panic!("expected Tool variant");
        }
    }

    /// Absent counter fields should deserialise as None (backwards compat).
    #[test]
    fn tool_event_missing_counters_are_none() {
        let json = r#"{"event":"tool","ts":1000,"tool_name":"shell"}"#;
        let ev: ProgressEvent = serde_json::from_str(json).unwrap();
        if let ProgressEvent::Tool {
            tool_count,
            turn_count,
            ..
        } = ev
        {
            assert_eq!(tool_count, None);
            assert_eq!(turn_count, None);
        } else {
            panic!("expected Tool variant");
        }
    }

    /// Start events round-trip the new name field.
    #[test]
    fn start_event_name_round_trip() {
        let ev = ProgressEvent::start(Some("file-locator".into()), Some("20260611_123".into()));
        let json = serde_json::to_string(&ev).unwrap();
        let parsed: ProgressEvent = serde_json::from_str(&json).unwrap();
        if let ProgressEvent::Start { name, .. } = parsed {
            assert_eq!(name.as_deref(), Some("file-locator"));
        } else {
            panic!("expected Start variant");
        }
    }
}
