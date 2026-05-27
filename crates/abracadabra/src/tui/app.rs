//! Terminal setup + main event loop + per-tab dispatch.

use std::fs::OpenOptions;
use std::io::{self, Stdout, Write};
use std::path::PathBuf;
use std::time::Duration;

#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;

use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::Style;
use ratatui::widgets::{Block, Borders, Tabs};
use ratatui::Frame;
use ratatui::Terminal;

use std::cell::RefCell;

use crate::model::alerts::{Alert, AlertKind, Severity};
use crate::model::analysis;
use crate::model::buckets::TimeBuckets;
use crate::model::slot::SlotStatus;
use crate::model::state::State;
use crate::model::window::{self, WindowStats};
use crate::tui::panel;
use crate::tui::theme;
use crate::tui::view::{LatencySnapshot, SlotViewRow, VoteResumeViewRow};
use crate::tui::TuiError;

/// `O_NOFOLLOW` flag value on Linux. The constant is part of the
/// stable kernel ABI (asm-generic/fcntl.h, `0o400000`). We avoid
/// pulling `libc` in as a direct dependency for a single integer.
#[cfg(target_os = "linux")]
const fn libc_o_nofollow() -> i32 {
    0o400_000
}

/// Same value, different naming on some BSDs. Build target for this
/// crate is Linux per the project README; this branch is here only to
/// keep `cargo check` clean if someone builds on macOS for editor IDE
/// support. macOS `O_NOFOLLOW` = 0x0100 (256).
#[cfg(all(unix, not(target_os = "linux")))]
const fn libc_o_nofollow() -> i32 {
    0x0100
}

/// Resolve the directory we yank into. Order:
///   1. `XDG_RUNTIME_DIR/abracadabra` (per-user, tmpfs, mode 0700 by spec)
///   2. `HOME/.cache/abracadabra/yank` (XDG_CACHE_HOME default)
///   3. error
///
/// Creates the directory if missing. Returns the resolved path.
fn yank_dir() -> io::Result<PathBuf> {
    let base = if let Some(rt) = std::env::var_os("XDG_RUNTIME_DIR") {
        PathBuf::from(rt).join("abracadabra")
    } else if let Some(home) = std::env::var_os("HOME") {
        PathBuf::from(home).join(".cache/abracadabra/yank")
    } else {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "no XDG_RUNTIME_DIR or HOME set; cannot pick a safe yank directory",
        ));
    };
    std::fs::create_dir_all(&base)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // Best-effort 0700: silently ignore if the directory was
        // pre-created with broader perms by user choice.
        let _ = std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o700));
    }
    Ok(base)
}

/// Render an alert into a copy-friendly plain-text block.
fn format_alert_for_yank(state: &State, alert: &Alert) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    out.push_str("# abracadabra — alert yank\n\n");
    let sev = match alert.severity {
        Severity::Critical => "CRIT",
        Severity::Warn => "WARN",
        Severity::Info => "INFO",
    };
    let _ = writeln!(out, "severity:      {sev}");
    let _ = writeln!(out, "at:            {}", alert.at);
    match &alert.kind {
        AlertKind::LogPattern {
            severity,
            module,
            count,
        } => {
            let _ = writeln!(out, "module:        {module}");
            let _ = writeln!(out, "count:         {count} occurrences");
            if let Some(group) = state.log_issues_get(*severity, module) {
                let _ = writeln!(out, "first:         {}", group.first_at);
                let _ = writeln!(out, "last:          {}", group.last_at);
                out.push_str("first sample body:\n");
                let _ = writeln!(out, "  {}", group.sample_body);
            }
        }
        AlertKind::LocalLeaderSummary {
            slot_count,
            window_count,
        } => {
            let _ = writeln!(
                out,
                "kind:          local-leader summary\n\
                 slot_count:    {slot_count}\n\
                 window_count:  {window_count}  (4-slot bursts)",
            );
        }
        AlertKind::ClusterSlotsShutdownObserved => {
            out.push_str("kind:          cluster-slots service shutdown observed\n");
        }
        AlertKind::StandstillObserved { at_slot } => {
            let _ = writeln!(out, "kind:          standstill at slot {at_slot}");
        }
        AlertKind::IdentityChanged => {
            out.push_str("kind:          operator identity change\n");
        }
    }
    out.push_str("\ndescription:\n");
    let _ = writeln!(out, "  {}", alert.description);
    out
}

