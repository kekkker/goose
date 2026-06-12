/// In-place status block for concurrent delegate subagent progress.
///
/// While one or more delegate tailers are active a block of lines at the bottom
/// of terminal output shows every active delegate's spinner, name, current tool,
/// event count and elapsed seconds — redrawn in place on every update.  When a
/// delegate finishes a permanent dim summary line is printed and the delegate is
/// removed from the block.
///
/// Rendering:
/// - Each active delegate gets its own line (one line per entry, up to 8; a
///   "… and N more" footer is appended when there are more than 7 active).
/// - Redraws use `console::Term::clear_last_lines(n)` to erase exactly the
///   previously drawn number of lines before reprinting.
/// - ClearNow / PrintLine both clear the full previously drawn block so normal
///   output is never corrupted.
///
/// Non-TTY fallback: all clear/rewrite tricks are skipped; delegate events are
/// printed as plain appended lines instead.
///
/// All stdout writes (both the status block and any "normal" lines that need to
/// appear above it) must go through the renderer so they don't interleave.
use std::collections::{HashMap, HashSet};
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tokio::sync::mpsc;

// ── spinner ───────────────────────────────────────────────────────────────────

const SPINNER_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// Maximum number of live delegate lines shown at once (7 entries + 1 "…" footer).
const MAX_VISIBLE: usize = 7;

// ── per-delegate state ────────────────────────────────────────────────────────

#[derive(Clone, Debug)]
pub struct DelegateState {
    pub name: String,
    pub current_tool: Option<String>,
    pub tool_count: usize,
    pub turn_count: Option<u32>,
    pub started_at: Instant,
}

// ── pure composer (testable without I/O) ─────────────────────────────────────

/// Build one status line for a single delegate entry.
///
/// `spinner_idx` indexes into `SPINNER_FRAMES` (wrapping).
/// `max_width` is the terminal column count; the result is truncated to fit.
/// The returned string is guaranteed single-line (no `\n` / `\r`).
pub fn compose_entry_line(state: &DelegateState, spinner_idx: usize, max_width: usize) -> String {
    let frame = SPINNER_FRAMES[spinner_idx % SPINNER_FRAMES.len()];
    let elapsed = state.started_at.elapsed().as_secs();
    let tool_part = state
        .current_tool
        .as_deref()
        .map(|t| format!(" \"{}\"", sanitize_one_line(t, 60)))
        .unwrap_or_default();
    let tool_word = if state.tool_count == 1 {
        "tool"
    } else {
        "tools"
    };
    let mut stats = format!("{} {}", state.tool_count, tool_word);
    if let Some(turns) = state.turn_count {
        let turn_word = if turns == 1 { "turn" } else { "turns" };
        stats.push_str(&format!(" · {} {}", turns, turn_word));
    }
    stats.push_str(&format!(" · {}s", elapsed));

    let line = format!("{} {}:{} · {}", frame, state.name, tool_part, stats);

    truncate_to_width(line, max_width)
}

/// Build the full status block as a `Vec<String>`, one entry per line.
///
/// At most `MAX_VISIBLE` + 1 lines are returned (7 delegate lines + optional
/// "… and N more" line).  Order follows `order` (insertion order).
pub fn compose_status_block(
    states: &[&DelegateState],
    spinner_idx: usize,
    max_width: usize,
) -> Vec<String> {
    if states.is_empty() {
        return Vec::new();
    }
    let total = states.len();
    let visible = states.iter().take(MAX_VISIBLE);
    let mut lines: Vec<String> = visible
        .map(|s| compose_entry_line(s, spinner_idx, max_width))
        .collect();
    if total > MAX_VISIBLE {
        let overflow = total - MAX_VISIBLE;
        lines.push(format!("  … and {} more", overflow));
    }
    lines
}

/// Kept for API compatibility with callers that expect a single joined line.
/// Joins block lines with " | " — only used in tests that still test the old
/// single-line shape; new callers should use `compose_status_block`.
#[cfg(test)]
fn compose_status_line(states: &[&DelegateState], spinner_idx: usize, max_width: usize) -> String {
    compose_status_block(states, spinner_idx, max_width).join(" | ")
}

