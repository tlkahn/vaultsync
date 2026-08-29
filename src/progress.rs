//! Progress event feed + rendering (issue 27, I27-home).
//!
//! This module owns the event model, the `Progress` trait + `NoProgress`
//! default sink, and (later cycles) the pure `ProgressLine` state machine
//! and the TTY/non-TTY renderers. `exec` emits events, `cli` renders;
//! neither depends on the other's rendering. Std-only (I27-deps).

use std::sync::Mutex;

/// Which executor pass an event belongs to (I27-events). The human verb per
/// pass lives in the renderer (`Uploading`, `Downloading`, ...), not here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PassKind {
    Upload,
    Download,
    DeleteRemote,
    DeleteLocal,
}

/// Coarse, pass-scoped progress events emitted by the executor on completion
/// (I27-events). Emission is completion-driven, so under `concurrency > 1`
/// `KeyDone` order may interleave; consumers must never assume plan order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProgressEvent {
    /// A pass is about to start. `total_bytes` is informational (source-side
    /// entity sizes; deletes contribute 0); key counts are the primary
    /// signal.
    PassStart {
        kind: PassKind,
        total_keys: u32,
        total_bytes: u64,
    },
    /// One key finished (transfer or delete). `bytes` is the action's
    /// planned byte size (0 for deletes) regardless of `ok` - the executor
    /// always emits the planned size on success and failure. `ok` mirrors
    /// the per-key result; consumers tracking transferred bytes should
    /// accumulate only on `ok` and subtract a failed key's planned bytes
    /// from the pass total (the in-tree `ProgressLine` does exactly this,
    /// policy B, PR 28 r1 F1).
    KeyDone {
        kind: PassKind,
        key: String,
        bytes: u64,
        ok: bool,
    },
    /// A pass finished (after the per-pass fold).
    PassEnd { kind: PassKind },
    /// The whole run finished (after the final pass fold).
    RunEnd { executed: u32, failed: u32 },
}

/// Sink for executor progress events (I27-thread): `Send + Sync` so worker
/// threads may call it directly under `run_bounded`; implementors serialize
/// internally.
pub trait Progress: Send + Sync {
    fn event(&self, ev: ProgressEvent);
}

/// The default no-op sink: `execute_plan` (the wrapper) passes this so
/// library callers see zero behavior change.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoProgress;

impl Progress for NoProgress {
    fn event(&self, _ev: ProgressEvent) {}
}

/// Pure, IO-free progress-line state machine (I27-home/I27-render): fed
/// executor events (with an injected elapsed-millis clock so rate/ETA stay
/// deterministic, I27-rate), it renders one bounded line per active pass.
/// The renderers (cycle 6) own the `\r`/`\x1b[K` refresh mechanics and the
/// writer; this type only computes text.
///
/// Line layout (I27-render): `{verb:<16}{key:<32}  {done}/{total}  [{bar}]  `
/// `{pct:>3}%  {rate}  ETA {eta}`; rate/ETA are omitted until at least one
/// byte-complete event with non-zero elapsed. Bar: fixed 20 cells (I27-width).
#[derive(Debug, Default)]
pub struct ProgressLine {
    pass: Option<PassKind>,
    total_keys: u32,
    total_bytes: u64,
    done: u32,
    bytes_done: u64,
    current_key: String,
    /// Most recent injected clock value (ms since the run started, as
    /// measured by the renderer's `Instant`).
    now_ms: u64,
    /// Clock value at `PassStart`; elapsed = `now_ms - start_ms`.
    start_ms: u64,
}

impl ProgressLine {
    pub fn new() -> Self {
        ProgressLine::default()
    }

    /// Fold one executor event. Time (`now_ms`) is injected so tests never
    /// touch a wall clock (I27-rate). `PassStart` resets the counters for the
    /// new pass; `KeyDone`/`PassEnd` for the current pass update them;
    /// `RunEnd` and foreign-kind events are ignored.
    pub fn on_event(&mut self, ev: ProgressEvent, now_ms: u64) {
        self.now_ms = now_ms;
        match ev {
            ProgressEvent::PassStart {
                kind,
                total_keys,
                total_bytes,
            } => {
                self.pass = Some(kind);
                self.total_keys = total_keys;
                self.total_bytes = total_bytes;
                self.done = 0;
                self.bytes_done = 0;
                self.current_key.clear();
                self.start_ms = now_ms;
            }
            ProgressEvent::KeyDone {
                kind,
                key,
                bytes,
                ok,
            } => {
                if self.pass == Some(kind) {
                    self.done += 1;
                    // I27-bytes (policy B, PR 28 r1 F1): failed keys still
                    // advance the key count but only successful bytes
                    // accumulate; the failed key's planned bytes leave the
                    // pass total so the final frame stays a clean 100% with
                    // no phantom ETA and rate/ETA reflect bytes that landed.
                    if ok {
                        self.bytes_done += bytes;
                    } else {
                        self.total_bytes = self.total_bytes.saturating_sub(bytes);
                    }
                    self.current_key = key;
                }
            }
            ProgressEvent::PassEnd { kind: _ } => {
                // Keep the final (done == total) state; the renderer emits
                // the final 100% frame with a trailing newline.
            }
            ProgressEvent::RunEnd { .. } => {}
        }
    }

