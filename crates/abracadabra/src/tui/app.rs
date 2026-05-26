//! Terminal setup + main event loop + per-tab dispatch.

use std::io::{self, Stdout};
use std::time::Duration;

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

use crate::model::alerts::{Alert, AlertKind, Severity};
use crate::model::analysis;
use crate::model::buckets::TimeBuckets;
use crate::model::slot::SlotStatus;
use crate::model::state::State;
use crate::tui::panel;
use crate::tui::theme;
use crate::tui::view::{SlotViewRow, VoteResumeViewRow};
use crate::tui::TuiError;

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
        AlertKind::LeaderTimeoutCrashed { at_slot } => {
            let _ = writeln!(
                out,
                "kind:          leader-timeout-crashed at slot {at_slot}"
            );
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
    pub buckets: Option<&'s TimeBuckets>,
    /// Time-series bucket size in seconds (set via `--bucket`). Carried
    /// here so panels that don't read `buckets` directly (e.g. Overview's
    /// file-meta block) can still surface the value to the user.
    pub bucket_secs: i64,
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
    /// Monotonic counter for `/tmp/abracadabra-yank-N.txt` filenames so
    /// repeated yanks during the same session don't overwrite.
    pub yank_counter: u32,
}

#[allow(clippy::missing_const_for_fn)] // interactive state machine — const is semantically wrong
impl<'s> App<'s> {
    pub fn new(state: &'s State, buckets: Option<&'s TimeBuckets>, bucket_secs: i64) -> Self {
        let slot_rows: Vec<SlotViewRow> =
            state.slots.values().map(SlotViewRow::from_record).collect();
        let slot_indices: Vec<usize> = (0..slot_rows.len()).collect();
        let mut resumes = analysis::vote_resumes_after_tcl(state);
        resumes.sort_by_key(|r| std::cmp::Reverse(r.resume_us));
        let resume_rows = resumes
            .into_iter()
            .map(VoteResumeViewRow::from_record)
            .collect();
        Self {
            state,
            buckets,
            bucket_secs,
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
        }
    }

    /// Write the currently-selected alert (Alerts tab) to a file under
    /// `/tmp/` so engineers can pipe / grep / copy it without fighting
    /// the TUI's raw-mode mouse capture. Sets `status_message` on
    /// completion (success path: file location; failure path: error).
    pub fn yank_current_alert(&mut self) {
        let Some(alert) = self.state.alerts.get(self.alert_scroll) else {
            self.status_message = Some("no alert under cursor".to_owned());
            return;
        };
        self.yank_counter = self.yank_counter.saturating_add(1);
        let path =
            std::path::PathBuf::from(format!("/tmp/abracadabra-yank-{}.txt", self.yank_counter));
        match std::fs::write(&path, format_alert_for_yank(self.state, alert)) {
            Ok(()) => {
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
        }
        self.recompute_slot_indices();
    }

    pub fn clear_filters(&mut self) {
        self.slot_filters = SlotFilters::default();
        self.recompute_slot_indices();
    }

    fn scroll_target(&mut self) -> &mut usize {
        match self.current_tab {
            3 => &mut self.slot_scroll,
            4 => &mut self.resume_scroll,
            5 => &mut self.alert_scroll,
            _ => &mut self.slot_scroll, // unused on Overview / Time series / Windows
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
        let target = self.scroll_target();
        if delta < 0 {
            let d = delta.unsigned_abs();
            *target = target.saturating_sub(d);
        } else {
            *target = (*target).saturating_add(delta as usize).min(max);
        }
    }

    fn jump_top(&mut self) {
        *self.scroll_target() = 0;
    }

    fn jump_bottom(&mut self) {
        let max = self.scroll_max();
        *self.scroll_target() = max;
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
    let mut terminal = setup_terminal()?;
    let mut app = App::new(state, buckets, bucket_secs);
    let result = event_loop(&mut terminal, &mut app);
    restore_terminal(&mut terminal)?;
    result
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
        0 => panel::overview::render(app.state, app.bucket_secs, frame, chunks[2]),
        1 => panel::timeseries::render_detail(app.buckets, frame, chunks[2]),
        2 => panel::windows::render(app.state, frame, chunks[2]),
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