// ── sanitization helpers ──────────────────────────────────────────────────────

/// Normalize a tool summary string so it is safe for single-line rendering:
/// 1. Replace all ASCII control characters (including `\n`, `\r`, `\t`) with a
///    space.
/// 2. Collapse runs of whitespace to a single space.
/// 3. Trim leading/trailing whitespace.
/// 4. Cap length to `max_chars` (truncating with `…` if needed).
///
/// This is called by `ProgressWriter::write_tool` so every consumer receives
/// sanitized text in the progress file.
pub fn sanitize_summary(s: &str, max_chars: usize) -> String {
    let normalized: String = s
        .chars()
        .map(|c| {
            if c.is_ascii_control() || c == '\r' || c == '\n' {
                ' '
            } else {
                c
            }
        })
        .collect();
    // Collapse whitespace runs.
    let collapsed = normalized
        .split_ascii_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    // Cap length (char-count safe).
    if collapsed.chars().count() <= max_chars {
        collapsed
    } else {
        let truncated: String = collapsed
            .chars()
            .take(max_chars.saturating_sub(1))
            .collect();
        format!("{}…", truncated)
    }
}

/// Strip newlines / control chars from `s`, collapse whitespace, and truncate
/// to `max_chars`.  Used defensively inside the renderer so even unsanitized
/// data from older progress file writers cannot corrupt the in-place block.
fn sanitize_one_line(s: &str, max_chars: usize) -> String {
    sanitize_summary(s, max_chars)
}

/// Truncate `line` so its visible width fits in `max_width` columns.
fn truncate_to_width(line: String, max_width: usize) -> String {
    if max_width == 0 {
        return line;
    }
    if console::measure_text_width(&line) <= max_width {
        return line;
    }
    let ellipsis = "…";
    let mut chars: Vec<char> = line.chars().collect();
    while !chars.is_empty()
        && console::measure_text_width(&(chars.iter().collect::<String>() + ellipsis)) > max_width
    {
        chars.pop();
    }
    chars.iter().collect::<String>() + ellipsis
}

// ── process-wide file registry (fix: deduplicate tailers on same file) ────────

/// Tracks which progress files are already being tailed in this process.
///
/// A second tailer that resolves the same path exits immediately so only one
/// renderer entry tracks each real file.
static TAILED_FILES: std::sync::OnceLock<Arc<Mutex<HashSet<PathBuf>>>> = std::sync::OnceLock::new();

fn tailed_files() -> Arc<Mutex<HashSet<PathBuf>>> {
    Arc::clone(TAILED_FILES.get_or_init(|| Arc::new(Mutex::new(HashSet::new()))))
}

/// Attempt to claim `path` as the file being tailed for this entry.
///
/// Returns `true` if the claim succeeded (this tailer should proceed), or
/// `false` if another tailer already owns the file (this tailer should abort
/// silently without printing a done-summary).
pub fn claim_progress_file(path: &Path) -> bool {
    tailed_files()
        .lock()
        .map(|mut set| set.insert(path.to_path_buf()))
        .unwrap_or(false)
}

/// Release the claim on `path` when the tailer finishes.
pub fn release_progress_file(path: &Path) {
    if let Ok(mut set) = tailed_files().lock() {
        set.remove(path);
    }
}

// ── renderer command ──────────────────────────────────────────────────────────