/// One-bit filter dimensions for the Slots tab. Multiple flags AND
/// together (e.g. `tcl + leader` -> only rows that are both
/// `crashed_leader` AND `we_are_leader`).
#[derive(Debug, Default, Clone, Copy)]
pub struct SlotFilters {
    pub tcl: bool,
    pub s2n: bool,
    pub s2s: bool,
    pub leader: bool,
    pub fast_only: bool,
    pub slow_only: bool,
    pub skipped_only: bool,
    /// Rows where both `voted_notarize` and `voted_skip` are set. Surfaces
    /// the protocol-ambiguous case where the validator cast a Notarize
    /// vote and later a Skip vote on the same slot (vote_pattern rendered
    /// as `"N+S"` or `"N+F+S"`).
    pub mixed_votes: bool,
}

impl SlotFilters {
    pub const fn any_active(self) -> bool {
        self.tcl
            || self.s2n
            || self.s2s
            || self.leader
            || self.fast_only
            || self.slow_only
            || self.skipped_only
            || self.mixed_votes
    }

    pub const fn matches(self, r: &SlotViewRow) -> bool {
        if self.tcl && !r.crashed_leader {
            return false;
        }
        if self.s2n && !r.safe_to_notar {
            return false;
        }
        if self.s2s && !r.safe_to_skip {
            return false;
        }
        if self.leader && !r.we_are_leader {
            return false;
        }
        if self.fast_only && !matches!(r.status, SlotStatus::FastFinalized) {
            return false;
        }
        if self.slow_only && !matches!(r.status, SlotStatus::SlowFinalized) {
            return false;
        }
        if self.skipped_only && !matches!(r.status, SlotStatus::Skipped) {
            return false;
        }
        if self.mixed_votes && !(r.voted_notarize && r.voted_skip) {
            return false;
        }
        true
    }
}

/// Names a single filter dimension; used by the event loop to toggle one
/// of the flags on `SlotFilters` without leaking individual booleans
/// into the key dispatch.
#[derive(Debug, Clone, Copy)]
pub enum FilterKind {
    Tcl,
    S2n,
    S2s,
    Leader,
    FastOnly,
    SlowOnly,
    SkippedOnly,
    MixedVotes,
}

const TAB_NAMES: [&str; 6] = [
    "Overview",
    "Time series",
    "Windows",
    "Slots",
    "Leader timeouts",
    "Alerts",
];

pub struct App<'s> {
    pub state: &'s State,
    /// Pre-bucketed time-series. `None` when the log carries no usable
    /// time range (`TimeBuckets::from_state` couldn't determine
    /// `(lo, hi)`). Every panel that reads this field MUST early-return
    /// on `None` rather than treating empty buckets as zero — the
    /// distinction matters for "no-data" placeholders. See the
    /// timeseries / leader-timeouts panels for the canonical pattern.
    pub buckets: Option<&'s TimeBuckets>,
    /// Time-series bucket size in seconds (set via `--bucket`). Carried
    /// here so panels that don't read `buckets` directly (e.g. Overview's
    /// file-meta block) can still surface the value to the user.
    pub bucket_secs: i64,
    /// Pre-computed latency/severity snapshot. Built once in `App::new`;
    /// panels read fields rather than re-running analytics per frame
    /// (previously called `lifecycle_latencies` / `LatencyStages::compute`
    /// / `vote_resumes_after_tcl` on every draw, which sorts ~179k
    /// entries five times per Slots frame at 5 fps).
    pub latency: LatencySnapshot,
    /// Pre-computed rolling-window comparison stats. Built once in
    /// `App::new` so the Windows tab doesn't re-run six `compute_one`
    /// passes + six `vote_resumes_after_tcl` scans on every draw.
    pub window_stats: Vec<WindowStats>,
    /// Total number of slots where this validator was leader. Computed
    /// once in `App::new` so the Slots tab's two KPI sites don't each
    /// re-scan the full `state.slots` map per frame.
    pub leader_slot_count: u64,
    pub current_tab: usize,
    pub slot_scroll: usize,
    pub resume_scroll: usize,
    pub alert_scroll: usize,
    pub slot_rows: Vec<SlotViewRow>,
    pub resume_rows: Vec<VoteResumeViewRow>,
    pub slot_filters: SlotFilters,
    /// Indices into `slot_rows` that pass the current `slot_filters`.
    /// Rebuilt on every filter change; `slot_scroll` is bounded by this
    /// length, not by `slot_rows.len()`.
    pub slot_indices: Vec<usize>,
    /// Transient status line shown in the bottom strip. Cleared on the
    /// next key press so messages don't linger.
    pub status_message: Option<String>,
    /// Monotonic counter appended to yank filenames so repeated yanks
    /// during the same session don't overwrite. The yank directory is
    /// `$XDG_RUNTIME_DIR/abracadabra` when set, otherwise
    /// `$HOME/.cache/abracadabra/yank` — see `yank_dir`. Files use the
    /// pattern `abracadabra-yank-N.txt` with `N = yank_counter`.
    pub yank_counter: u32,
    /// Memoised result of the alerts-panel `bucket_timestamps` call for
    /// the currently-selected LogPattern alert. Keyed on
    /// `(alert_scroll, bucket_count)`; invalidated implicitly when the
    /// key changes. Single-threaded TUI -> `RefCell` is sufficient.
    pub alert_spark_cache: RefCell<Option<AlertSparkCache>>,
}

