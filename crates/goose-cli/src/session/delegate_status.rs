/// In-place status line for concurrent delegate subagent progress.
///
/// While one or more delegate tailers are active a single line at the bottom of
/// terminal output shows every active delegate's spinner, name, current tool,
/// event count and elapsed seconds — updated in place with carriage-return
/// rewrites.  When a delegate finishes a permanent dim summary line is printed
/// and the delegate is removed from the status.
///
/// Non-TTY fallback: all `\r` tricks are skipped; delegate events are printed
/// as plain appended lines instead.
///
/// All stdout writes (both the status line and any "normal" lines that need to
/// appear above it) must go through the renderer so they don't interleave.
use std::collections::{HashMap, HashSet};
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tokio::sync::mpsc;

// ── spinner ───────────────────────────────────────────────────────────────────

const SPINNER_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

// ── per-delegate state ────────────────────────────────────────────────────────

#[derive(Clone, Debug)]
pub struct DelegateState {
    pub name: String,
    pub current_tool: Option<String>,
    pub tool_count: usize,
    pub turn_count: Option<u32>,
    pub started_at: Instant,
}

// ── pure status-line composer (testable without I/O) ─────────────────────────

/// Build the status line string given the current delegate states.
///
/// `spinner_idx` indexes into `SPINNER_FRAMES` (wrapping).
/// `max_width` is the terminal column count; the result is truncated to fit.
pub fn compose_status_line(
    states: &[&DelegateState],
    spinner_idx: usize,
    max_width: usize,
) -> String {
    if states.is_empty() {
        return String::new();
    }
    let frame = SPINNER_FRAMES[spinner_idx % SPINNER_FRAMES.len()];
    let segments: Vec<String> = states
        .iter()
        .map(|s| {
            let elapsed = s.started_at.elapsed().as_secs();
            let tool_part = s
                .current_tool
                .as_deref()
                .map(|t| format!(" {}", t))
                .unwrap_or_default();
            let tool_word = if s.tool_count == 1 { "tool" } else { "tools" };
            let mut stats = format!("{} {}", s.tool_count, tool_word);
            if let Some(turns) = s.turn_count {
                let turn_word = if turns == 1 { "turn" } else { "turns" };
                stats.push_str(&format!(" · {} {}", turns, turn_word));
            }
            format!(
                "{} {}:{} · {} · {}s",
                frame, s.name, tool_part, stats, elapsed
            )
        })
        .collect();

    let line = segments.join(" | ");

    // Truncate to fit terminal width (account for the carriage-return column).
    if max_width > 0 && console::measure_text_width(&line) > max_width {
        // Trim by character until it fits (Unicode-safe via console).
        let mut chars: Vec<char> = line.chars().collect();
        let ellipsis = "…";
        while !chars.is_empty()
            && console::measure_text_width(&(chars.iter().collect::<String>() + ellipsis))
                > max_width
        {
            chars.pop();
        }
        chars.iter().collect::<String>() + ellipsis
    } else {
        line
    }
}

// ── process-wide file registry (fix 1: deduplicate tailers on same file) ─────

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
    /// Print a normal line above the status line (clear status, print, redraw).
    PrintLine { text: String },
    /// Clear the status line immediately (no redraw).  Used by `clear_now()` so
    /// normal output can safely `println!` without corrupting the status line.
    ClearNow {
        ack: std::sync::Arc<std::sync::atomic::AtomicBool>,
    },
}

// ── process-wide renderer for clearing the status line before normal output ───

/// A process-wide `DelegateStatusRenderer` handle installed by
/// `install_tool_progress_callback` at the start of each turn.  `output.rs`
/// can call `clear_status_line()` to ensure the in-place status line is erased
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

/// Clear the in-place status line immediately, if one is active.
///
/// Safe to call from any thread at any time; no-ops when there is no active
/// renderer or when stdout is not a TTY.  Printing helpers in `output.rs` call
/// this before every block of normal output so the status line is never
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
    /// above the status line.
    pub fn print_line(&self, text: String) {
        if self.is_tty {
            let _ = self.tx.send(RendererCmd::PrintLine { text });
        } else {
            println!("{}", text);
        }
    }

    /// Synchronously clear the status line so the caller can safely `println!`
    /// without corrupting the in-place status line.
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
    // Whether a status line is currently drawn on the current terminal row.
    let mut status_drawn = false;
    // Width cache — re-queried on each redraw.
    let term = console::Term::stdout();

    // Tick interval: update elapsed counter at ~1 Hz even with no events.
    let mut tick = tokio::time::interval(tokio::time::Duration::from_millis(100));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            biased;

            cmd = rx.recv() => {
                match cmd {
                    None => {
                        // All senders dropped; clean up and exit.
                        if is_tty && status_drawn {
                            clear_line();
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
                            redraw_status(&delegates, &order, &term, &mut spinner_idx, &mut status_drawn);
                        }
                    }
                    Some(RendererCmd::ToolEvent { key, tool_name, tool_count, turn_count }) => {
                        if let Some(state) = delegates.get_mut(&key) {
                            if !tool_name.is_empty() {
                                state.current_tool = Some(tool_name);
                            }
                            // Prefer explicit counter from event; fall back to incrementing.
                            state.tool_count = tool_count
                                .map(|c| c as usize)
                                .unwrap_or_else(|| state.tool_count + 1);
                            if turn_count.is_some() {
                                state.turn_count = turn_count;
                            }
                        }
                        if is_tty {
                            redraw_status(&delegates, &order, &term, &mut spinner_idx, &mut status_drawn);
                        }
                    }
                    Some(RendererCmd::AdoptName { key, name }) => {
                        if let Some(state) = delegates.get_mut(&key) {
                            // Only override if the current name looks like a bare session id
                            // (e.g. "20260611_193") — don't clobber a meaningful delegate name.
                            if goose::agents::platform_extensions::summon::is_session_id(&state.name) {
                                state.name = name;
                            }
                        }
                        if is_tty {
                            redraw_status(&delegates, &order, &term, &mut spinner_idx, &mut status_drawn);
                        }
                    }
                    Some(RendererCmd::Done { key, silent }) => {
                        if let Some(state) = delegates.remove(&key) {
                            order.retain(|k| k != &key);
                            if is_tty && status_drawn {
                                clear_line();
                                status_drawn = false;
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
                            redraw_status(&delegates, &order, &term, &mut spinner_idx, &mut status_drawn);
                        }
                    }
                    Some(RendererCmd::PrintLine { text }) => {
                        if is_tty && status_drawn {
                            clear_line();
                            status_drawn = false;
                        }
                        println!("{}", text);
                        let _ = std::io::stdout().flush();
                        if is_tty && !delegates.is_empty() {
                            redraw_status(&delegates, &order, &term, &mut spinner_idx, &mut status_drawn);
                        }
                    }
                    Some(RendererCmd::ClearNow { ack }) => {
                        if is_tty && status_drawn {
                            clear_line();
                            status_drawn = false;
                        }
                        ack.store(true, std::sync::atomic::Ordering::Release);
                    }
                }
            }

            _ = tick.tick() => {
                if is_tty && !delegates.is_empty() {
                    spinner_idx = spinner_idx.wrapping_add(1);
                    redraw_status(&delegates, &order, &term, &mut spinner_idx, &mut status_drawn);
                }
            }
        }
    }
}