pub enum RendererCmd {
    /// A delegate tailer started; registers the delegate with the given key.
    Register { key: String, name: String },
    /// A Tool event arrived for the keyed delegate.
    ToolEvent {
        key: String,
        tool_name: String,
        tool_count: Option<u32>,
        turn_count: Option<u32>,
    },
    /// The keyed delegate should adopt `name` from the Start event when its
    /// current label looks like a bare session id.
    AdoptName { key: String, name: String },
    /// The keyed delegate finished; `silent` suppresses the done-summary line
    /// (used when a duplicate tailer detected it was racing the primary one).
    Done { key: String, silent: bool },
    /// Print a normal line above the status block (clear block, print, redraw).
    PrintLine { text: String },
    /// Clear the status block immediately (no redraw).  Used by `clear_now()` so
    /// normal output can safely `println!` without corrupting the status block.
    ClearNow {
        ack: std::sync::Arc<std::sync::atomic::AtomicBool>,
    },
}

// ── process-wide renderer for clearing the status block before normal output ──

/// A process-wide `DelegateStatusRenderer` handle installed by
/// `install_tool_progress_callback` at the start of each turn.  `output.rs`
/// can call `clear_status_line()` to ensure the in-place status block is erased
/// before any normal `println!` output lands on stdout.
///
/// The handle is reset to `None` between turns (when the renderer's sender is
/// dropped) so there is no cross-turn leakage.
static GLOBAL_RENDERER: std::sync::OnceLock<Arc<Mutex<Option<DelegateStatusRenderer>>>> =
    std::sync::OnceLock::new();

fn global_renderer_cell() -> &'static Arc<Mutex<Option<DelegateStatusRenderer>>> {
    GLOBAL_RENDERER.get_or_init(|| Arc::new(Mutex::new(None)))
}

/// Install `renderer` as the process-wide handle.  Called once per turn.
pub fn set_global_renderer(renderer: DelegateStatusRenderer) {
    if let Ok(mut guard) = global_renderer_cell().lock() {
        *guard = Some(renderer);
    }
}

/// Remove the process-wide handle (called when the turn's renderer is dropped).
pub fn clear_global_renderer() {
    if let Ok(mut guard) = global_renderer_cell().lock() {
        *guard = None;
    }
}

/// Clear the in-place status block immediately, if one is active.
///
/// Safe to call from any thread at any time; no-ops when there is no active
/// renderer or when stdout is not a TTY.  Printing helpers in `output.rs` call
/// this before every block of normal output so the status block is never
/// partially overwritten.
pub fn clear_status_line() {
    if let Ok(guard) = global_renderer_cell().lock() {
        if let Some(ref r) = *guard {
            r.clear_now();
        }
    }
}

// ── renderer handle ───────────────────────────────────────────────────────────

/// Cheap clone-able handle that routes commands to the background render task.
#[derive(Clone)]
pub struct DelegateStatusRenderer {
    tx: mpsc::UnboundedSender<RendererCmd>,
    is_tty: bool,
}

impl DelegateStatusRenderer {
    /// Spawn the background render task and return a handle to it.
    ///
    /// The task exits when all senders are dropped (channel closes).
    pub fn spawn() -> Self {
        let is_tty = std::io::stdout().is_terminal();
        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(run_render_task(rx, is_tty));
        Self { tx, is_tty }
    }

    pub fn register(&self, key: String, name: String) {
        let _ = self.tx.send(RendererCmd::Register { key, name });
    }

    pub fn tool_event(
        &self,
        key: String,
        tool_name: String,
        tool_count: Option<u32>,
        turn_count: Option<u32>,
    ) {
        let _ = self.tx.send(RendererCmd::ToolEvent {
            key,
            tool_name,
            tool_count,
            turn_count,
        });
    }

    /// Suggest a better human name for an entry whose label is a bare session id.
    pub fn adopt_name(&self, key: String, name: String) {
        let _ = self.tx.send(RendererCmd::AdoptName { key, name });
    }

    pub fn delegate_done(&self, key: String) {
        let _ = self.tx.send(RendererCmd::Done { key, silent: false });
    }

    /// Mark the entry done without printing a summary — used when a duplicate
    /// tailer detected it was racing the primary one for the same file.
    pub fn delegate_done_silent(&self, key: String) {
        let _ = self.tx.send(RendererCmd::Done { key, silent: true });
    }