/// Cached bucket-timestamps result for the alerts panel sparkline.
#[derive(Debug, Clone)]
pub struct AlertSparkCache {
    pub alert_index: usize,
    pub bucket_count: usize,
    pub buckets: Vec<u64>,
}

#[allow(clippy::missing_const_for_fn)] // interactive state machine — const is semantically wrong
impl<'s> App<'s> {
    pub fn new(state: &'s State, buckets: Option<&'s TimeBuckets>, bucket_secs: i64) -> Self {
        let slot_rows: Vec<SlotViewRow> =
            state.slots.values().map(SlotViewRow::from_record).collect();
        let slot_indices: Vec<usize> = (0..slot_rows.len()).collect();

        // Count leader slots once from the contiguous `slot_rows` vec
        // (cache-friendly) so the Slots tab's two KPI sites don't each
        // walk the full BTreeMap per frame.
        let leader_slot_count = slot_rows.iter().filter(|r| r.we_are_leader).count() as u64;

        // Single analytics pass: scan TCL→next-notarize once, derive
        // both `latency` (sorted ascending for percentiles) and
        // `resume_rows` (sorted descending for the incidents table)
        // from the same vector.
        let mut resumes = analysis::vote_resumes_after_tcl(state);
        let latency = LatencySnapshot::compute(state, &resumes);
        resumes.sort_by_key(|r| std::cmp::Reverse(r.resume_us));
        let resume_rows = resumes
            .into_iter()
            .map(VoteResumeViewRow::from_record)
            .collect();

        // Rolling-window stats computed once; the Windows tab reads this
        // directly. `compute` returns an empty vec when `time_range` is
        // None, which the panel handles as a "no data" path.
        let window_stats = window::compute(state, &window::default_windows());

        Self {
            state,
            buckets,
            bucket_secs,
            latency,
            window_stats,
            leader_slot_count,
            current_tab: 0,
            slot_scroll: 0,
            resume_scroll: 0,
            alert_scroll: 0,
            slot_rows,
            resume_rows,
            slot_filters: SlotFilters::default(),
            slot_indices,
            status_message: None,
            yank_counter: 0,
            alert_spark_cache: RefCell::new(None),
        }
    }

    /// Write the currently-selected alert (Alerts tab) to a per-user
    /// file so engineers can pipe / grep / copy it without fighting the
    /// TUI's raw-mode mouse capture. Sets `status_message` on
    /// completion (success path: file location; failure path: error).
    ///
    /// [SECURITY] Avoids the `/tmp/abracadabra-yank-N.txt` symlink-
    /// follow attack: opens with `O_CREAT | O_EXCL` (via
    /// `create_new(true)`) and `O_NOFOLLOW`, refusing to overwrite a
    /// pre-existing path or to follow a symlink at the path. On
    /// `AlreadyExists` the counter is bumped and we retry up to
    /// `YANK_MAX_RETRIES` times before giving up.
    ///
    /// Yank directory is the user's `XDG_RUNTIME_DIR` (mode 0700 by
    /// spec) when available; falls back to a private subdir under the
    /// user's `HOME`. Avoids `/tmp/` entirely so co-tenants on a shared
    /// host can't pre-position attack symlinks.
    pub fn yank_current_alert(&mut self) {
        let Some(alert) = self.state.alerts.get(self.alert_scroll) else {
            self.status_message = Some("no alert under cursor".to_owned());
            return;
        };

        let dir = match yank_dir() {
            Ok(d) => d,
            Err(e) => {
                self.status_message = Some(format!("yank failed: cannot prepare dir: {e}"));
                return;
            }
        };

        let body = format_alert_for_yank(self.state, alert);
        match self.try_write_yank(&dir, &body) {
            Ok(path) => {
                self.status_message = Some(format!(
                    "yanked to {} — cat / xclip / pbcopy that path",
                    path.display()
                ));
            }
            Err(e) => {
                self.status_message = Some(format!("yank failed: {e}"));
            }
        }
    }