    /// Render the current line. Empty string when there is no active pass or
    /// the pass has `total_keys == 0` (I27-render: such passes render
    /// nothing).
    pub fn render(&self) -> String {
        let Some(pass) = self.pass else {
            return String::new();
        };
        if self.total_keys == 0 {
            return String::new();
        }
        let verb = match pass {
            PassKind::Upload => "Uploading",
            PassKind::Download => "Downloading",
            PassKind::DeleteRemote => "Deleting remote",
            PassKind::DeleteLocal => "Deleting local",
        };
        let key_field = truncate_pad(&self.current_key, 32);
        let fraction = self.done as f64 / self.total_keys as f64;
        let pct = (self.done as u64 * 100) / self.total_keys as u64;
        let bar = bar20(fraction);
        let mut line = format!(
            "{verb:<16}{key_field}  {}/{}  {bar}  {pct:>3}%",
            self.done, self.total_keys
        );
        // I27-rate: cumulative rate only, after at least one byte and a
        // non-zero elapsed; ETA from bytes remaining / rate.
        if self.bytes_done > 0 {
            let elapsed_ms = self.now_ms.saturating_sub(self.start_ms);
            if elapsed_ms > 0 {
                let rate = self.bytes_done as f64 / (elapsed_ms as f64 / 1000.0);
                line.push_str(&format!("  {}", human_rate(rate)));
                if self.total_bytes > self.bytes_done && rate > 0.0 {
                    let remaining = (self.total_bytes - self.bytes_done) as f64;
                    line.push_str(&format!("  ETA {}", format_eta(remaining / rate)));
                }
            }
        }
        line
    }
}

/// Truncate `s` to at most `budget` characters (char-boundary safe) and
/// right-pad with spaces to exactly `budget`, so the following columns never
/// jump (I27-width: long keys are truncated in the line).
fn truncate_pad(s: &str, budget: usize) -> String {
    let mut out: String = s.chars().take(budget).collect();
    let pad = budget.saturating_sub(out.chars().count());
    out.push_str(&" ".repeat(pad));
    out
}

/// Fixed 20-cell bar (I27-width): `[` + 20 cells + `]`. All full at 100%;
/// otherwise `=`-filled cells, a `>` head on the next cell (any non-zero
/// progress), then `-` remainder.
fn bar20(fraction: f64) -> String {
    const CELLS: usize = 20;
    let filled = (fraction * CELLS as f64).floor() as usize;
    let mut s = String::from("[");
    if filled >= CELLS {
        s.push_str(&"=".repeat(CELLS));
    } else if fraction > 0.0 {
        s.push_str(&"=".repeat(filled));
        s.push('>');
        s.push_str(&"-".repeat(CELLS - filled - 1));
    } else {
        s.push_str(&"-".repeat(CELLS));
    }
    s.push(']');
    s
}

/// Human byte rate with one decimal (I27-render sample `12.4 MB/s`; units
/// are the binary `B`/`KiB`/`MiB`/`GiB` per cycle-5 formatter spec).
fn human_rate(rate: f64) -> String {
    const UNITS: [&str; 4] = ["B", "KiB", "MiB", "GiB"];
    let mut v = rate;
    let mut unit = 0;
    while v >= 1024.0 && unit < UNITS.len() - 1 {
        v /= 1024.0;
        unit += 1;
    }
    format!("{v:.1} {}/s", UNITS[unit])
}

