//! Progress event feed + rendering (issue 27, I27-home).
//!
//! This module owns the event model, the `Progress` trait + `NoProgress`
//! default sink, the pure `ProgressLine` state machine, and the TTY/non-TTY
//! renderers. `exec` emits events, `cli` renders; neither depends on the
//! other's rendering. Std-only (I27-deps).

use std::sync::Mutex;

/// Fixed progress-line budgets (PR 28 r2 F1, Option A): a 12-column verb field,
/// a 12-column key field, and an 8-cell bar. Together they keep the common
/// worst-supported frame at 80 columns or fewer; terminal-width detection is a
/// possible follow-up (Option B), not this PR.
const VERB_BUDGET: usize = 12;
const KEY_BUDGET: usize = 12;
const BAR_CELLS: usize = 8;

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
    /// Issue 42 (I42-events, B1): the cold inventory is about to run. The
    /// optional opening bracket of the plan phase; renders nothing on its
    /// own (blank until the first `ListPage`/`HeadsStart`, W327 lock). Only
    /// emitted on the COLD path - warm manifest loads emit zero plan-phase
    /// events (I42-warm-events).
    PlanStart,
    /// Issue 42 (I42-pages): one `ListObjectsV2` page completed. `page` is
    /// 1-based; `keys_so_far` is the CUMULATIVE raw object-row count from the
    /// page `contents` (pre-folder-synth; matches wall-time work, W342 lock).
    /// No total-page denominator - S3 does not know it up front.
    ListPage { page: u32, keys_so_far: u64 },
    /// Issue 42 (I42-heads): the head-enrichment fan-out is about to start;
    /// `total_keys` is the object rows that WILL be headed (post-reserved,
    /// non-folder). Skipped when there are zero object rows.
    HeadsStart { total_keys: u32 },
    /// Issue 42 (I42-heads): one object head completed (`done` is 1..=total,
    /// success or NotFound-vanish). Under concurrency > 1 the order may
    /// interleave; totals are pinned, not order.
    HeadDone { done: u32, total_keys: u32 },
    /// Issue 42 (I42-finalize): the cold inventory finished (success path).
    /// The renderer finalizes the plan frame with a newline here; the CLI
    /// must observe this (or its own `finish_plan` belt-and-braces) before
    /// printing W236 / warnings so `\r` bars never collide. Not emitted on
    /// mid-cold failure (CLI clears defensively).
    PlanEnd,
}

/// Sink for executor progress events (I27-thread): `Send + Sync` so worker
/// threads may call it directly under `run_bounded`; implementors serialize
/// internally.
pub trait Progress: Send + Sync {
    fn event(&self, ev: ProgressEvent);