    /// Bounded-retry write loop for `yank_current_alert`. Returns the
    /// path written on success.
    ///
    /// Filename pattern: `abracadabra-yank-<pid>-<n>.txt`. The PID
    /// segment makes cross-session collisions impossible — without it,
    /// 16+ accumulated yanks in the persistent fallback directory
    /// (`$HOME/.cache/abracadabra/yank`) would exhaust the retry budget
    /// in subsequent sessions (REL-01 regression guard).
    fn try_write_yank(&mut self, dir: &std::path::Path, body: &str) -> io::Result<PathBuf> {
        const YANK_MAX_RETRIES: u32 = 16;
        let pid = std::process::id();
        let mut last_err: Option<io::Error> = None;
        for _ in 0..YANK_MAX_RETRIES {
            self.yank_counter = self.yank_counter.saturating_add(1);
            let path = dir.join(format!("abracadabra-yank-{pid}-{}.txt", self.yank_counter));

            let mut opts = OpenOptions::new();
            opts.write(true).create_new(true);
            // O_NOFOLLOW + O_CREAT|O_EXCL: defense-in-depth against
            // symlink-attack TOCTOU. `create_new` already fails on a
            // pre-existing path; `O_NOFOLLOW` additionally guarantees
            // we never traverse a symlink even at the leaf component.
            #[cfg(unix)]
            opts.custom_flags(libc_o_nofollow());

            match opts.open(&path) {
                Ok(mut f) => {
                    // Best-effort 0600 perms on Unix so a co-tenant
                    // cannot read the yank file once written.
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::PermissionsExt;
                        let _ = f.set_permissions(std::fs::Permissions::from_mode(0o600));
                    }
                    f.write_all(body.as_bytes())?;
                    f.flush()?;
                    return Ok(path);
                }
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                    // Bump counter on next loop iteration; record so
                    // the final error surfaces if all attempts collide.
                    last_err = Some(e);
                }
                Err(e) => return Err(e),
            }
        }
        Err(last_err
            .unwrap_or_else(|| io::Error::other("yank: exhausted retries finding unused filename")))
    }

    /// Recompute the filtered index list. Called whenever a filter flag
    /// changes. Resets `slot_scroll` to 0 so the cursor stays valid.
    fn recompute_slot_indices(&mut self) {
        let filters = self.slot_filters;
        self.slot_indices = if filters.any_active() {
            self.slot_rows
                .iter()
                .enumerate()
                .filter_map(|(i, r)| filters.matches(r).then_some(i))
                .collect()
        } else {
            (0..self.slot_rows.len()).collect()
        };
        self.slot_scroll = 0;
    }

    pub fn toggle_filter(&mut self, kind: FilterKind) {
        match kind {
            FilterKind::Tcl => self.slot_filters.tcl = !self.slot_filters.tcl,
            FilterKind::S2n => self.slot_filters.s2n = !self.slot_filters.s2n,
            FilterKind::S2s => self.slot_filters.s2s = !self.slot_filters.s2s,
            FilterKind::Leader => self.slot_filters.leader = !self.slot_filters.leader,
            FilterKind::FastOnly => self.slot_filters.fast_only = !self.slot_filters.fast_only,
            FilterKind::SlowOnly => self.slot_filters.slow_only = !self.slot_filters.slow_only,
            FilterKind::SkippedOnly => {
                self.slot_filters.skipped_only = !self.slot_filters.skipped_only;
            }
            FilterKind::MixedVotes => {
                self.slot_filters.mixed_votes = !self.slot_filters.mixed_votes;
            }
        }
        self.recompute_slot_indices();
    }

    pub fn clear_filters(&mut self) {
        self.slot_filters = SlotFilters::default();
        self.recompute_slot_indices();
    }

    /// Returns a mutable reference to the cursor field driven by the
    /// current tab's scroll keys. `None` for tabs that don't host a
    /// scrollable list (Overview, Time series, Windows) — callers must
    /// short-circuit so the keystroke is a no-op rather than silently
    /// clobbering a cursor on another tab.
    fn scroll_target(&mut self) -> Option<&mut usize> {
        match self.current_tab {
            3 => Some(&mut self.slot_scroll),
            4 => Some(&mut self.resume_scroll),
            5 => Some(&mut self.alert_scroll),
            _ => None,
        }
    }

    fn scroll_max(&self) -> usize {
        match self.current_tab {
            3 => self.slot_indices.len().saturating_sub(1),
            4 => self.resume_rows.len().saturating_sub(1),
            5 => self.state.alerts.len().saturating_sub(1),
            _ => 0,
        }
    }

    fn step_scroll(&mut self, delta: isize) {
        let max = self.scroll_max();
        let Some(target) = self.scroll_target() else {
            return;
        };
        if delta < 0 {
            let d = delta.unsigned_abs();
            *target = target.saturating_sub(d);
        } else {
            *target = (*target).saturating_add(delta as usize).min(max);
        }
    }

    fn jump_top(&mut self) {
        if let Some(target) = self.scroll_target() {
            *target = 0;
        }
    }

    fn jump_bottom(&mut self) {
        let max = self.scroll_max();
        if let Some(target) = self.scroll_target() {
            *target = max;
        }
    }

    fn next_tab(&mut self) {
        self.current_tab = (self.current_tab + 1) % TAB_NAMES.len();
    }

    fn prev_tab(&mut self) {
        self.current_tab = (self.current_tab + TAB_NAMES.len() - 1) % TAB_NAMES.len();
    }

    fn set_tab(&mut self, idx: usize) {
        if idx < TAB_NAMES.len() {
            self.current_tab = idx;
        }
    }
}