    /// Route a "normal" println through the renderer so it appears cleanly
    /// above the status block.
    pub fn print_line(&self, text: String) {
        if self.is_tty {
            let _ = self.tx.send(RendererCmd::PrintLine { text });
        } else {
            println!("{}", text);
        }
    }

    /// Synchronously clear the status block so the caller can safely `println!`
    /// without corrupting the in-place status block.
    ///
    /// Sends `ClearNow` and spin-waits (up to ~10 ms) for the render task to
    /// acknowledge.  On non-TTY or when there is nothing drawn this returns
    /// immediately.
    pub fn clear_now(&self) {
        if !self.is_tty {
            return;
        }
        let ack = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let _ = self.tx.send(RendererCmd::ClearNow { ack: ack.clone() });
        // Spin briefly so the render task can drain and process our command.
        let start = std::time::Instant::now();
        while !ack.load(std::sync::atomic::Ordering::Acquire) {
            if start.elapsed().as_millis() > 10 {
                break;
            }
            std::hint::spin_loop();
        }
    }
}

// ── background render task ────────────────────────────────────────────────────

async fn run_render_task(mut rx: mpsc::UnboundedReceiver<RendererCmd>, is_tty: bool) {
    // Insertion-ordered map: key -> state.
    let mut delegates: HashMap<String, DelegateState> = HashMap::new();
    // Track insertion order for deterministic display.
    let mut order: Vec<String> = Vec::new();
    let mut spinner_idx: usize = 0;
    // Number of lines currently drawn in the status block (0 = nothing drawn).
    let mut lines_drawn: usize = 0;
    let term = console::Term::stdout();

    // Tick interval: update elapsed counter / advance spinner at ~10 Hz.
    let mut tick = tokio::time::interval(tokio::time::Duration::from_millis(100));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            biased;

            cmd = rx.recv() => {
                match cmd {
                    None => {
                        // All senders dropped; clean up and exit.
                        if is_tty && lines_drawn > 0 {
                            clear_block(&term, lines_drawn);
                        }
                        break;
                    }
                    Some(RendererCmd::Register { key, name }) => {
                        if !delegates.contains_key(&key) {
                            delegates.insert(key.clone(), DelegateState {
                                name,
                                current_tool: None,
                                tool_count: 0,
                                turn_count: None,
                                started_at: Instant::now(),
                            });
                            order.push(key);
                        }
                        if is_tty {
                            redraw_block(&delegates, &order, &term, &mut spinner_idx, &mut lines_drawn);
                        }
                    }
                    Some(RendererCmd::ToolEvent { key, tool_name, tool_count, turn_count }) => {
                        if let Some(state) = delegates.get_mut(&key) {
                            if !tool_name.is_empty() {
                                state.current_tool = Some(tool_name);
                            }
                            state.tool_count = tool_count
                                .map(|c| c as usize)
                                .unwrap_or_else(|| state.tool_count + 1);
                            if turn_count.is_some() {
                                state.turn_count = turn_count;
                            }
                        }
                        if is_tty {
                            redraw_block(&delegates, &order, &term, &mut spinner_idx, &mut lines_drawn);
                        }
                    }
                    Some(RendererCmd::AdoptName { key, name }) => {
                        if let Some(state) = delegates.get_mut(&key) {
                            if goose::agents::platform_extensions::summon::is_session_id(&state.name) {
                                state.name = name;
                            }
                        }
                        if is_tty {
                            redraw_block(&delegates, &order, &term, &mut spinner_idx, &mut lines_drawn);
                        }
                    }
                    Some(RendererCmd::Done { key, silent }) => {
                        if let Some(state) = delegates.remove(&key) {
                            order.retain(|k| k != &key);
                            if is_tty && lines_drawn > 0 {
                                clear_block(&term, lines_drawn);
                                lines_drawn = 0;
                            }
                            if !silent {
                                let elapsed = state.started_at.elapsed().as_secs();
                                let mut summary_parts = format!("{} tools", state.tool_count);
                                if let Some(turns) = state.turn_count {
                                    summary_parts.push_str(&format!(" · {} turns", turns));
                                }
                                summary_parts.push_str(&format!(" · {}s", elapsed));
                                let summary = format!(
                                    "    {} {} done — {}",
                                    console::style("▸").cyan().dim(),
                                    console::style(&state.name).dim(),
                                    console::style(summary_parts).dim(),
                                );
                                println!("{}", summary);
                                let _ = std::io::stdout().flush();
                            }
                        }
                        if is_tty && !delegates.is_empty() {
                            redraw_block(&delegates, &order, &term, &mut spinner_idx, &mut lines_drawn);
                        }
                    }
                    Some(RendererCmd::PrintLine { text }) => {
                        if is_tty && lines_drawn > 0 {
                            clear_block(&term, lines_drawn);
                            lines_drawn = 0;
                        }
                        println!("{}", text);
                        let _ = std::io::stdout().flush();
                        if is_tty && !delegates.is_empty() {
                            redraw_block(&delegates, &order, &term, &mut spinner_idx, &mut lines_drawn);
                        }
                    }
                    Some(RendererCmd::ClearNow { ack }) => {
                        if is_tty && lines_drawn > 0 {
                            clear_block(&term, lines_drawn);
                            lines_drawn = 0;
                        }
                        ack.store(true, std::sync::atomic::Ordering::Release);
                    }
                }
            }

            _ = tick.tick() => {
                if is_tty && !delegates.is_empty() {
                    spinner_idx = spinner_idx.wrapping_add(1);
                    redraw_block(&delegates, &order, &term, &mut spinner_idx, &mut lines_drawn);
                }
            }
        }
    }
}