    /// Issue 42 belt-and-braces (I42-finalize): finalize a partial plan bar
    /// with a newline so later stderr lines (warnings, W236, errors) never
    /// collide with a mid-line `\r` frame - even when the library could not
    /// emit `PlanEnd` (mid-cold failure). The CLI calls this after every
    /// cold plan attempt; the TTY renderer finalizes at `PlanEnd` already, so
    /// this is a no-op there. Default: no-op (Quiet/NoProgress sinks write
    /// nothing).
    fn finish_plan(&self) {}
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
/// Line layout (I27-render): `{verb:<12}{key:<12}  {done}/{total}  [{bar}]  `
/// `{pct:>3}% {rate} ETA {eta}`; rate/ETA are omitted until at least one
/// byte-complete event with non-zero elapsed. Fixed 80-column budget (PR 28 r2
/// F1, Option A): 12-column verb field, 12-column key field, 8-cell bar,
/// one-space rate/ETA suffixes. No terminal-width detection (Option B).
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
    /// `RunEnd` and foreign-kind events are ignored. The injected clock only
    /// advances on ACCEPTED events (W330): foreign/plan-phase events leave
    /// the line machine byte-identical, so `render()` after them equals the
    /// previous executor state exactly.
    pub fn on_event(&mut self, ev: ProgressEvent, now_ms: u64) {
        match ev {
            ProgressEvent::PassStart {
                kind,
                total_keys,
                total_bytes,
            } => {
                self.now_ms = now_ms;
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
                    self.now_ms = now_ms;
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
            // I42 (W326): plan-phase events are foreign to the executor line
            // machine - ignored here, routed to `PlanProgressLine` by the
            // renderers (W330 pins the symmetric ignore).
            ProgressEvent::PlanStart
            | ProgressEvent::ListPage { .. }
            | ProgressEvent::HeadsStart { .. }
            | ProgressEvent::HeadDone { .. }
            | ProgressEvent::PlanEnd => {}
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
            PassKind::DeleteRemote => "Del remote",
            PassKind::DeleteLocal => "Del local",
        };
        let key_field = truncate_pad(&self.current_key, KEY_BUDGET);
        let fraction = self.done as f64 / self.total_keys as f64;
        let pct = (self.done as u64 * 100) / self.total_keys as u64;
        let bar = bar(fraction);
        let mut line = format!(
            "{verb:<width$}{key_field}  {}/{}  {bar}  {pct:>3}%",
            self.done,
            self.total_keys,
            width = VERB_BUDGET
        );
        // I27-rate: cumulative rate only, after at least one byte and a
        // non-zero elapsed; ETA from bytes remaining / rate.
        if self.bytes_done > 0 {
            let elapsed_ms = self.now_ms.saturating_sub(self.start_ms);
            if elapsed_ms > 0 {
                let rate = self.bytes_done as f64 / (elapsed_ms as f64 / 1000.0);
                line.push_str(&format!(" {}", human_rate(rate)));
                if self.total_bytes > self.bytes_done && rate > 0.0 {
                    let remaining = (self.total_bytes - self.bytes_done) as f64;
                    line.push_str(&format!(" ETA {}", format_eta(remaining / rate)));
                }
            }
        }
        line
    }
}

/// Pure, IO-free plan-phase line state machine (I42-line, B2): fed the
/// issue-42 plan-phase events (`PlanStart` / `ListPage` / `HeadsStart` /
/// `HeadDone` / `PlanEnd`), it renders one bounded line per active phase - a
/// cumulative listing line or a heading bar line. Foreign executor events
/// never mutate it (W329), and the executor `ProgressLine` symmetrically
/// ignores plan-phase events (W330). No byte rate / ETA on the plan phase
/// (I42-line).
///
/// Line layout (mirrors the executor budgets): `Listing     page N  K keys`
/// (12-col verb) and `Heading     D/T  [bar]  P%` (12-col verb + 8-cell bar).
/// Exact strings are locked by the pure-line unit tests before any renderer
/// wiring (W327/W328).
#[derive(Debug, Default)]
pub struct PlanProgressLine {
    /// (page, keys_so_far) after the most recent `ListPage`.
    listing: Option<(u32, u64)>,
    /// Heading totals once `HeadsStart` armed: (done, total).
    heading: Option<(u32, u32)>,
}

impl PlanProgressLine {
    pub fn new() -> Self {
        PlanProgressLine::default()
    }

    /// Fold one plan-phase event. `PlanStart` renders nothing on its own
    /// (blank until the first `ListPage`/`HeadsStart`, W327 lock); `PlanEnd`
    /// keeps the last frame state for the renderer to print with a newline
    /// (I42-finalize); foreign executor events are ignored (W329).
    pub fn on_event(&mut self, ev: ProgressEvent) {
        match ev {
            ProgressEvent::PlanStart => {}
            ProgressEvent::ListPage { page, keys_so_far } => {
                self.listing = Some((page, keys_so_far));
            }
            ProgressEvent::HeadsStart { total_keys } => {
                self.heading = Some((0, total_keys));
            }
            ProgressEvent::HeadDone { done, total_keys } => {
                self.heading = Some((done, total_keys));
            }
            ProgressEvent::PlanEnd => {}
            ProgressEvent::PassStart { .. }
            | ProgressEvent::KeyDone { .. }
            | ProgressEvent::PassEnd { .. }
            | ProgressEvent::RunEnd { .. } => {}
        }
    }