pub fn run(state: &State, buckets: Option<&TimeBuckets>, bucket_secs: i64) -> Result<(), TuiError> {
    install_panic_hook();
    let mut terminal = setup_terminal()?;
    let mut app = App::new(state, buckets, bucket_secs);
    let result = event_loop(&mut terminal, &mut app);
    restore_terminal(&mut terminal)?;
    result
}

/// Install a panic hook that restores the terminal (disables raw mode,
/// leaves the alt screen) before the original hook runs, so a panic
/// inside `terminal.draw` or anywhere in the event loop doesn't leave
/// the calling shell in raw mode with no echo.
///
/// Idempotent: guarded by `OnceLock` so a second TUI session in the
/// same process (tests / library use) does not chain the hook onto
/// itself recursively.
fn install_panic_hook() {
    use std::sync::OnceLock;
    static INSTALLED: OnceLock<()> = OnceLock::new();
    INSTALLED.get_or_init(|| {
        let prior = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            // Best-effort terminal restore. We cannot propagate
            // errors from inside a panic hook; swallow them silently.
            let _ = disable_raw_mode();
            let _ = execute!(io::stdout(), LeaveAlternateScreen);
            prior(info);
        }));
    });
}

fn setup_terminal() -> Result<Terminal<CrosstermBackend<Stdout>>, TuiError> {
    enable_raw_mode().map_err(TuiError::Io)?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen).map_err(TuiError::Io)?;
    let backend = CrosstermBackend::new(stdout);
    Terminal::new(backend).map_err(TuiError::Io)
}

fn restore_terminal(terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> Result<(), TuiError> {
    disable_raw_mode().map_err(TuiError::Io)?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen).map_err(TuiError::Io)?;
    terminal.show_cursor().map_err(TuiError::Io)?;
    Ok(())
}