/// Clear exactly `n` previously drawn block lines using `console::Term`.
fn clear_block(term: &console::Term, n: usize) {
    if n == 0 {
        return;
    }
    // clear_last_lines(n) moves cursor up n, clears each line, then returns
    // the cursor to the top of the cleared region.
    let _ = term.clear_last_lines(n);
    let _ = std::io::stdout().flush();
}

/// Erase the old block (if any), then print the new block lines.
fn redraw_block(
    delegates: &HashMap<String, DelegateState>,
    order: &[String],
    term: &console::Term,
    spinner_idx: &mut usize,
    lines_drawn: &mut usize,
) {
    let width = term.size_checked().map(|(_, w)| w as usize).unwrap_or(80);
    let states: Vec<&DelegateState> = order.iter().filter_map(|k| delegates.get(k)).collect();

    if states.is_empty() {
        if *lines_drawn > 0 {
            clear_block(term, *lines_drawn);
            *lines_drawn = 0;
        }
        return;
    }

    // Clear the previously drawn block before printing the new one.
    if *lines_drawn > 0 {
        clear_block(term, *lines_drawn);
    }

    let block = compose_status_block(&states, *spinner_idx, width);
    for line in &block {
        // Pad each line to terminal width so stale characters from a previous
        // wider line are overwritten.
        let line_w = console::measure_text_width(line);
        let padding = if width > line_w {
            " ".repeat(width - line_w)
        } else {
            String::new()
        };
        println!("{}{}", line, padding);
    }
    let _ = std::io::stdout().flush();
    *lines_drawn = block.len();
}