/// ETA clock text: `m:ss` under an hour, `h:mm:ss` from an hour up
/// (I27-render `0:01:12` is the hour-prefixed form).
fn format_eta(secs: f64) -> String {
    let total = secs.round() as u64;
    let h = total / 3600;
    let m = (total % 3600) / 60;
    let s = total % 60;
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

/// TTY renderer (I27-home/I27-render): wraps a writer + a [`ProgressLine`]
/// and refreshes one line in place - `\r{line}\x1b[K` on `PassStart`/
/// `KeyDone`, `\r{line}\x1b[K\n` to finalize a `PassEnd`. `RunEnd` and
/// zero-total passes render nothing. `event()` serializes through an internal
/// `Mutex`, so worker-thread emission (I27-thread) is safe and the writer is
/// flushed after every frame (a buffered cursor cannot freeze the bar).
///
/// The writer is `&mut (dyn Write + Send)` (not bare `dyn Write`) so the
/// `Mutex` keeps `TermProgress: Send + Sync` and thus usable behind `dyn
/// Progress` (I27-thread).
pub struct TermProgress<'w> {
    inner: Mutex<TermInner<'w>>,
    start: std::time::Instant,
}

struct TermInner<'w> {
    line: ProgressLine,
    writer: &'w mut (dyn std::io::Write + Send),
}

impl<'w> TermProgress<'w> {
    pub fn new(writer: &'w mut (dyn std::io::Write + Send)) -> Self {
        TermProgress {
            inner: Mutex::new(TermInner {
                line: ProgressLine::new(),
                writer,
            }),
            start: std::time::Instant::now(),
        }
    }
}

impl Progress for TermProgress<'_> {
    fn event(&self, ev: ProgressEvent) {
        if matches!(ev, ProgressEvent::RunEnd { .. }) {
            // The pass line was finalized with a newline at PassEnd; a RunEnd
            // frame would overwrite it without one.
            return;
        }
        let finalize = matches!(ev, ProgressEvent::PassEnd { .. });
        let mut inner = self.inner.lock().unwrap();
        let now = self.start.elapsed().as_millis() as u64;
        inner.line.on_event(ev, now);
        let rendered = inner.line.render();
        if rendered.is_empty() {
            return;
        }
        let frame = if finalize {
            format!("\r{rendered}\x1b[K\n")
        } else {
            format!("\r{rendered}\x1b[K")
        };
        let _ = inner.writer.write_all(frame.as_bytes());
        let _ = inner.writer.flush();
    }
}

/// Non-TTY renderer (I27-tty): the piped/redirected-stderr sink. Holds the
/// writer behind a `Mutex` (so it stays `Send + Sync` for `dyn Progress`, same
/// as [`TermProgress`]) for a uniform API, but writes nothing.
pub struct QuietProgress<'w> {
    _writer: Mutex<&'w mut (dyn std::io::Write + Send)>,
}

impl<'w> QuietProgress<'w> {
    pub fn new(writer: &'w mut (dyn std::io::Write + Send)) -> Self {
        QuietProgress {
            _writer: Mutex::new(writer),
        }
    }
}

impl Progress for QuietProgress<'_> {
    fn event(&self, _ev: ProgressEvent) {}
}

/// Progress renderer selection carried on the run seam (I27-test): `Auto`
/// (the real binary path) follows `stderr().is_terminal()`; tests default to
/// `Off` so captured-stderr contracts are untouched; `Always` forces the bar
/// for CLI progress tests. A future `--progress=` flag reuses this seam
/// without behavior change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProgressMode {
    Auto,
    Off,
    Always,
}

/// What the executor's progress events should do once resolved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolvedMode {
    Render,
    Quiet,
}