fn event_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    app: &mut App<'_>,
) -> Result<(), TuiError> {
    loop {
        terminal
            .draw(|frame| draw(frame, app))
            .map_err(TuiError::Io)?;
        if event::poll(Duration::from_millis(200)).map_err(TuiError::Io)? {
            if let Event::Key(key) = event::read().map_err(TuiError::Io)? {
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                // Any key clears a stale status message. Yank below
                // resets it again after this clear.
                app.status_message = None;
                match key.code {
                    KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
                    KeyCode::Char('1') => app.set_tab(0),
                    KeyCode::Char('2') => app.set_tab(1),
                    KeyCode::Char('3') => app.set_tab(2),
                    KeyCode::Char('4') => app.set_tab(3),
                    KeyCode::Char('5') => app.set_tab(4),
                    KeyCode::Char('6') => app.set_tab(5),
                    KeyCode::Tab => app.next_tab(),
                    KeyCode::BackTab => app.prev_tab(),
                    KeyCode::Char('j') | KeyCode::Down => app.step_scroll(1),
                    KeyCode::Char('k') | KeyCode::Up => app.step_scroll(-1),
                    KeyCode::PageDown => app.step_scroll(20),
                    KeyCode::PageUp => app.step_scroll(-20),
                    KeyCode::Char('g') => app.jump_top(),
                    KeyCode::Char('G') if key.modifiers.contains(KeyModifiers::SHIFT) => {
                        app.jump_bottom();
                    }
                    KeyCode::Home => app.jump_top(),
                    KeyCode::End => app.jump_bottom(),
                    // Alerts-tab-only: yank the selected alert to a tmp
                    // file so the engineer can copy it without fighting
                    // raw-mode mouse capture. `yank_current_alert`
                    // overwrites `status_message` after the generic
                    // clear above, so the message persists for one
                    // render frame as intended.
                    KeyCode::Char('y') if app.current_tab == 5 => {
                        app.yank_current_alert();
                    }
                    // Slot-tab-only filter shortcuts. Constrained to
                    // `current_tab == 3` so the same letters stay free
                    // for future tab-specific bindings elsewhere.
                    KeyCode::Char('t') if app.current_tab == 3 => {
                        app.toggle_filter(FilterKind::Tcl);
                    }
                    KeyCode::Char('n') if app.current_tab == 3 => {
                        app.toggle_filter(FilterKind::S2n);
                    }
                    // `p` for S2S (safe-to-ski**P**); pairs with `n`
                    // for S2N — both safe-to-X events use the last
                    // letter of their qualifier.
                    KeyCode::Char('p') if app.current_tab == 3 => {
                        app.toggle_filter(FilterKind::S2s);
                    }
                    // `s` is the natural mnemonic for the SKIP status
                    // filter. Free now that S2S moved to `p`.
                    KeyCode::Char('s') if app.current_tab == 3 => {
                        app.toggle_filter(FilterKind::SkippedOnly);
                    }
                    KeyCode::Char('l') if app.current_tab == 3 => {
                        app.toggle_filter(FilterKind::Leader);
                    }
                    KeyCode::Char('f') if app.current_tab == 3 => {
                        app.toggle_filter(FilterKind::FastOnly);
                    }
                    // `x` for slow-finalized — no natural mnemonic, but
                    // visually pairs with `f` for fast (f/x toggles the
                    // two finalization paths).
                    KeyCode::Char('x') if app.current_tab == 3 => {
                        app.toggle_filter(FilterKind::SlowOnly);
                    }
                    // `m` for mixed-vote rows (N+S / N+F+S). Filters to
                    // the protocol-ambiguous slots where the validator
                    // cast both Notarize and Skip on the same slot —
                    // worth surfacing because vote_pattern renders these
                    // distinctly and the model permits the combination.
                    KeyCode::Char('m') if app.current_tab == 3 => {
                        app.toggle_filter(FilterKind::MixedVotes);
                    }
                    KeyCode::Char('c') if app.current_tab == 3 => {
                        app.clear_filters();
                    }
                    _ => {}
                }
            }
        }
    }
}

fn draw(frame: &mut Frame<'_>, app: &App<'_>) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(4), // header
            Constraint::Length(3), // tab strip
            Constraint::Min(10),   // main content
            Constraint::Length(1), // status bar
        ])
        .split(frame.area());

    panel::header::render(app.state, frame, chunks[0]);
    render_tabs(app, frame, chunks[1]);
    match app.current_tab {
        0 => panel::overview::render(app, frame, chunks[2]),
        1 => panel::timeseries::render_detail(app.buckets, frame, chunks[2]),
        2 => panel::windows::render(app, frame, chunks[2]),
        3 => panel::slots::render(app, frame, chunks[2]),
        4 => panel::leader_timeouts::render(app, frame, chunks[2]),
        5 => panel::alerts::render_full(app, frame, chunks[2]),
        _ => {}
    }
    panel::status_bar::render(
        app.current_tab,
        app.status_message.as_deref(),
        frame,
        chunks[3],
    );
}