    /// Render the current plan-phase line. Empty when nothing has arrived,
    /// or a heading with `total_keys == 0` (mirrors the executor zero-total
    /// policy, W328).
    pub fn render(&self) -> String {
        if let Some((done, total)) = self.heading {
            if total == 0 {
                return String::new();
            }
            let fraction = done as f64 / total as f64;
            let pct = (done as u64 * 100) / total as u64;
            format!(
                "{:<width$}{}/{}  {}  {:>3}%",
                "Heading",
                done,
                total,
                bar(fraction),
                pct,
                width = VERB_BUDGET
            )
        } else if let Some((page, keys_so_far)) = self.listing {
            format!(
                "{:<width$}page {}  {} keys",
                "Listing",
                page,
                keys_so_far,
                width = VERB_BUDGET
            )
        } else {
            String::new()
        }
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

/// Fixed 8-cell bar (PR 28 r2 F1): `[` + 8 cells + `]` (10 columns total).
/// All full at 100%;
/// otherwise `=`-filled cells, a `>` head on the next cell (any non-zero
/// progress), then `-` remainder.
fn bar(fraction: f64) -> String {
    let filled = (fraction * BAR_CELLS as f64).floor() as usize;
    let mut s = String::from("[");
    if filled >= BAR_CELLS {
        s.push_str(&"=".repeat(BAR_CELLS));
    } else if fraction > 0.0 {
        s.push_str(&"=".repeat(filled));
        s.push('>');
        s.push_str(&"-".repeat(BAR_CELLS - filled - 1));
    } else {
        s.push_str(&"-".repeat(BAR_CELLS));
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

/// ETA clock text (PR 28 r1 F2): renders `m:ss` under an hour (`1:12` for
/// 72 s) and `h:mm:ss` from an hour up (`1:01:01` for 3661 s). No hour
/// prefix is shown below an hour.
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

/// True for issue-42 plan-phase events, routed to [`PlanProgressLine`] by the
/// renderers (W331). Executor events route to [`ProgressLine`].
fn is_plan_phase(ev: &ProgressEvent) -> bool {
    matches!(
        ev,
        ProgressEvent::PlanStart
            | ProgressEvent::ListPage { .. }
            | ProgressEvent::HeadsStart { .. }
            | ProgressEvent::HeadDone { .. }
            | ProgressEvent::PlanEnd
    )
}

/// TTY renderer (I27-home/I27-render): wraps a writer + a [`ProgressLine`]
/// and refreshes one line in place - `\r{line}\x1b[K` on `PassStart`/
/// `KeyDone`, `\r{line}\x1b[K\n` to finalize a `PassEnd`. `RunEnd` and
/// zero-total passes render nothing. `event()` serializes through an internal
/// `Mutex`, so worker-thread emission (I27-thread) is safe and the writer is
/// flushed after every frame (a buffered cursor cannot freeze the bar).
///
/// Issue 42 (W331+): plan-phase events route to a [`PlanProgressLine`]
/// beside the executor line. `PlanEnd` finalizes the plan frame with a
/// newline; `finish_plan()` is the belt-and-braces finalize for error paths
/// where the library could not emit `PlanEnd` (I42-finalize).
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
    plan_line: PlanProgressLine,
    /// Set once the plan frame was finalized with a newline (`PlanEnd` or
    /// `finish_plan`) so the belt-and-braces finalize is idempotent.
    plan_finalized: bool,
    writer: &'w mut (dyn std::io::Write + Send),
}

impl<'w> TermProgress<'w> {
    pub fn new(writer: &'w mut (dyn std::io::Write + Send)) -> Self {
        TermProgress {
            inner: Mutex::new(TermInner {
                line: ProgressLine::new(),
                plan_line: PlanProgressLine::new(),
                plan_finalized: false,
                writer,
            }),
            start: std::time::Instant::now(),
        }
    }
}

/// Issue 42 finalize (I42-finalize, W370/W372): write the single plan
/// finalize frame - `\r{rendered}\x1b[K\n` - exactly once. Idempotent (guards
/// on `plan_finalized`); marks `plan_finalized` even when there is nothing to
/// render (L1: a `PlanEnd` with an empty render still finalizes, so a later
/// belt-and-braces call cannot race a future non-empty state). Shared by both
/// the `PlanEnd` path in `event` and the `finish_plan` trait override so the
/// finalize frame format string lives in one place (no drift).
fn finalize_plan_frame(inner: &mut TermInner) {
    if inner.plan_finalized {
        return;
    }
    inner.plan_finalized = true;
    let rendered = inner.plan_line.render();
    if rendered.is_empty() {
        return;
    }
    let frame = format!("\r{rendered}\x1b[K\n");
    let _ = inner.writer.write_all(frame.as_bytes());
    let _ = inner.writer.flush();
}

impl Progress for TermProgress<'_> {
    /// Issue 42 belt-and-braces (I42-finalize, W370): finalize a partial plan
    /// bar with a newline so later stderr lines (warnings, W236, errors) never
    /// collide with a mid-line `\r` frame - even when the library could not
    /// emit `PlanEnd` (mid-cold failure). No-op when the plan line was already
    /// finalized via `PlanEnd` or there is nothing to render. This override is
    /// what makes `Box<dyn Progress>::finish_plan()` (the CLI's renderer
    /// boundary) run the real body; before it, dispatch hit the trait default
    /// no-op (H1).
    fn finish_plan(&self) {
        let mut inner = self.inner.lock().unwrap();
        finalize_plan_frame(&mut inner);
    }

    fn event(&self, ev: ProgressEvent) {
        if matches!(ev, ProgressEvent::RunEnd { .. }) {
            // The pass line was finalized with a newline at PassEnd; a RunEnd
            // frame would overwrite it without one.
            return;
        }
        // Issue 42 (W331): plan-phase events drive the plan line machine and
        // finalize at `PlanEnd` (newline). The executor line machine never
        // sees them, and the plan line never sees executor events (W329/W330).
        if is_plan_phase(&ev) {
            let finalize = matches!(ev, ProgressEvent::PlanEnd);
            let mut inner = self.inner.lock().unwrap();
            inner.plan_line.on_event(ev);
            if finalize {
                // PlanEnd updates the line state first (keeps the last frame);
                // the shared helper writes the single finalize frame. L1: it
                // marks finalize even when the render is empty.
                finalize_plan_frame(&mut inner);
            } else {
                let rendered = inner.plan_line.render();
                if rendered.is_empty() {
                    return;
                }
                let frame = format!("\r{rendered}\x1b[K");
                let _ = inner.writer.write_all(frame.as_bytes());
                let _ = inner.writer.flush();
            }
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
        assert_eq!(bar_cells(&r0), "-".repeat(8), "0% bar");

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
        // 1/3 of 8 cells = 2 full + head + 5 remainder
        assert_eq!(
            bar_cells(&r1),
            format!("{}>{}", "=".repeat(2), "-".repeat(5))
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
        assert_eq!(bar_cells(&r3), "=".repeat(8), "100% bar");

        // verb per PassKind
        for (kind, verb) in [
            (PassKind::Upload, "Uploading"),
            (PassKind::Download, "Downloading"),
            (PassKind::DeleteRemote, "Del remote"),
            (PassKind::DeleteLocal, "Del local"),
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
        assert_eq!(bar_cells(&l.render()), "-".repeat(8));

        // 5/10 = 50% -> 4 full + head + 3 remainder
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
            format!("{}>{}", "=".repeat(4), "-".repeat(3))
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
        assert_eq!(bar_cells(&l.render()), "=".repeat(8));
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
        assert!(r.contains(&"x".repeat(12)), "key truncated to budget: {r}");
        assert!(
            !r.contains(&"x".repeat(13)),
            "key overflowed the budget: {r}"
        );
        assert!(
            r.chars().count() <= 80,
            "line too long ({} chars): {r}",
            r.chars().count()
        );
    }

    // PR 28 r2 F1 (Option A): the common worst-supported frame fits an
    // 80-column TTY. DeleteRemote verb, a 10k-key fraction at its widest
    // (9999/10000), an overlong current key (must truncate), a max-width
    // rate shape and an hour ETA.
    #[test]
    fn progress_line_worst_supported_frame_fits_80_columns() {
        let mut line = ProgressLine::new();
        // rate = bytes_done / elapsed_s must render the max-width shape
        // "1023.9 GiB/s". Use elapsed 1000ms so rate == bytes_done; then
        // pick bytes_done so human_rate lands on 1023.9.
        let bytes_done: u64 = (1023.9_f64 * 1024f64.powi(3)) as u64;
        // remaining / rate = 3661 s -> ETA "1:01:01", so the pass total is
        // bytes_done * (3661 + 1) with bytes_done already transferred.
        let total_bytes: u64 = bytes_done * 3662;
        line.on_event(
            ProgressEvent::PassStart {
                kind: PassKind::DeleteRemote,
                total_keys: 10_000,
                total_bytes,
            },
            0,
        );
        // 9998 zero-byte successes advance the key count without touching
        // bytes (kept at the public state-machine boundary).
        for _ in 0..9998 {
            line.on_event(
                ProgressEvent::KeyDone {
                    kind: PassKind::DeleteRemote,
                    key: "x".to_string(),
                    bytes: 0,
                    ok: true,
                },
                0,
            );
        }
        // final key: overlong (must truncate) and carries the byte total at
        // elapsed = 1000ms.
        line.on_event(
            ProgressEvent::KeyDone {
                kind: PassKind::DeleteRemote,
                key: "k".repeat(200),
                bytes: bytes_done,
                ok: true,
            },
            1000,
        );
        let r = line.render();
        assert!(r.contains("Del remote"), "abbreviated delete verb: {r}");
        assert!(r.contains("9999/10000"), "10k fraction: {r}");
        assert!(r.contains("1023.9 GiB/s"), "max-rate shape: {r}");
        assert!(r.contains("ETA 1:01:01"), "hour ETA: {r}");
        assert!(
            r.chars().count() <= 80,
            "frame width {} > 80: {r}",
            r.chars().count()
        );
    }

    // PR 28 r2 F2: the documented 70% sample (issue 27 / doc/cli.md) shows
    // an 8-cell bar body `=====>--`: five filled, one `>` head, two
    // remaining. Test-side counterpart to the markdown sample.
    #[test]
    fn progress_line_seventy_percent_sample_has_8_cell_bar() {
        let mut l = ProgressLine::new();
        l.on_event(
            ProgressEvent::PassStart {
                kind: PassKind::Upload,
                total_keys: 100,
                total_bytes: 100_000,
            },
            0,
        );
        for _ in 0..70 {
            l.on_event(
                ProgressEvent::KeyDone {
                    kind: PassKind::Upload,
                    key: "notes/foo.md".to_string(),
                    bytes: 1000,
                    ok: true,
                },
                1000,
            );
        }
        assert_eq!(bar_cells(&l.render()), "=====>--");
    }

    // I42-render (W333): the non-TTY renderer swallows the full plan-phase
    // sequence - the writer stays empty (same contract as the executor path).
    #[test]
    fn quiet_renderer_swallows_plan_phase() {
        let mut buf = Vec::new();
        {
            let p = QuietProgress::new(&mut buf);
            p.event(ProgressEvent::PlanStart);
            p.event(ProgressEvent::ListPage {
                page: 1,
                keys_so_far: 1000,
            });
            p.event(ProgressEvent::HeadsStart { total_keys: 5 });
            p.event(ProgressEvent::HeadDone {
                done: 2,
                total_keys: 5,
            });
            p.event(ProgressEvent::PlanEnd);
        }
        assert!(buf.is_empty(), "quiet renderer must write nothing");
    }

    // I42-render (W332): `PlanEnd` finalizes the plan frame with a newline
    // (and clear); a subsequent executor pass starts a fresh line that never
    // corrupts the finalized plan line (split contents on `\n` and assert).
    #[test]
    fn tty_renderer_plan_end_then_executor_pass() {
        let mut buf = Vec::new();
        {
            let p = TermProgress::new(&mut buf);
            p.event(ProgressEvent::PlanStart);
            p.event(ProgressEvent::ListPage {
                page: 1,
                keys_so_far: 1000,
            });
            p.event(ProgressEvent::ListPage {
                page: 2,
                keys_so_far: 2000,
            });
            p.event(ProgressEvent::PlanEnd);
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
        }
        let s = String::from_utf8(buf).unwrap();
        let mut lines = s.split('\n');
        let plan_part = lines.next().unwrap();
        let exec_part = lines.next().unwrap();
        assert!(
            plan_part.contains("Listing"),
            "plan frames on the first line: {plan_part:?}"
        );
        assert!(
            plan_part.contains("2000 keys"),
            "final plan frame state: {plan_part:?}"
        );
        assert!(
            plan_part.ends_with("\x1b[K"),
            "plan end clears the line: {plan_part:?}"
        );
        assert!(
            exec_part.contains("Uploading"),
            "executor frames on the next line: {exec_part:?}"
        );
        assert!(exec_part.contains("1/2"), "{exec_part:?}");
        assert_eq!(lines.next(), None, "no stray extra lines: {s:?}");
    }

    // I42-render (W331): TermProgress routes plan-phase events to the plan
    // line machine with in-place `\r` refresh per `ListPage`; no newline is
    // written until `PlanEnd` (I42-finalize).
    #[test]
    fn tty_renderer_refreshes_plan_listing_frames() {
        let mut buf = Vec::new();
        {
            let p = TermProgress::new(&mut buf);
            p.event(ProgressEvent::PlanStart);
            p.event(ProgressEvent::ListPage {
                page: 1,
                keys_so_far: 1000,
            });
            p.event(ProgressEvent::ListPage {
                page: 2,
                keys_so_far: 2000,
            });
        }
        let s = String::from_utf8(buf).unwrap();
        assert_eq!(
            s.matches('\r').count(),
            2,
            "one refresh per ListPage: {s:?}"
        );
        assert_eq!(
            s.matches('\n').count(),
            0,
            "no newline before PlanEnd: {s:?}"
        );
        assert!(s.contains("Listing"), "{s:?}");
        assert!(s.contains("2000 keys"), "latest key count: {s:?}");
        assert!(s.contains("\x1b[K"), "clear-to-EOL: {s:?}");
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

    // I42-events (W326): the plan-phase variants exist and are constructible;
    // each carries its fields unchanged through construction + inspection
    // (event-shape lock for the W-series).
    #[test]
    fn plan_progress_event_variants_carry_fields() {
        use super::ProgressEvent;

        match ProgressEvent::PlanStart {
            ProgressEvent::PlanStart => {}
            _ => panic!("wrong variant"),
        }

        match (ProgressEvent::ListPage {
            page: 1,
            keys_so_far: 1000,
        }) {
            ProgressEvent::ListPage { page, keys_so_far } => {
                assert_eq!(page, 1);
                assert_eq!(keys_so_far, 1000);
            }
            _ => panic!("wrong variant"),
        }

        match (ProgressEvent::HeadsStart { total_keys: 3 }) {
            ProgressEvent::HeadsStart { total_keys } => {
                assert_eq!(total_keys, 3);
            }
            _ => panic!("wrong variant"),
        }

        match (ProgressEvent::HeadDone {
            done: 1,
            total_keys: 3,
        }) {
            ProgressEvent::HeadDone { done, total_keys } => {
                assert_eq!(done, 1);
                assert_eq!(total_keys, 3);
            }
            _ => panic!("wrong variant"),
        }

        match ProgressEvent::PlanEnd {
            ProgressEvent::PlanEnd => {}
            _ => panic!("wrong variant"),
        }
    }

    // I42-line (W330): the executor `ProgressLine` ignores plan-phase
    // variants - state unchanged, and an active executor frame survives them
    // intact (symmetric with `PlanProgressLine` ignoring executor events,
    // W329).
    #[test]
    fn progress_line_ignores_plan_phase_events() {
        let mut line = ProgressLine::new();
        line.on_event(ProgressEvent::PlanStart, 0);
        line.on_event(
            ProgressEvent::ListPage {
                page: 1,
                keys_so_far: 1000,
            },
            0,
        );
        line.on_event(ProgressEvent::HeadsStart { total_keys: 5 }, 0);
        line.on_event(
            ProgressEvent::HeadDone {
                done: 1,
                total_keys: 5,
            },
            0,
        );
        line.on_event(ProgressEvent::PlanEnd, 0);
        assert_eq!(
            line.render(),
            "",
            "plan-phase events must not arm an executor frame"
        );

        // an active executor pass survives plan-phase events unchanged
        line.on_event(
            ProgressEvent::PassStart {
                kind: PassKind::Upload,
                total_keys: 2,
                total_bytes: 20,
            },
            0,
        );
        line.on_event(
            ProgressEvent::KeyDone {
                kind: PassKind::Upload,
                key: "a.md".to_string(),
                bytes: 10,
                ok: true,
            },
            100,
        );
        let before = line.render();
        assert!(before.contains("1/2"), "{before}");
        line.on_event(ProgressEvent::PlanStart, 200);
        line.on_event(
            ProgressEvent::ListPage {
                page: 2,
                keys_so_far: 2000,
            },
            200,
        );
        line.on_event(ProgressEvent::PlanEnd, 200);
        assert_eq!(
            line.render(),
            before,
            "plan-phase events must not disturb an active executor frame"
        );
    }

    // I42-line (W329): `PlanEnd` keeps the last frame state (the renderer
    // prints it with a trailing newline, I42-finalize); foreign executor
    // events never mutate `PlanProgressLine`.
    #[test]
    fn plan_line_plan_end_keeps_frame_and_ignores_executor_events() {
        let mut line = PlanProgressLine::new();
        line.on_event(ProgressEvent::ListPage {
            page: 1,
            keys_so_far: 1000,
        });
        line.on_event(ProgressEvent::PlanEnd);
        assert_eq!(
            line.render(),
            "Listing     page 1  1000 keys",
            "PlanEnd keeps the last frame state"
        );

        // foreign executor events do not mutate the plan line
        line.on_event(ProgressEvent::PassStart {
            kind: PassKind::Upload,
            total_keys: 3,
            total_bytes: 60,
        });
        line.on_event(ProgressEvent::KeyDone {
            kind: PassKind::Upload,
            key: "a.md".to_string(),
            bytes: 20,
            ok: true,
        });
        line.on_event(ProgressEvent::PassEnd {
            kind: PassKind::Upload,
        });
        line.on_event(ProgressEvent::RunEnd {
            executed: 1,
            failed: 0,
        });
        assert_eq!(
            line.render(),
            "Listing     page 1  1000 keys",
            "executor events must not mutate the plan line"
        );

        // a fresh line stays empty under only executor events
        let mut fresh = PlanProgressLine::new();
        fresh.on_event(ProgressEvent::PassStart {
            kind: PassKind::Upload,
            total_keys: 1,
            total_bytes: 1,
        });
        assert_eq!(fresh.render(), "");
    }

    // I42-line (W328): `HeadsStart` arms heading mode with a 0/total frame;
    // `HeadDone` advances the bar/percent; 100% fills the bar; a zero-total
    // heading renders nothing (mirrors the executor zero-total policy).
    #[test]
    fn plan_line_heading_frames() {
        let mut line = PlanProgressLine::new();
        line.on_event(ProgressEvent::HeadsStart { total_keys: 100 });
        assert_eq!(
            line.render(),
            "Heading     0/100  [--------]    0%",
            "HeadsStart arms the 0/total frame"
        );

        line.on_event(ProgressEvent::HeadDone {
            done: 40,
            total_keys: 100,
        });
        assert_eq!(
            line.render(),
            "Heading     40/100  [===>----]   40%",
            "40/100 renders the bar + percent"
        );

        line.on_event(ProgressEvent::HeadDone {
            done: 100,
            total_keys: 100,
        });
        assert_eq!(
            line.render(),
            "Heading     100/100  [========]  100%",
            "100% fills the bar"
        );

        // zero-total heading renders empty (W328, executor zero-total policy)
        let mut zero = PlanProgressLine::new();
        zero.on_event(ProgressEvent::HeadsStart { total_keys: 0 });
        assert_eq!(zero.render(), "");
        zero.on_event(ProgressEvent::HeadDone {
            done: 0,
            total_keys: 0,
        });
        assert_eq!(zero.render(), "");
    }

    // I42-line (W327): the plan-phase line machine renders cumulative
    // listing frames. Empty until the first `ListPage` (PlanStart alone stays
    // blank, W327 lock); each `ListPage` updates the frame in place; no byte
    // rate / ETA substrings on the plan phase (I42-line).
    #[test]
    fn plan_line_listing_frames_are_cumulative() {
        let mut line = PlanProgressLine::new();
        assert_eq!(line.render(), "", "empty line machine renders empty");

        line.on_event(ProgressEvent::PlanStart);
        assert_eq!(
            line.render(),
            "",
            "PlanStart alone stays blank until the first ListPage (W327 lock)"
        );

        line.on_event(ProgressEvent::ListPage {
            page: 1,
            keys_so_far: 1000,
        });
        assert_eq!(line.render(), "Listing     page 1  1000 keys");

        line.on_event(ProgressEvent::ListPage {
            page: 2,
            keys_so_far: 2000,
        });
        assert_eq!(line.render(), "Listing     page 2  2000 keys");

        let r = line.render();
        assert!(!r.contains("B/s"), "no rate on plan phase: {r}");
        assert!(!r.contains("ETA"), "no ETA on plan phase: {r}");
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

    // PR 28 r2 F3: pin the hour ETA formatting. `format_eta` renders `m:ss`
    // under an hour and `h:mm:ss` from an hour up (PR 28 r1 F2).
    // Characterization pin: GREEN on arrival; mutation-check the hour branch.
    #[test]
    fn format_eta_renders_hours_as_h_mm_ss() {
        assert_eq!(format_eta(3600.0), "1:00:00");
        assert_eq!(format_eta(3661.0), "1:01:01");
        // just below an hour stays `m:ss` (the m:ss -> h:mm:ss transition)
        assert_eq!(format_eta(3599.4), "59:59");
    }

    // I42-finalize (W369, H1 RED): `finish_plan` called through `Box<dyn
    // Progress>` - the exact dispatch boundary the CLI uses - must finalize a
    // partial plan bar (no `PlanEnd`) with a newline. Today the trait default
    // no-op leaves only `\r` refreshes with no trailing newline, so the
    // finalize assertions fail (RED). This is characterization-GREEN on the
    // W370 trait override.
    #[test]
    fn finish_plan_via_dyn_progress_finalizes_partial_bar() {
        let mut buf = Vec::new();
        {
            let boxed: Box<dyn Progress> = Box::new(TermProgress::new(&mut buf));
            boxed.event(ProgressEvent::PlanStart);
            boxed.event(ProgressEvent::ListPage {
                page: 1,
                keys_so_far: 1000,
            });
            boxed.event(ProgressEvent::ListPage {
                page: 2,
                keys_so_far: 2000,
            });
            // belt-and-braces finalize with NO PlanEnd (mid-cold failure)
            boxed.finish_plan();
            // second finalize must be idempotent (no extra newline)
            boxed.finish_plan();
        }
        let s = String::from_utf8(buf).unwrap();
        assert!(s.contains("Listing"), "last frame state: {s:?}");
        assert!(s.contains("2000 keys"), "latest frame content: {s:?}");
        assert!(
            s.ends_with("\x1b[K\n"),
            "finalize must clear then newline: {s:?}"
        );
        assert_eq!(
            s.matches('\r').count(),
            3,
            "two refreshes + one finalize frame: {s:?}"
        );
        assert_eq!(
            s.matches('\n').count(),
            1,
            "exactly one finalize newline (idempotent): {s:?}"
        );
    }

    // I42-finalize (W369 characterization): after a success-path `PlanEnd`
    // (which writes its own newline), a subsequent belt-and-braces
    // `finish_plan` is a no-op - still a single plan-line newline.
    #[test]
    fn plan_end_then_finish_plan_is_idempotent() {
        let mut buf = Vec::new();
        {
            let boxed: Box<dyn Progress> = Box::new(TermProgress::new(&mut buf));
            boxed.event(ProgressEvent::PlanStart);
            boxed.event(ProgressEvent::ListPage {
                page: 1,
                keys_so_far: 1000,
            });
            boxed.event(ProgressEvent::PlanEnd);
            boxed.finish_plan(); // belt-and-braces after success: no-op
        }
        let s = String::from_utf8(buf).unwrap();
        assert_eq!(s.matches('\n').count(), 1, "single plan newline: {s:?}");
        assert!(s.ends_with("\x1b[K\n"), "{s:?}");
    }

    // I42-finalize (L1 fold): `PlanEnd` with an empty render (PlanStart only,
    // no ListPage/HeadsStart) must still mark the plan finalized, so a later
    // belt-and-braces `finish_plan` writes nothing extra rather than racing a
    // future non-empty state.
    #[test]
    fn finish_plan_after_empty_plan_end_writes_nothing() {
        let mut buf = Vec::new();
        {
            let boxed: Box<dyn Progress> = Box::new(TermProgress::new(&mut buf));
            boxed.event(ProgressEvent::PlanStart);
            boxed.event(ProgressEvent::PlanEnd);
            boxed.finish_plan();
        }
        let s = String::from_utf8(buf).unwrap();
        assert_eq!(s, "", "empty plan must render nothing: {s:?}");
    }
}
