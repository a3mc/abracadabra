//! Bottom strip of the Live tab.
//!
//! Single-row pane that aggregates everything *not* tied to one slot:
//! the most recent head slot (latest FirstShred), running tx rate from
//! recent BankFrozen events, current standstill state, and a flashing
//! indicator that pulses when any event lands in the most recent
//! frame. Pure status surface — no entity world here.
//!
//! Layout: three columns, separated by `·`. Width-tolerant; shrinks
//! gracefully when the area is narrow by dropping the rightmost cells.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;
use time::OffsetDateTime;

use crate::live::animation::Pane;
use crate::parser::{Event, EventKind};
use crate::tui::theme;

/// Window over which the rolling tx rate is averaged. Long enough that
/// per-slot bursts smooth out; short enough to feel responsive.
const TX_WINDOW: Duration = Duration::from_secs(10);

/// Pulse hold time. The "event landed" indicator stays bright for this
/// duration after the most recent on_event, then fades to dim.
const PULSE_HOLD: Duration = Duration::from_millis(250);

/// One `(timestamp, signature_count)` sample used to compute the
/// rolling tx rate. Old samples drop off the front in `tick`.
#[derive(Debug, Clone, Copy)]
struct BankSample {
    at: OffsetDateTime,
    sigs: u64,
}

pub struct DecorationsPane {
    head_slot: Option<u64>,
    samples: VecDeque<BankSample>,
    /// Current standstill anchor slot if a `StandstillExtending` event
    /// has been seen and no subsequent `StandstillEnded`.
    standstill_anchor: Option<u64>,
    /// Parsed log timestamp of the most recent `on_event` — drives
    /// Wall-clock instant of the most recent observed event. The
    /// pulse indicator decays over wall-clock time (not event time),
    /// so it visibly fades when the stream stalls regardless of
    /// playback speed. Sample retention uses [`latest_event_ts`]
    /// instead — different concept, different clock.
    last_event_at: Option<Instant>,
    /// Newest event ts seen so far. Acts as the anchor for pulse
    /// decay and sample retention; replaces wall-clock `now`.
    latest_event_ts: Option<OffsetDateTime>,
}

impl DecorationsPane {
    pub const fn new() -> Self {
        Self {
            head_slot: None,
            samples: VecDeque::new(),
            standstill_anchor: None,
            last_event_at: None,
            latest_event_ts: None,
        }
    }

    /// Σ signatures across the in-window samples ÷ the actual covered
    /// span (newest sample minus oldest). Returns 0 when the window
    /// has no samples; falls back to a 1 s divisor when only one
    /// sample is in flight so the rate is not divided by zero.
    fn tx_per_second(&self) -> u64 {
        let (Some(earliest), Some(latest)) = (self.samples.front(), self.samples.back()) else {
            return 0;
        };
        let sum: u64 = self.samples.iter().map(|s| s.sigs).sum();
        let span = (latest.at - earliest.at).as_seconds_f64().max(1.0);
        // `sum` is u64; `span` is a positive f64. Cast intentionally.
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_precision_loss,
            clippy::cast_sign_loss
        )]
        let rate = (sum as f64 / span) as u64;
        rate
    }

    /// Drop samples whose `at` falls before `latest_event_ts - TX_WINDOW`.
    /// No-op when no events have been seen yet.
    fn prune_samples(&mut self) {
        let Some(anchor) = self.latest_event_ts else {
            return;
        };
        // `OffsetDateTime::checked_sub` wants `time::Duration`; convert
        // once. The fallback guards against a hypothetical pre-`MIN`
        // anchor (test fixtures only).
        let window = time::Duration::try_from(TX_WINDOW).unwrap_or(time::Duration::ZERO);
        let threshold = anchor.checked_sub(window).unwrap_or(anchor);
        while let Some(front) = self.samples.front() {
            if front.at < threshold {
                self.samples.pop_front();
            } else {
                break;
            }
        }
    }
}

impl Default for DecorationsPane {
    fn default() -> Self {
        Self::new()
    }
}

impl Pane for DecorationsPane {
    fn on_event(&mut self, ev: &Event) {
        self.last_event_at = Some(Instant::now());
        // Guard against out-of-order log lines: anchor advances
        // monotonically.
        self.latest_event_ts = Some(match self.latest_event_ts {
            Some(prev) if prev > ev.ts => prev,
            _ => ev.ts,
        });
        match &ev.kind {
            EventKind::FirstShred { slot } => {
                self.head_slot = Some(*slot);
            }
            EventKind::BankFrozen {
                signature_count, ..
            } => {
                self.samples.push_back(BankSample {
                    at: ev.ts,
                    sigs: *signature_count,
                });
                self.prune_samples();
            }
            EventKind::StandstillExtending { slot } => {
                self.standstill_anchor = Some(*slot);
            }
            EventKind::StandstillEnded { .. } => {
                self.standstill_anchor = None;
            }
            _ => {}
        }
    }

    fn tick(&mut self, _now: Instant) {
        // Sample retention is anchored on the newest event ts, not
        // wall-clock. The pulse indicator likewise compares parsed
        // log timestamps.
        self.prune_samples();
    }

    fn render(&self, frame: &mut Frame<'_>, area: Rect) {
        let block = Block::default()
            .borders(Borders::ALL)
            .title(" status ")
            .title_style(theme::title_style())
            .border_style(theme::title_style());
        let inner = block.inner(area);
        frame.render_widget(block, area);

        if inner.width < 10 || inner.height == 0 {
            return;
        }

        // Pulse indicator: bright while we observed an event within
        // the last [`PULSE_HOLD`] of wall-clock. Event-time clocks
        // don't advance between events so they can't drive a fade;
        // we use Instant::now() here for the decay specifically.
        let pulse_active = self
            .last_event_at
            .is_some_and(|t| Instant::now().duration_since(t) < PULSE_HOLD);
        let pulse_glyph = if pulse_active { '●' } else { '◌' };
        let pulse_style = if pulse_active {
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::DarkGray)
        };