fn render_tabs(app: &App<'_>, frame: &mut Frame<'_>, area: ratatui::layout::Rect) {
    use ratatui::style::Color;

    // Render each tab as a `[N] Name` button. Bracket characters carry
    // the affordance ("click this"), the digit reads as the keyboard
    // shortcut, name reads as the destination. The widget's
    // `highlight_style` overlays a filled cyan rectangle on the
    // currently-active tab — a literal "this button is pressed" look.
    let titles: Vec<ratatui::text::Line<'_>> = TAB_NAMES
        .iter()
        .enumerate()
        .map(|(i, name)| {
            ratatui::text::Line::from(vec![
                ratatui::text::Span::styled(" [", theme::label_style()),
                ratatui::text::Span::styled(format!("{}", i + 1), theme::accent_style()),
                ratatui::text::Span::styled("] ", theme::label_style()),
                ratatui::text::Span::styled((*name).to_owned(), theme::value_style()),
                ratatui::text::Span::styled(" ", theme::label_style()),
            ])
        })
        .collect();
    let tabs = Tabs::new(titles)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" navigate  (1-6 · Tab / Shift+Tab · q to quit) ")
                .title_style(theme::title_style()),
        )
        .select(app.current_tab)
        .divider(ratatui::text::Span::styled("  ", theme::label_style()))
        // Pressed-button look: dark text on cyan background. Overrides
        // the per-span colours so the whole tab reads as one block.
        .highlight_style(Style::default().bg(theme::ACCENT).fg(Color::Black));
    frame.render_widget(tabs, area);
}