// ── unit tests (pure, no I/O) ─────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn make_state(
        name: &str,
        tool: Option<&str>,
        tool_count: usize,
        turn_count: Option<u32>,
        age_secs: u64,
    ) -> DelegateState {
        DelegateState {
            name: name.to_string(),
            current_tool: tool.map(str::to_string),
            tool_count,
            turn_count,
            started_at: Instant::now() - Duration::from_secs(age_secs),
        }
    }

    // ── compose_entry_line ────────────────────────────────────────────────────

    #[test]
    fn single_entry_line_contains_expected_parts() {
        let s = make_state("file-locator", Some("Grep"), 12, Some(3), 34);
        let line = compose_entry_line(&s, 0, 200);
        assert!(line.contains("file-locator"), "name must appear");
        assert!(line.contains("Grep"), "tool must appear");
        assert!(line.contains("12 tools"), "tool count must appear");
        assert!(line.contains("3 turns"), "turn count must appear");
        assert!(line.contains("34s"), "elapsed must appear");
        assert!(
            SPINNER_FRAMES.iter().any(|f| line.contains(f)),
            "spinner must appear"
        );
    }

    #[test]
    fn entry_line_no_tool_no_tool_label() {
        let s = make_state("investigator", None, 0, None, 1);
        let line = compose_entry_line(&s, 0, 200);
        assert!(line.contains("investigator"));
        assert!(!line.contains(": None"));
    }

    #[test]
    fn entry_line_omits_turn_count_when_none() {
        let s = make_state("agent", Some("shell"), 5, None, 10);
        let line = compose_entry_line(&s, 0, 200);
        assert!(line.contains("5 tools"));
        assert!(!line.contains("turns"));
    }

    #[test]
    fn entry_line_truncates_to_max_width() {
        let s = make_state(
            "very-long-delegate-name-that-goes-on-and-on",
            Some("SomeToolWithALongName"),
            999,
            Some(99),
            9999,
        );
        let line = compose_entry_line(&s, 0, 40);
        assert!(
            console::measure_text_width(&line) <= 40,
            "line '{}' wider than 40",
            line
        );
    }

    #[test]
    fn entry_line_pluralizes_correctly() {
        let one_tool = make_state("agent", None, 1, Some(1), 0);
        let two_tools = make_state("agent", None, 2, Some(2), 0);
        assert!(compose_entry_line(&one_tool, 0, 200).contains("1 tool"));
        assert!(!compose_entry_line(&one_tool, 0, 200).contains("1 tools"));
        assert!(compose_entry_line(&two_tools, 0, 200).contains("2 tools"));
        let one_line = compose_entry_line(&one_tool, 0, 200);
        assert!(one_line.contains("1 turn"), "singular turn: {}", one_line);
        let two_line = compose_entry_line(&two_tools, 0, 200);
        assert!(two_line.contains("2 turns"), "plural turns: {}", two_line);
    }

    #[test]
    fn spinner_cycles_through_all_frames() {
        let s = make_state("agent", None, 0, None, 0);
        let mut seen = std::collections::HashSet::new();
        for i in 0..SPINNER_FRAMES.len() {
            let line = compose_entry_line(&s, i, 200);
            for frame in SPINNER_FRAMES {
                if line.contains(frame) {
                    seen.insert(*frame);
                    break;
                }
            }
        }
        assert_eq!(
            seen.len(),
            SPINNER_FRAMES.len(),
            "all spinner frames must be used"
        );
    }

    // ── compose_status_block ──────────────────────────────────────────────────

    #[test]
    fn empty_states_returns_empty_block() {
        let block = compose_status_block(&[], 0, 200);
        assert!(block.is_empty());
    }

    #[test]
    fn single_delegate_produces_one_line_block() {
        let s = make_state("file-locator", Some("Grep"), 3, None, 5);
        let block = compose_status_block(&[&s], 0, 200);
        assert_eq!(block.len(), 1);
        assert!(block[0].contains("file-locator"));
    }

    #[test]
    fn two_delegates_produce_two_line_block() {
        let a = make_state("file-locator", Some("Grep"), 12, Some(4), 34);
        let b = make_state("codebase-analyzer", Some("Read"), 8, None, 21);
        let block = compose_status_block(&[&a, &b], 1, 200);
        assert_eq!(block.len(), 2);
        assert!(block[0].contains("file-locator"));
        assert!(block[1].contains("codebase-analyzer"));
    }

    #[test]
    fn block_caps_at_max_visible_plus_footer() {
        let states: Vec<DelegateState> = (0..10)
            .map(|i| make_state(&format!("agent-{}", i), None, i, None, i as u64))
            .collect();
        let refs: Vec<&DelegateState> = states.iter().collect();
        let block = compose_status_block(&refs, 0, 200);
        // 7 visible + 1 "… and 3 more" footer = 8 total.
        assert_eq!(block.len(), MAX_VISIBLE + 1);
        assert!(
            block[MAX_VISIBLE].contains("… and 3 more"),
            "footer: {}",
            block[MAX_VISIBLE]
        );
    }

    #[test]
    fn block_no_footer_when_exactly_max_visible() {
        let states: Vec<DelegateState> = (0..MAX_VISIBLE)
            .map(|i| make_state(&format!("agent-{}", i), None, i, None, i as u64))
            .collect();
        let refs: Vec<&DelegateState> = states.iter().collect();
        let block = compose_status_block(&refs, 0, 200);
        assert_eq!(block.len(), MAX_VISIBLE);
        assert!(!block[MAX_VISIBLE - 1].contains("more"));
    }

    // ── sanitize_summary ──────────────────────────────────────────────────────

    #[test]
    fn sanitize_replaces_newlines_with_spaces() {
        let input = "gh api repos/.../issues\nimport sys,json\nprint(1)";
        let out = sanitize_summary(input, 200);
        assert!(!out.contains('\n'), "no newlines: {:?}", out);
        assert!(!out.contains('\r'), "no carriage returns: {:?}", out);
    }

    #[test]
    fn sanitize_collapses_whitespace() {
        let input = "hello   \t  world\n  again";
        let out = sanitize_summary(input, 200);
        assert_eq!(out, "hello world again");
    }

    #[test]
    fn sanitize_caps_length() {
        let long = "a".repeat(300);
        let out = sanitize_summary(&long, 50);
        assert!(
            out.chars().count() <= 50,
            "len {} > 50",
            out.chars().count()
        );
        assert!(out.ends_with('…'), "must end with ellipsis: {:?}", out);
    }

    #[test]
    fn sanitize_short_string_unchanged() {
        let s = "kubectl get pods";
        let out = sanitize_summary(s, 200);
        assert_eq!(out, s);
    }

    #[test]
    fn sanitize_strips_control_chars() {
        let input = "hello\x01\x02\x1b[31mworld\x00";
        let out = sanitize_summary(input, 200);
        assert!(
            !out.chars().any(|c| c.is_ascii_control()),
            "control chars remain: {:?}",
            out
        );
    }

    #[test]
    fn entry_line_multiline_tool_rendered_single_line() {
        // If a tool summary containing newlines somehow reaches the renderer,
        // compose_entry_line must still produce a single-line string.
        let s = make_state(
            "worker",
            Some("gh api ...\nimport sys\nprint(x)"),
            1,
            None,
            1,
        );
        let line = compose_entry_line(&s, 0, 200);
        assert!(!line.contains('\n'), "must be single line: {:?}", line);
        assert!(!line.contains('\r'), "must have no \\r: {:?}", line);
    }

    // ── legacy single-line API (kept for compatibility) ───────────────────────

    #[test]
    fn two_delegates_joined_by_pipe() {
        let a = make_state("file-locator", Some("Grep"), 12, Some(4), 34);
        let b = make_state("codebase-analyzer", Some("Read"), 8, None, 21);
        let line = compose_status_line(&[&a, &b], 1, 200);
        assert!(line.contains(" | "), "delegates must be separated by ' | '");
        assert!(line.contains("file-locator"));
        assert!(line.contains("codebase-analyzer"));
    }

    #[test]
    fn no_delegates_returns_empty() {
        let line = compose_status_line(&[], 0, 200);
        assert!(line.is_empty());
    }

    /// Renderer deduplication: a second claim on the same path must fail.
    #[test]
    fn file_claim_deduplicates() {
        let path = PathBuf::from(format!("/tmp/test-claim-{}.ndjson", std::process::id()));
        assert!(claim_progress_file(&path));
        assert!(!claim_progress_file(&path));
        release_progress_file(&path);
        assert!(claim_progress_file(&path));
        release_progress_file(&path);
    }
}