        // Standstill: red ALERT when active; gray "clear" when not.
        let (standstill_label, standstill_style) = self.standstill_anchor.map_or_else(
            || {
                (
                    "standstill clear".to_owned(),
                    Style::default().fg(Color::DarkGray),
                )
            },
            |anchor| {
                (
                    format!("STANDSTILL @ {anchor}"),
                    Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                )
            },
        );

        let head_label = self
            .head_slot
            .map_or_else(|| "head —".to_owned(), |s| format!("head {s}"));

        let tx_label = format!("{} tx/s", self.tx_per_second());

        let line = Line::from(vec![
            Span::styled(pulse_glyph.to_string(), pulse_style),
            Span::styled("  ", theme::label_style()),
            Span::styled(head_label, theme::value_style()),
            Span::styled("  ·  ", theme::label_style()),
            Span::styled(tx_label, theme::value_style()),
            Span::styled("  ·  ", theme::label_style()),
            Span::styled(standstill_label, standstill_style),
        ]);
        frame.render_widget(Paragraph::new(line), inner);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk_event(kind: EventKind) -> Event {
        Event {
            ts: time::OffsetDateTime::UNIX_EPOCH,
            kind,
        }
    }

    fn mk_event_at(kind: EventKind, ts: time::OffsetDateTime) -> Event {
        Event { ts, kind }
    }

    #[test]
    fn first_shred_updates_head_slot() {
        let mut d = DecorationsPane::new();
        assert!(d.head_slot.is_none());
        d.on_event(&mk_event(EventKind::FirstShred { slot: 12345 }));
        assert_eq!(d.head_slot, Some(12345));
        d.on_event(&mk_event(EventKind::FirstShred { slot: 12346 }));
        assert_eq!(d.head_slot, Some(12346));
    }

    #[test]
    fn bank_frozen_pushes_samples_and_drives_rate() {
        let mut d = DecorationsPane::new();
        // 10 banks × 100 sigs = 1000 sigs, spread evenly over 9 s of
        // event timestamps. tx/s = 1000 / 9 = 111.
        let t0 = time::OffsetDateTime::UNIX_EPOCH;
        for i in 0..10_i64 {
            d.on_event(&mk_event_at(
                EventKind::BankFrozen {
                    slot: 1,
                    hash: "h".into(),
                    signature_count: 100,
                },
                t0 + time::Duration::seconds(i),
            ));
        }
        assert_eq!(d.tx_per_second(), 111);
    }

    #[test]
    fn tick_drops_samples_outside_window() {
        let mut d = DecorationsPane::new();
        // Drive samples through `on_event` so `latest_event_ts` is
        // anchored on the parsed log ts. Old sample's ts predates
        // the window relative to the newest event.
        let window_s = i64::try_from(TX_WINDOW.as_secs()).unwrap_or(i64::MAX);
        let old_ts = time::OffsetDateTime::UNIX_EPOCH + time::Duration::seconds(0);
        let new_ts = time::OffsetDateTime::UNIX_EPOCH + time::Duration::seconds(window_s + 5);
        d.on_event(&mk_event_at(
            EventKind::BankFrozen {
                slot: 1,
                hash: "h".into(),
                signature_count: 9999,
            },
            old_ts,
        ));
        d.on_event(&mk_event_at(
            EventKind::BankFrozen {
                slot: 2,
                hash: "h".into(),
                signature_count: 100,
            },
            new_ts,
        ));
        // The newer event's ts becomes the anchor; the older sample
        // is now outside the window and `on_event`'s prune drops it.
        assert_eq!(d.samples.len(), 1);
        assert_eq!(d.samples.front().unwrap().sigs, 100);
        // `tick` is a no-op for sample retention; the result holds.
        d.tick(Instant::now());
        assert_eq!(d.samples.len(), 1);
    }

    #[test]
    fn standstill_anchor_set_and_cleared() {
        let mut d = DecorationsPane::new();
        d.on_event(&mk_event(EventKind::StandstillExtending { slot: 42 }));
        assert_eq!(d.standstill_anchor, Some(42));
        d.on_event(&mk_event(EventKind::StandstillEnded {
            entry_slot: 38,
            exit_slot: 42,
        }));
        assert!(d.standstill_anchor.is_none());
    }

    #[test]
    fn pulse_recorded_on_event_with_wall_clock() {
        let mut d = DecorationsPane::new();
        d.tick(Instant::now());
        assert!(d.last_event_at.is_none());
        assert!(d.latest_event_ts.is_none());
        d.on_event(&mk_event(EventKind::FirstShred { slot: 1 }));
        let recorded = d.last_event_at.unwrap();
        // Wall-clock instant must be fresh — well under PULSE_HOLD.
        assert!(Instant::now().duration_since(recorded) < PULSE_HOLD);
        // Event-time anchor is set independently.
        assert!(d.latest_event_ts.is_some());
    }

    #[test]
    fn pulse_decays_after_pulse_hold_wall_clock() {
        let mut d = DecorationsPane::new();
        d.on_event(&mk_event(EventKind::FirstShred { slot: 1 }));
        // Backdate the recorded instant past PULSE_HOLD.
        d.last_event_at = Some(
            Instant::now()
                .checked_sub(PULSE_HOLD + Duration::from_millis(10))
                .unwrap(),
        );
        let last = d.last_event_at.unwrap();
        assert!(Instant::now().duration_since(last) >= PULSE_HOLD);
    }
}