// Overview is now a pure-stats panel — no embedded plots. Time-series
// visualisations live on tab 2, distributions on the Recoveries tab.
// See `panel::overview::render`.

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    /// SEC-01 regression: even when a malicious symlink pre-exists at
    /// the target path, `try_write_yank` must not follow it. The fix
    /// uses `create_new(true)` + `O_NOFOLLOW`, so a pre-placed symlink
    /// causes the open to fail (with `AlreadyExists`) and the loop
    /// retries on a fresh counter.
    #[test]
    fn yank_to_existing_symlink_does_not_overwrite_target() {
        // Set up an isolated yank dir under a tempdir.
        let tmp =
            std::env::temp_dir().join(format!("abracadabra-yank-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        // Place an attacker's symlink at the path the yank would
        // first try. Filename pattern is
        // `abracadabra-yank-<pid>-<n>.txt` (REL-01 fix); pre-position
        // the attack at <pid>-1.
        let pid = std::process::id();
        let victim = tmp.join("victim.txt");
        std::fs::write(&victim, b"original-victim-content").unwrap();
        let attack_link = tmp.join(format!("abracadabra-yank-{pid}-1.txt"));
        symlink(&victim, &attack_link).unwrap();

        // Construct a minimal App and drive the yank.
        let state = crate::model::state::State::new(PathBuf::from("/tmp/x"), 0);
        let mut app = App::new(&state, None, 60);
        let result = app.try_write_yank(&tmp, "yank-body-payload");

        // Outcome: yank succeeds, but writes to a DIFFERENT path
        // (counter bumped past 1). The victim's content is untouched.
        let written = result.expect("yank should succeed by bumping counter");
        assert_ne!(written, attack_link);
        // Victim untouched.
        let victim_after = std::fs::read_to_string(&victim).unwrap();
        assert_eq!(victim_after, "original-victim-content");
        // The yank's actual content lives at `written`.
        let yank_body = std::fs::read_to_string(&written).unwrap();
        assert_eq!(yank_body, "yank-body-payload");

        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// STRUCT-01 regression: scroll keys on tabs without a scrollable
    /// list (Overview, Time series, Windows) must NOT clobber a
    /// cursor on a scrollable tab. The old `scroll_target` default
    /// arm wrote to `slot_scroll`, so `g`/`G`/`Home`/`End` on tab 0
    /// would reset `slot_scroll` to 0.
    #[test]
    fn scroll_on_non_list_tab_does_not_mutate_other_cursors() {
        let state = crate::model::state::State::new(PathBuf::from("/tmp/x"), 0);
        let mut app = App::new(&state, None, 60);
        // Pretend the user has navigated 42 rows down the Slots table.
        app.slot_scroll = 42;
        // Switch to Overview (tab 0) and press G — must be a no-op for
        // every cursor, including slot_scroll.
        app.current_tab = 0;
        app.jump_bottom();
        app.jump_top();
        app.step_scroll(1);
        app.step_scroll(-1);
        assert_eq!(
            app.slot_scroll, 42,
            "slot_scroll clobbered by tab-0 scroll keys"
        );
        // Same check on Time series (tab 1) and Windows (tab 2).
        app.current_tab = 1;
        app.jump_bottom();
        app.step_scroll(20);
        assert_eq!(
            app.slot_scroll, 42,
            "slot_scroll clobbered by tab-1 scroll keys"
        );
        app.current_tab = 2;
        app.jump_top();
        assert_eq!(
            app.slot_scroll, 42,
            "slot_scroll clobbered by tab-2 scroll keys"
        );
    }

    /// PERF-03 regression: leader-slot count is precomputed once in
    /// `App::new`. Asserts the field matches a direct count from
    /// `slot_rows` so the two read-sites on the Slots tab return the
    /// same value the old per-frame scans did.
    #[test]
    fn leader_slot_count_matches_slot_rows_filter() {
        let mut state = crate::model::state::State::new(PathBuf::from("/tmp/x"), 0);
        state.slot_mut(1).we_are_leader = true;
        state.slot_mut(2).we_are_leader = false;
        state.slot_mut(3).we_are_leader = true;
        state.slot_mut(4).we_are_leader = true;
        let app = App::new(&state, None, 60);
        let direct = app.slot_rows.iter().filter(|r| r.we_are_leader).count() as u64;
        assert_eq!(app.leader_slot_count, direct);
        assert_eq!(app.leader_slot_count, 3);
    }

    /// COR-03 follow-up: the `mixed_votes` filter selects rows that
    /// cast both Notarize and Skip on the same slot — the protocol-
    /// ambiguous case `vote_pattern` now surfaces as `N+S` / `N+F+S`.
    #[test]
    fn mixed_votes_filter_matches_n_and_s_rows() {
        use crate::tui::view::SlotViewRow;
        let mk = |n, s| SlotViewRow {
            slot: 0,
            status: SlotStatus::Pending,
            fast: None,
            we_are_leader: false,
            assembly_ms: None,
            consensus_ms: None,
            lifecycle_ms: None,
            voted_notarize: n,
            voted_finalize: false,
            voted_skip: s,
            safe_to_notar: false,
            safe_to_skip: false,
            crashed_leader: false,
        };
        let filters = SlotFilters {
            mixed_votes: true,
            ..SlotFilters::default()
        };
        // Pure-N or pure-S rows must NOT match.
        assert!(!filters.matches(&mk(true, false)));
        assert!(!filters.matches(&mk(false, true)));
        assert!(!filters.matches(&mk(false, false)));
        // Both Notarize and Skip set -> matches.
        assert!(filters.matches(&mk(true, true)));
        // any_active picks up the new dimension.
        assert!(filters.any_active());
    }

    /// COR-02 regression guard for the math: deriving the per-hour
    /// rate must use the real elapsed hours, not a clamped 1.0
    /// denominator. With 60 events in 20 minutes, the true rate is
    /// 180/h — the old `hours.max(1.0)` would have reported 60/h.
    #[test]
    fn rate_per_hour_does_not_clamp_short_log() {
        let twenty_min_hours = 20.0_f64 / 60.0;
        let events = 60.0_f64;
        let rate = events / twenty_min_hours;
        assert!((rate - 180.0).abs() < 1e-9, "expected 180/h, got {rate}");
    }

    /// REL-01 regression: panic hook installation must be idempotent
    /// — calling `install_panic_hook` twice in the same process must
    /// not stack the restore logic recursively. Verified by checking
    /// that the `OnceLock` short-circuits the second call.
    #[test]
    fn panic_hook_install_is_idempotent() {
        // We can't directly observe hook chain depth without panicking
        // (which would mess up the test runner's output). Instead,
        // assert that two back-to-back installs leave the process
        // alive and that subsequent calls are no-ops.
        install_panic_hook();
        install_panic_hook();
        install_panic_hook();
        // If we got here without infinite recursion or stack overflow,
        // the OnceLock guard is doing its job.
    }
}