fn redraw_status(
    delegates: &HashMap<String, DelegateState>,
    order: &[String],
    term: &console::Term,
    spinner_idx: &mut usize,
    status_drawn: &mut bool,
) {
    let width = term.size_checked().map(|(_, w)| w as usize).unwrap_or(80);
    let states: Vec<&DelegateState> = order.iter().filter_map(|k| delegates.get(k)).collect();

    if states.is_empty() {
        if *status_drawn {
            clear_line();
            *status_drawn = false;
        }
        return;
    }

    let line = compose_status_line(&states, *spinner_idx, width);
    // Pad to width so previous longer lines are fully overwritten, then place
    // cursor back at column 0.  Using a single leading \r avoids the glitch
    // where two \r in one write can let a partial previous line bleed through
    // on some terminals.
    let line_width = console::measure_text_width(&line);
    let padding = if width > line_width {
        " ".repeat(width - line_width)
    } else {
        String::new()
    };

    print!("\r{}{}", line, padding);
    // Move cursor back to column 0 so the next redraw overwrites this line.
    print!("\r");
    let _ = std::io::stdout().flush();
    *status_drawn = true;
}

fn clear_line() {
    // Overwrite with spaces then return carriage to column 0.
    let width = console::Term::stdout()
        .size_checked()
        .map(|(_, w)| w as usize)
        .unwrap_or(80);
    print!("\r{}\r", " ".repeat(width));
    let _ = std::io::stdout().flush();
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
            // Fake a started_at that is `age_secs` in the past.
            started_at: Instant::now() - Duration::from_secs(age_secs),
        }
    }

    #[test]
    fn single_delegate_with_tool_and_counters() {
        let s = make_state("file-locator", Some("Grep"), 12, Some(3), 34);
        let line = compose_status_line(&[&s], 0, 200);
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

    #[test]
    fn truncates_to_max_width() {
        let s = make_state(
            "very-long-delegate-name-that-goes-on-and-on",
            Some("SomeToolWithALongName"),
            999,
            Some(99),
            9999,
        );
        let line = compose_status_line(&[&s], 0, 40);
        // Visible width must not exceed 40.
        assert!(
            console::measure_text_width(&line) <= 40,
            "line '{}' wider than 40",
            line
        );
    }

    #[test]
    fn spinner_cycles_through_frames() {
        let s = make_state("agent", None, 0, None, 0);
        let mut seen = std::collections::HashSet::new();
        for i in 0..SPINNER_FRAMES.len() {
            let line = compose_status_line(&[&s], i, 200);
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

    #[test]
    fn delegate_without_tool_shows_name_only() {
        let s = make_state("investigator", None, 0, None, 1);
        let line = compose_status_line(&[&s], 0, 200);
        assert!(line.contains("investigator"));
        // Tool part should be absent (just no tool name after `:`)
        assert!(!line.contains(": None"));
    }

    #[test]
    fn omit_turn_count_when_none() {
        let s = make_state("agent", Some("shell"), 5, None, 10);
        let line = compose_status_line(&[&s], 0, 200);
        assert!(line.contains("5 tools"), "tool count must appear");
        assert!(
            !line.contains("turns"),
            "turn count must be absent when None"
        );
    }

    /// Renderer deduplication: a second claim on the same path must fail.
    #[test]
    fn file_claim_deduplicates() {
        // Use a fresh unique path to avoid interference with other tests.
        let path = PathBuf::from(format!("/tmp/test-claim-{}.ndjson", std::process::id()));
        // First claim succeeds.
        assert!(claim_progress_file(&path));
        // Second claim on same path fails.
        assert!(!claim_progress_file(&path));
        // After release, claim succeeds again.
        release_progress_file(&path);
        assert!(claim_progress_file(&path));
        // Clean up.
        release_progress_file(&path);
    }
}