/// Pure mode-resolution seam (I27-test): the `is_tty` bool is the injectable
/// stand-in for `std::io::stderr().is_terminal()`.
pub fn resolve_progress_mode(mode: ProgressMode, is_tty: bool) -> ResolvedMode {
    match mode {
        ProgressMode::Auto => {
            if is_tty {
                ResolvedMode::Render
            } else {
                ResolvedMode::Quiet
            }
        }
        ProgressMode::Off => ResolvedMode::Quiet,
        ProgressMode::Always => ResolvedMode::Render,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Extract the 20-cell bar body (between the brackets) from a rendered
    /// line, for exact cell-count assertions.
    fn bar_cells(s: &str) -> String {
        let start = s.find('[').unwrap_or_else(|| panic!("no bar open: {s:?}"));
        let end = s.find(']').unwrap_or_else(|| panic!("no bar close: {s:?}"));
        s[start + 1..end].to_string()
    }

    // I27-render: scripted events into the pure state machine; the rendered
    // line carries verb, done/total, percent and the bar fill per PassKind.
    #[test]
    fn progress_line_renders_pass_progress() {
        let mut line = ProgressLine::new();
        line.on_event(
            ProgressEvent::PassStart {
                kind: PassKind::Upload,
                total_keys: 3,
                total_bytes: 60,
            },
            0,
        );
        let r0 = line.render();
        assert!(r0.contains("Uploading"), "{r0}");
        assert!(r0.contains("0/3"), "{r0}");
        assert!(r0.contains("  0%"), "{r0}");
        assert_eq!(bar_cells(&r0), "-".repeat(20), "0% bar");

        line.on_event(
            ProgressEvent::KeyDone {
                kind: PassKind::Upload,
                key: "a.md".to_string(),
                bytes: 10,
                ok: true,
            },
            100,
        );
        let r1 = line.render();
        assert!(r1.contains("1/3"), "{r1}");
        assert!(r1.contains("33%"), "{r1}");
        // 1/3 of 20 cells = 6 full + head + 13 remainder
        assert_eq!(
            bar_cells(&r1),
            format!("{}>{}", "=".repeat(6), "-".repeat(13))
        );

        line.on_event(
            ProgressEvent::KeyDone {
                kind: PassKind::Upload,
                key: "b.md".to_string(),
                bytes: 20,
                ok: true,
            },
            200,
        );
        line.on_event(
            ProgressEvent::KeyDone {
                kind: PassKind::Upload,
                key: "c.md".to_string(),
                bytes: 30,
                ok: true,
            },
            300,
        );
        let r3 = line.render();
        assert!(r3.contains("3/3"), "{r3}");
        assert!(r3.contains("100%"), "{r3}");
        assert_eq!(bar_cells(&r3), "=".repeat(20), "100% bar");

        // verb per PassKind
        for (kind, verb) in [
            (PassKind::Upload, "Uploading"),
            (PassKind::Download, "Downloading"),
            (PassKind::DeleteRemote, "Deleting remote"),
            (PassKind::DeleteLocal, "Deleting local"),
        ] {
            let mut l = ProgressLine::new();
            l.on_event(
                ProgressEvent::PassStart {
                    kind,
                    total_keys: 1,
                    total_bytes: 1,
                },
                0,
            );
            assert!(l.render().contains(verb), "{verb}: {}", l.render());
        }
    }

    // I27-width: fixed 20-cell bar with exact cell counts at 0/50/100% and a
    // `>` head on every non-complete, non-zero bar.
    #[test]
    fn progress_line_bar_width_and_fill() {
        let mut l = ProgressLine::new();
        l.on_event(
            ProgressEvent::PassStart {
                kind: PassKind::Upload,
                total_keys: 10,
                total_bytes: 1000,
            },
            0,
        );
        assert_eq!(bar_cells(&l.render()), "-".repeat(20));

        // 5/10 = 50% -> 10 full + head + 9 remainder
        for _ in 0..5 {
            l.on_event(
                ProgressEvent::KeyDone {
                    kind: PassKind::Upload,
                    key: "k.md".to_string(),
                    bytes: 100,
                    ok: true,
                },
                100,
            );
        }
        assert_eq!(
            bar_cells(&l.render()),
            format!("{}>{}", "=".repeat(10), "-".repeat(9))
        );

        // 10/10 = 100% -> all full
        for _ in 5..10 {
            l.on_event(
                ProgressEvent::KeyDone {
                    kind: PassKind::Upload,
                    key: "k.md".to_string(),
                    bytes: 100,
                    ok: true,
                },
                200,
            );
        }
        assert_eq!(bar_cells(&l.render()), "=".repeat(20));
    }

    // I27-rate: no rate/ETA before the first byte-complete event; afterwards
    // cumulative rate and ETA match hand-computed values (time injected, so
    // deterministic).
    #[test]
    fn progress_line_shows_rate_and_eta_after_first_byte() {
        let mut l = ProgressLine::new();
        l.on_event(
            ProgressEvent::PassStart {
                kind: PassKind::Upload,
                total_keys: 10,
                total_bytes: 1000,
            },
            0,
        );
        let r0 = l.render();
        assert!(!r0.contains("B/s"), "no rate before bytes: {r0}");
        assert!(!r0.contains("ETA"), "no ETA before bytes: {r0}");

        // 400 bytes at t=2000ms -> 200 B/s; ETA (1000-400)/200 = 3s
        l.on_event(
            ProgressEvent::KeyDone {
                kind: PassKind::Upload,
                key: "a.md".to_string(),
                bytes: 400,
                ok: true,
            },
            2000,
        );
        let r1 = l.render();
        assert!(r1.contains("200.0 B/s"), "{r1}");
        assert!(r1.contains("ETA 0:03"), "{r1}");

        // cumulative 600 bytes at t=4000ms -> 150 B/s; ETA 400/150 = 2.67s
        l.on_event(
            ProgressEvent::KeyDone {
                kind: PassKind::Upload,
                key: "b.md".to_string(),
                bytes: 200,
                ok: true,
            },
            4000,
        );
        let r2 = l.render();
        assert!(r2.contains("150.0 B/s"), "{r2}");
        assert!(r2.contains("ETA 0:03"), "{r2}");
    }

    // I27-width: a very long key is truncated to the fixed column budget with
    // no line overflow.
    #[test]
    fn progress_line_truncates_long_keys() {
        let mut l = ProgressLine::new();
        l.on_event(
            ProgressEvent::PassStart {
                kind: PassKind::Upload,
                total_keys: 2,
                total_bytes: 100,
            },
            0,
        );
        l.on_event(
            ProgressEvent::KeyDone {
                kind: PassKind::Upload,
                key: "x".repeat(200),
                bytes: 50,
                ok: true,
            },
            100,
        );
        let r = l.render();
        assert!(r.contains(&"x".repeat(32)), "key truncated to budget: {r}");
        assert!(
            !r.contains(&"x".repeat(33)),
            "key overflowed the budget: {r}"
        );
        assert!(r.len() < 140, "line too long ({} chars): {r}", r.len());
    }

    // I27-render: a pass with total_keys == 0 renders nothing.
    #[test]
    fn progress_line_zero_total_pass_renders_nothing() {
        let mut l = ProgressLine::new();
        l.on_event(
            ProgressEvent::PassStart {
                kind: PassKind::Upload,
                total_keys: 0,
                total_bytes: 0,
            },
            0,
        );
        assert_eq!(l.render(), "");
        l.on_event(
            ProgressEvent::KeyDone {
                kind: PassKind::Upload,
                key: "a.md".to_string(),
                bytes: 0,
                ok: true,
            },
            10,
        );
        assert_eq!(l.render(), "");
    }

    // I27-render (cycle 6): the TTY renderer refreshes one line in place with
    // `\r` + ANSI clear-to-EOL, never writes `\n` mid-pass, and finalizes the
    // pass with a newline. The final visible line equals the ProgressLine
    // render at done == total.
    #[test]
    fn tty_renderer_refreshes_single_line_and_clears() {
        let mut buf = Vec::new();
        {
            let p = TermProgress::new(&mut buf);
            p.event(ProgressEvent::PassStart {
                kind: PassKind::Upload,
                total_keys: 3,
                total_bytes: 60,
            });
            p.event(ProgressEvent::KeyDone {
                kind: PassKind::Upload,
                key: "a.md".to_string(),
                bytes: 20,
                ok: true,
            });
            p.event(ProgressEvent::KeyDone {
                kind: PassKind::Upload,
                key: "b.md".to_string(),
                bytes: 20,
                ok: true,
            });
            p.event(ProgressEvent::KeyDone {
                kind: PassKind::Upload,
                key: "c.md".to_string(),
                bytes: 20,
                ok: true,
            });
            p.event(ProgressEvent::PassEnd {
                kind: PassKind::Upload,
            });
        }
        let s = String::from_utf8(buf).unwrap();
        // PassStart + 3 KeyDone refreshes + PassEnd finalize = 5 frames, each
        // carrying one \r and one clear-to-EOL.
        assert_eq!(s.matches('\r').count(), 5, "{s:?}");
        assert_eq!(s.matches("\x1b[K").count(), 5, "{s:?}");
        assert_eq!(
            s.matches('\n').count(),
            1,
            "newline only at pass end: {s:?}"
        );
        assert!(s.ends_with("\x1b[K\n"), "pass ends with a newline: {s:?}");
        // first frame renders the 0/n state
        let first = s.split('\r').nth(1).unwrap();
        assert!(first.starts_with("Uploading"), "{first:?}");
        assert!(first.contains("0/3"), "{first:?}");
        // last frame renders the final state
        let last = s.rsplit('\r').next().unwrap();
        assert!(last.contains("3/3"), "{last:?}");
        assert!(last.contains("100%"), "{last:?}");
        // the final visible line matches the pure ProgressLine render up to
        // the percent field (the rate/ETA suffix depends on wall-clock
        // elapsed in TermProgress, which is not scriptable here).
        let mut line = ProgressLine::new();
        line.on_event(
            ProgressEvent::PassStart {
                kind: PassKind::Upload,
                total_keys: 3,
                total_bytes: 60,
            },
            0,
        );
        line.on_event(
            ProgressEvent::KeyDone {
                kind: PassKind::Upload,
                key: "a.md".to_string(),
                bytes: 20,
                ok: true,
            },
            100,
        );
        line.on_event(
            ProgressEvent::KeyDone {
                kind: PassKind::Upload,
                key: "b.md".to_string(),
                bytes: 20,
                ok: true,
            },
            200,
        );
        line.on_event(
            ProgressEvent::KeyDone {
                kind: PassKind::Upload,
                key: "c.md".to_string(),
                bytes: 20,
                ok: true,
            },
            300,
        );
        fn structural(s: &str) -> &str {
            // everything up to and including the trailing '%' (bar and
            // counts; the rate/ETA tail is wall-clock-dependent)
            let pct_end = s.find('%').unwrap() + 1;
            &s[..pct_end]
        }
        let last_trimmed = last.trim_end_matches("\x1b[K\n");
        assert_eq!(structural(last_trimmed), structural(&line.render()));
    }

    // I27-render (cycle 6): after PassEnd the final frame shows total/total
    // and 100%.
    #[test]
    fn tty_renderer_final_line_contains_counts_and_100() {
        let mut buf = Vec::new();
        {
            let p = TermProgress::new(&mut buf);
            p.event(ProgressEvent::PassStart {
                kind: PassKind::Download,
                total_keys: 2,
                total_bytes: 40,
            });
            p.event(ProgressEvent::KeyDone {
                kind: PassKind::Download,
                key: "n/a.md".to_string(),
                bytes: 20,
                ok: true,
            });
            p.event(ProgressEvent::KeyDone {
                kind: PassKind::Download,
                key: "n/b.md".to_string(),
                bytes: 20,
                ok: true,
            });
            p.event(ProgressEvent::PassEnd {
                kind: PassKind::Download,
            });
        }
        let s = String::from_utf8(buf).unwrap();
        let last = s.rsplit('\r').next().unwrap();
        assert!(last.contains("2/2"), "{last:?}");
        assert!(last.contains("100%"), "{last:?}");
        assert!(last.contains("Downloading"), "{last:?}");
    }

    // I27-render (cycle 6): the non-TTY renderer receives the same event
    // stream and writes zero bytes.
    #[test]
    fn quiet_renderer_writes_nothing() {
        let mut buf = Vec::new();
        {
            let p = QuietProgress::new(&mut buf);
            p.event(ProgressEvent::PassStart {
                kind: PassKind::Upload,
                total_keys: 3,
                total_bytes: 60,
            });
            p.event(ProgressEvent::KeyDone {
                kind: PassKind::Upload,
                key: "a.md".to_string(),
                bytes: 20,
                ok: true,
            });
            p.event(ProgressEvent::PassEnd {
                kind: PassKind::Upload,
            });
            p.event(ProgressEvent::RunEnd {
                executed: 3,
                failed: 0,
            });
        }
        assert!(buf.is_empty(), "quiet renderer must write nothing");
    }

    // I27 cycle 6: every refresh is flushed, so a buffered cursor cannot
    // freeze the bar.
    #[test]
    fn renderer_flushes_writer() {
        struct FlushCounting {
            buf: Vec<u8>,
            flushes: usize,
        }
        impl std::io::Write for FlushCounting {
            fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
                self.buf.extend_from_slice(b);
                Ok(b.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                self.flushes += 1;
                Ok(())
            }
        }
        let mut w = FlushCounting {
            buf: Vec::new(),
            flushes: 0,
        };
        {
            let p = TermProgress::new(&mut w);
            p.event(ProgressEvent::PassStart {
                kind: PassKind::Upload,
                total_keys: 2,
                total_bytes: 20,
            });
            p.event(ProgressEvent::KeyDone {
                kind: PassKind::Upload,
                key: "a.md".to_string(),
                bytes: 10,
                ok: true,
            });
            p.event(ProgressEvent::KeyDone {
                kind: PassKind::Upload,
                key: "b.md".to_string(),
                bytes: 10,
                ok: true,
            });
        }
        assert_eq!(w.flushes, 3, "one flush per rendered frame");
    }

    // I27-test: the mode resolution seam - Auto follows the injected TTY
    // flag, Off is always quiet, Always always renders.
    #[test]
    fn progress_mode_auto_uses_stderr_is_terminal() {
        use super::{ProgressMode, ResolvedMode, resolve_progress_mode};
        assert_eq!(
            resolve_progress_mode(ProgressMode::Auto, true),
            ResolvedMode::Render
        );
        assert_eq!(
            resolve_progress_mode(ProgressMode::Auto, false),
            ResolvedMode::Quiet
        );
        assert_eq!(
            resolve_progress_mode(ProgressMode::Off, true),
            ResolvedMode::Quiet
        );
        assert_eq!(
            resolve_progress_mode(ProgressMode::Always, false),
            ResolvedMode::Render
        );
    }

    // I27-events: the event shape lock. Each variant carries its fields
    // unchanged through construction + inspection.
    #[test]
    fn progress_event_variants_carry_fields() {
        use super::{PassKind, ProgressEvent};

        let start = ProgressEvent::PassStart {
            kind: PassKind::Upload,
            total_keys: 3,
            total_bytes: 42,
        };
        match start {
            ProgressEvent::PassStart {
                kind,
                total_keys,
                total_bytes,
            } => {
                assert_eq!(kind, PassKind::Upload);
                assert_eq!(total_keys, 3);
                assert_eq!(total_bytes, 42);
            }
            _ => panic!("wrong variant"),
        }

        let done = ProgressEvent::KeyDone {
            kind: PassKind::Download,
            key: "n/a.md".to_string(),
            bytes: 12,
            ok: true,
        };
        match done {
            ProgressEvent::KeyDone {
                kind,
                key,
                bytes,
                ok,
            } => {
                assert_eq!(kind, PassKind::Download);
                assert_eq!(key, "n/a.md");
                assert_eq!(bytes, 12);
                assert!(ok);
            }
            _ => panic!("wrong variant"),
        }

        let end = ProgressEvent::PassEnd {
            kind: PassKind::DeleteRemote,
        };
        match end {
            ProgressEvent::PassEnd { kind } => {
                assert_eq!(kind, PassKind::DeleteRemote);
            }
            _ => panic!("wrong variant"),
        }

        let run_end = ProgressEvent::RunEnd {
            executed: 7,
            failed: 2,
        };
        match run_end {
            ProgressEvent::RunEnd { executed, failed } => {
                assert_eq!(executed, 7);
                assert_eq!(failed, 2);
            }
            _ => panic!("wrong variant"),
        }
    }

    // I27-api: the default sink accepts every variant and does nothing.
    #[test]
    fn no_progress_accepts_events() {
        use super::{NoProgress, PassKind, Progress, ProgressEvent};

        let sink = NoProgress;
        for ev in [
            ProgressEvent::PassStart {
                kind: PassKind::Upload,
                total_keys: 3,
                total_bytes: 42,
            },
            ProgressEvent::KeyDone {
                kind: PassKind::Upload,
                key: "a.md".to_string(),
                bytes: 5,
                ok: true,
            },
            ProgressEvent::PassEnd {
                kind: PassKind::Upload,
            },
            ProgressEvent::RunEnd {
                executed: 1,
                failed: 0,
            },
        ] {
            sink.event(ev);
        }
    }

    // I27-thread: `dyn Progress` must be usable from worker threads.
    #[test]
    fn progress_is_send_sync() {
        fn assert_ss<T: ?Sized + Send + Sync>() {}
        assert_ss::<dyn super::Progress>();
    }

    // PR 28 r1 F1 (policy B): a failed key advances the key count but its
    // planned bytes NEVER accumulate into `bytes_done`, so rate/ETA reflect
    // bytes that actually landed. RED today: the failure's 600 bytes inflate
    // `bytes_done` to 1000 and the rate renders as 500 B/s.
    #[test]
    fn key_done_failure_bytes_do_not_inflate_rate() {
        let mut line = ProgressLine::new();
        line.on_event(
            ProgressEvent::PassStart {
                kind: PassKind::Upload,
                total_keys: 2,
                total_bytes: 1000,
            },
            0,
        );
        // 600-byte key fails, 400-byte key succeeds, over 2s.
        line.on_event(
            ProgressEvent::KeyDone {
                kind: PassKind::Upload,
                key: "a.md".to_string(),
                bytes: 600,
                ok: false,
            },
            1000,
        );
        line.on_event(
            ProgressEvent::KeyDone {
                kind: PassKind::Upload,
                key: "b.md".to_string(),
                bytes: 400,
                ok: true,
            },
            2000,
        );
        let r = line.render();
        assert!(r.contains("2/2"), "{r}");
        assert!(r.contains("100%"), "{r}");
        // rate from the 400 transferred bytes over 2s, not the 1000 planned
        assert!(
            r.contains("200.0 B/s"),
            "rate must count successful bytes: {r}"
        );
        assert!(!r.contains("ETA"), "no leftover ETA at done==total: {r}");
    }

    // PR 28 r1 F1 (policy B): the failed key's planned bytes leave the pass
    // total (`total_bytes` shrinks), so a mid-pass ETA is computed against
    // the adjusted remainder. RED today: `total_bytes` stays 1000 and the
    // failure's bytes still count toward the rate, giving a wrong ETA.
    #[test]
    fn key_done_failure_shrinks_total_before_pass_end() {
        let mut line = ProgressLine::new();
        line.on_event(
            ProgressEvent::PassStart {
                kind: PassKind::Upload,
                total_keys: 3,
                total_bytes: 1000,
            },
            0,
        );
        // the 600-byte key fails: its planned size leaves the pass total
        line.on_event(
            ProgressEvent::KeyDone {
                kind: PassKind::Upload,
                key: "a.md".to_string(),
                bytes: 600,
                ok: false,
            },
            1000,
        );
        assert_eq!(
            line.total_bytes, 400,
            "failed planned bytes must shrink the pass total"
        );
        // one 200-byte success at t=2000: bytes_done 200 over 2s = 100 B/s;
        // remaining = 400 - 200 = 200 -> ETA 2s (`0:02`).
        line.on_event(
            ProgressEvent::KeyDone {
                kind: PassKind::Upload,
                key: "b.md".to_string(),
                bytes: 200,
                ok: true,
            },
            2000,
        );
        let r = line.render();
        assert!(r.contains("100.0 B/s"), "{r}");
        assert!(
            r.contains("ETA 0:02"),
            "ETA must use the shrunk total (400-200): {r}"
        );
    }

    // PR 28 r1 F1 edge (1): an all-keys-fail pass saturates `total_bytes` to
    // zero and leaves `bytes_done` at zero, so the final frame shows 100%
    // keys with no rate and no ETA. RED today: bytes_done still holds the
    // failed bytes and a 500 B/s rate renders.
    #[test]
    fn key_done_all_failures_saturate_total_to_zero() {
        let mut line = ProgressLine::new();
        line.on_event(
            ProgressEvent::PassStart {
                kind: PassKind::Upload,
                total_keys: 2,
                total_bytes: 1000,
            },
            0,
        );
        line.on_event(
            ProgressEvent::KeyDone {
                kind: PassKind::Upload,
                key: "a.md".to_string(),
                bytes: 600,
                ok: false,
            },
            1000,
        );
        line.on_event(
            ProgressEvent::KeyDone {
                kind: PassKind::Upload,
                key: "b.md".to_string(),
                bytes: 400,
                ok: false,
            },
            2000,
        );
        assert_eq!(line.total_bytes, 0, "total saturates to zero");
        assert_eq!(line.bytes_done, 0, "no bytes transferred");
        let r = line.render();
        assert!(r.contains("2/2"), "{r}");
        assert!(r.contains("100%"), "{r}");
        assert!(
            !r.contains("B/s"),
            "no rate with zero transferred bytes: {r}"
        );
        assert!(!r.contains("ETA"), "no ETA: {r}");
    }

    // PR 28 r1 F1 edge (2): a failed delete (bytes == 0) subtracts zero, so
    // delete failures never distort byte accounting and the render is
    // unaffected. No panic; `total_bytes` stays 0. Green on arrival (the
    // 0-byte failure is a no-op under both policies) - characterization.
    #[test]
    fn key_done_failed_delete_subtracts_zero() {
        let mut line = ProgressLine::new();
        line.on_event(
            ProgressEvent::PassStart {
                kind: PassKind::DeleteRemote,
                total_keys: 1,
                total_bytes: 0,
            },
            0,
        );
        line.on_event(
            ProgressEvent::KeyDone {
                kind: PassKind::DeleteRemote,
                key: "gone.md".to_string(),
                bytes: 0,
                ok: false,
            },
            1000,
        );
        assert_eq!(line.total_bytes, 0);
        assert_eq!(line.bytes_done, 0);
        assert_eq!(line.done, 1);
        let r = line.render();
        assert!(r.contains("1/1"), "{r}");
        assert!(r.contains("100%"), "{r}");
        assert!(!r.contains("B/s"), "{r}");
        assert!(!r.contains("ETA"), "{r}");
    }
}
