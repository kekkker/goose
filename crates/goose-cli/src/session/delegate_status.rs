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
use std::collections::HashMap;
use std::io::{IsTerminal, Write};
use std::time::Instant;
use tokio::sync::mpsc;

// ── spinner ───────────────────────────────────────────────────────────────────

const SPINNER_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

// ── per-delegate state ────────────────────────────────────────────────────────

#[derive(Clone, Debug)]
pub struct DelegateState {
    pub name: String,
    pub current_tool: Option<String>,
    pub event_count: usize,
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
            format!(
                "{} {}:{} ({} evts, {}s)",
                frame, s.name, tool_part, s.event_count, elapsed
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

// ── renderer command ──────────────────────────────────────────────────────────

pub enum RendererCmd {
    /// A delegate tailer started; registers the delegate with the given key.
    Register { key: String, name: String },
    /// A Tool event arrived for the keyed delegate.
    ToolEvent { key: String, tool_name: String },
    /// The keyed delegate finished. Prints a summary line and removes it.
    Done { key: String },
    /// Print a normal line above the status line (clear status, print, redraw).
    PrintLine { text: String },
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

    pub fn tool_event(&self, key: String, tool_name: String) {
        let _ = self.tx.send(RendererCmd::ToolEvent { key, tool_name });
    }

    pub fn delegate_done(&self, key: String) {
        let _ = self.tx.send(RendererCmd::Done { key });
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
                                event_count: 0,
                                started_at: Instant::now(),
                            });
                            order.push(key);
                        }
                        if is_tty {
                            redraw_status(&delegates, &order, &term, &mut spinner_idx, &mut status_drawn);
                        }
                    }
                    Some(RendererCmd::ToolEvent { key, tool_name }) => {
                        if let Some(state) = delegates.get_mut(&key) {
                            if !tool_name.is_empty() {
                                state.current_tool = Some(tool_name);
                            }
                            state.event_count += 1;
                        }
                        if is_tty {
                            redraw_status(&delegates, &order, &term, &mut spinner_idx, &mut status_drawn);
                        }
                    }
                    Some(RendererCmd::Done { key }) => {
                        if let Some(state) = delegates.remove(&key) {
                            order.retain(|k| k != &key);
                            if is_tty && status_drawn {
                                clear_line();
                                status_drawn = false;
                            }
                            let elapsed = state.started_at.elapsed().as_secs();
                            let summary = format!(
                                "    {} {} done — {} tools, {}s",
                                console::style("▸").cyan().dim(),
                                console::style(&state.name).dim(),
                                console::style(state.event_count).dim(),
                                console::style(elapsed).dim(),
                            );
                            println!("{}", summary);
                            let _ = std::io::stdout().flush();
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
    // Pad to width so previous longer lines are fully overwritten.
    let line_width = console::measure_text_width(&line);
    let padding = if width > line_width {
        " ".repeat(width - line_width)
    } else {
        String::new()
    };

    print!("\r{}{}\r", line, padding);
    // Move cursor back to start so next \r rewrite lands at column 0.
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

    fn make_state(name: &str, tool: Option<&str>, evts: usize, age_secs: u64) -> DelegateState {
        DelegateState {
            name: name.to_string(),
            current_tool: tool.map(str::to_string),
            event_count: evts,
            // Fake a started_at that is `age_secs` in the past.
            started_at: Instant::now() - Duration::from_secs(age_secs),
        }
    }

    #[test]
    fn single_delegate_with_tool() {
        let s = make_state("file-locator", Some("Grep"), 12, 34);
        let line = compose_status_line(&[&s], 0, 200);
        assert!(line.contains("file-locator"), "name must appear");
        assert!(line.contains("Grep"), "tool must appear");
        assert!(line.contains("12 evts"), "event count must appear");
        assert!(line.contains("34s"), "elapsed must appear");
        assert!(
            SPINNER_FRAMES.iter().any(|f| line.contains(f)),
            "spinner must appear"
        );
    }

    #[test]
    fn two_delegates_joined_by_pipe() {
        let a = make_state("file-locator", Some("Grep"), 12, 34);
        let b = make_state("codebase-analyzer", Some("Read"), 8, 21);
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
        let s = make_state("agent", None, 0, 0);
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
        let s = make_state("investigator", None, 0, 1);
        let line = compose_status_line(&[&s], 0, 200);
        assert!(line.contains("investigator"));
        // Tool part should be absent (just a space).
        assert!(!line.contains(": None"));
    }
}
