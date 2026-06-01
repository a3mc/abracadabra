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
        AlertKind::StandstillObserved {
            at_slot,
            count,
            last_at,
        } => {
            let _ = writeln!(out, "kind:          standstill at slot {at_slot}");
            let _ = writeln!(out, "firings:       {count}");
            let _ = writeln!(out, "first at:      {}", alert.at);
            let _ = writeln!(out, "last at:       {last_at}");
        }
        AlertKind::IdentityChanged => {
            out.push_str("kind:          operator identity change\n");
        }
    }
    out.push_str("\ndescription:\n");
    let _ = writeln!(out, "  {}", alert.description);
    out
}

/// One-bit filter dimensions for the Slots tab. Most flags AND
/// together (e.g. `tcl + leader` -> only rows that are both
/// `crashed_leader` AND `we_are_leader`). The skip-family pair
/// (`vskip_only`, `canonical_skip_only`) is the exception — they OR
/// together so `[v]+[c]` shows both buckets at once.
#[derive(Debug, Default, Clone, Copy)]
pub struct SlotFilters {
    pub tcl: bool,
    pub s2n: bool,
    pub s2s: bool,
    pub leader: bool,
    pub fast_only: bool,
    pub slow_only: bool,
    /// Rows where the slot is VSKIP (vote-skip with no canonical
    /// evidence — the indeterminate bucket). Toggled with `[v]`.
    pub vskip_only: bool,
    /// Rows where the slot is CSKIP (vote-skip on a canonical slot).
    /// Toggled with `[c]`. Headline operator-facing failure filter.
    pub canonical_skip_only: bool,
}

impl SlotFilters {
    pub const fn any_active(self) -> bool {
        self.tcl
            || self.s2n
            || self.s2s
            || self.leader
            || self.fast_only
            || self.slow_only
            || self.vskip_only
            || self.canonical_skip_only
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
        // Skip-family OR semantics: when either flag is on, the row
        // must match at least one of the requested buckets. Both off
        // means no skip filter (no constraint added).
        if self.vskip_only || self.canonical_skip_only {
            let is_cskip = r.skip_classification.is_canonical_skip();
            let is_vskip = matches!(r.status, SlotStatus::Skipped) && !is_cskip;
            let want_v = self.vskip_only && is_vskip;
            let want_c = self.canonical_skip_only && is_cskip;
            if !(want_v || want_c) {
                return false;
            }
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
    VskipOnly,
    CanonicalSkipOnly,
}

/// Logical identifier for each dashboard tab.
///
/// `current_tab: usize` is an index into `App::tabs: Vec<TabId>`. The
/// layout is decided at construction based on log activity: `Overview`
/// is always tab `1` so operator muscle memory survives the active /
/// static distinction. When the log is active, `Live` is appended at
/// the end (tab `7`); when static it is omitted entirely. All
/// conditional UI checks compare against `TabId` rather than raw
/// indices so they stay correct across both layouts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TabId {
    Live,
    Overview,
    TimeSeries,
    Windows,
    Slots,
    LeaderTimeouts,
    Alerts,
}

impl TabId {
    pub const fn name(self) -> &'static str {
        match self {
            Self::Live => "Live",
            Self::Overview => "Overview",
            Self::TimeSeries => "Time series",
            Self::Windows => "Windows",
            Self::Slots => "Slots",
            Self::LeaderTimeouts => "Leader timeouts",
            Self::Alerts => "Alerts",
        }
    }
}

/// Build the tab layout for the given activity classification.
///
/// `Overview` is always tab `1` — operator muscle memory should not
/// depend on whether the input is active or static. The `Live` tab is
/// appended at the end when the log is currently being written to;
/// static logs omit it entirely so the layout matches the historical
/// 6-tab UI exactly.
fn tab_layout(activity: &crate::live::detect::Activity) -> Vec<TabId> {
    let mut v = vec![
        TabId::Overview,
        TabId::TimeSeries,
        TabId::Windows,
        TabId::Slots,
        TabId::LeaderTimeouts,
        TabId::Alerts,
    ];
    if matches!(activity, crate::live::detect::Activity::Active) {
        v.push(TabId::Live);
    }
    v
}

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
    /// Classification of the input log file's activity, computed once
    /// at startup before the TUI takes over (the size-delta poll in
    /// `live::detect::classify` blocks for ~2s and must not run on
    /// the render path). Drives the Live tab's three render states.
    pub activity: crate::live::detect::Activity,
    /// True when the user has pressed SPACEBAR on the Live tab to start
    /// real-time following. LIVE-2 only flips the flag; the actual tail
    /// thread is wired in LIVE-3. Ignored when `activity` is `Static`.
    pub following: bool,
    /// Ordered list of tabs visible in this run. Active logs get
    /// `[Live, Overview, ...]`; static logs omit `Live` for a clean
    /// 6-tab layout. `current_tab` is an index into this vector.
    pub tabs: Vec<TabId>,
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
    pub fn new(
        state: &'s State,
        buckets: Option<&'s TimeBuckets>,
        bucket_secs: i64,
        activity: crate::live::detect::Activity,
    ) -> Self {
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
            // Always default to tab 0 (Overview). Overview is the
            // primary surface for both static and active runs; live
            // following is an explicit choice the operator makes by
            // pressing `7` (active layouts only).
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
            tabs: tab_layout(&activity),
            activity,
            following: false,
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
            FilterKind::VskipOnly => {
                self.slot_filters.vskip_only = !self.slot_filters.vskip_only;
            }
            FilterKind::CanonicalSkipOnly => {
                self.slot_filters.canonical_skip_only = !self.slot_filters.canonical_skip_only;
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
    /// `TabId` of the currently-selected tab. Single source of truth
    /// for every per-tab conditional check (key handlers, dispatch,
    /// status bar) so the same code works whether or not `Live` is
    /// present in the layout.
    pub fn current_kind(&self) -> TabId {
        self.tabs[self.current_tab]
    }

    fn scroll_target(&mut self) -> Option<&mut usize> {
        match self.current_kind() {
            TabId::Slots => Some(&mut self.slot_scroll),
            TabId::LeaderTimeouts => Some(&mut self.resume_scroll),
            TabId::Alerts => Some(&mut self.alert_scroll),
            _ => None,
        }
    }

    fn scroll_max(&self) -> usize {
        match self.current_kind() {
            TabId::Slots => self.slot_indices.len().saturating_sub(1),
            TabId::LeaderTimeouts => self.resume_rows.len().saturating_sub(1),
            TabId::Alerts => self.state.alerts.len().saturating_sub(1),
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
        let n = self.tabs.len();
        if n > 0 {
            self.current_tab = (self.current_tab + 1) % n;
        }
    }

    fn prev_tab(&mut self) {
        let n = self.tabs.len();
        if n > 0 {
            self.current_tab = (self.current_tab + n - 1) % n;
        }
    }

    fn set_tab(&mut self, idx: usize) {
        if idx < self.tabs.len() {
            self.current_tab = idx;
        }
    }
}

pub fn run(
    state: &State,
    buckets: Option<&TimeBuckets>,
    bucket_secs: i64,
    activity: crate::live::detect::Activity,
) -> Result<(), TuiError> {
    install_panic_hook();
    let mut terminal = setup_terminal()?;
    let mut app = App::new(state, buckets, bucket_secs, activity);
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
                    // Digit keys are 1-indexed into the active tab
                    // layout. `set_tab` bounds-checks against
                    // `app.tabs.len()`, so `7` on a 6-tab static layout
                    // is a silent no-op rather than a panic.
                    KeyCode::Char('1') => app.set_tab(0),
                    KeyCode::Char('2') => app.set_tab(1),
                    KeyCode::Char('3') => app.set_tab(2),
                    KeyCode::Char('4') => app.set_tab(3),
                    KeyCode::Char('5') => app.set_tab(4),
                    KeyCode::Char('6') => app.set_tab(5),
                    KeyCode::Char('7') => app.set_tab(6),
                    KeyCode::Tab => app.next_tab(),
                    KeyCode::BackTab => app.prev_tab(),
                    // Live-tab-only: SPACEBAR toggles the follow flag.
                    // Only fires when Live is in the layout (i.e. the
                    // log is Active); static-log runs do not see this.
                    KeyCode::Char(' ') if app.current_kind() == TabId::Live => {
                        app.following = !app.following;
                    }
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
                    KeyCode::Char('y') if app.current_kind() == TabId::Alerts => {
                        app.yank_current_alert();
                    }
                    // Slot-tab-only filter shortcuts. Gated on
                    // `current_kind() == TabId::Slots` so the same
                    // letters stay free for future tab-specific
                    // bindings elsewhere.
                    KeyCode::Char('t') if app.current_kind() == TabId::Slots => {
                        app.toggle_filter(FilterKind::Tcl);
                    }
                    KeyCode::Char('n') if app.current_kind() == TabId::Slots => {
                        app.toggle_filter(FilterKind::S2n);
                    }
                    // `p` for S2S (safe-to-ski**P**); pairs with `n`
                    // for S2N — both safe-to-X events use the last
                    // letter of their qualifier.
                    KeyCode::Char('p') if app.current_kind() == TabId::Slots => {
                        app.toggle_filter(FilterKind::S2s);
                    }
                    // `v` for VSKIP (we Voted skip, no canonical evidence).
                    // `c` for CSKIP (Canonical-skip). The two skip filters
                    // OR together in `SlotFilters::matches` so pressing
                    // both shows the union — equivalent to the old `[s]`
                    // "both buckets" toggle.
                    KeyCode::Char('v') if app.current_kind() == TabId::Slots => {
                        app.toggle_filter(FilterKind::VskipOnly);
                    }
                    KeyCode::Char('c') if app.current_kind() == TabId::Slots => {
                        app.toggle_filter(FilterKind::CanonicalSkipOnly);
                    }
                    KeyCode::Char('l') if app.current_kind() == TabId::Slots => {
                        app.toggle_filter(FilterKind::Leader);
                    }
                    KeyCode::Char('f') if app.current_kind() == TabId::Slots => {
                        app.toggle_filter(FilterKind::FastOnly);
                    }
                    // `s` for SLOW (was previously the combined VSKIP+CSKIP
                    // toggle; that combined behaviour is now expressed by
                    // pressing both `v` and `c`).
                    KeyCode::Char('s') if app.current_kind() == TabId::Slots => {
                        app.toggle_filter(FilterKind::SlowOnly);
                    }
                    // `x` clears all filters — moved here from `c` to free
                    // `c` for CSKIP. `x` reads as "cancel / clear".
                    KeyCode::Char('x') if app.current_kind() == TabId::Slots => {
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
    match app.current_kind() {
        TabId::Live => panel::live::render(
            &app.activity,
            &app.state.file_meta.path,
            app.following,
            frame,
            chunks[2],
        ),
        TabId::Overview => panel::overview::render(app, frame, chunks[2]),
        TabId::TimeSeries => panel::timeseries::render_detail(app.buckets, frame, chunks[2]),
        TabId::Windows => panel::windows::render(app, frame, chunks[2]),
        TabId::Slots => panel::slots::render(app, frame, chunks[2]),
        TabId::LeaderTimeouts => panel::leader_timeouts::render(app, frame, chunks[2]),
        TabId::Alerts => panel::alerts::render_full(app, frame, chunks[2]),
    }
    panel::status_bar::render(
        app.current_kind(),
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
    let titles: Vec<ratatui::text::Line<'_>> = app
        .tabs
        .iter()
        .enumerate()
        .map(|(i, kind)| {
            ratatui::text::Line::from(vec![
                ratatui::text::Span::styled(" [", theme::label_style()),
                ratatui::text::Span::styled(format!("{}", i + 1), theme::accent_style()),
                ratatui::text::Span::styled("] ", theme::label_style()),
                ratatui::text::Span::styled(kind.name(), theme::value_style()),
                ratatui::text::Span::styled(" ", theme::label_style()),
            ])
        })
        .collect();
    // Range string for the navigate title — `1-6` for static layouts
    // (6 tabs), `1-7` when Live is present. Derived from the live tab
    // count so adding a tab later does not desync this string.
    let nav_title = format!(
        " navigate  (1-{} · Tab / Shift+Tab{} · q quit) ",
        app.tabs.len(),
        if app.tabs.contains(&TabId::Live) {
            " · SPACE follow"
        } else {
            ""
        }
    );
    let tabs = Tabs::new(titles)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(nav_title)
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

// Tests live in a sibling file so this module stays under the
// ~800 LOC strong-warn threshold. The `#[path]` keeps the module
// identity as `crate::tui::app::tests`; `super` inside `app_tests.rs`
// still refers to `crate::tui::app`.
#[cfg(test)]
#[path = "app_tests.rs"]
mod tests;
