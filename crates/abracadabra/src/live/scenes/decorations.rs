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
    at: Instant,
    sigs: u64,
}

pub struct DecorationsPane {
    head_slot: Option<u64>,
    samples: VecDeque<BankSample>,
    /// Current standstill anchor slot if a `StandstillExtending` event
    /// has been seen and no subsequent `StandstillEnded`.
    standstill_anchor: Option<u64>,
    /// Most recent `on_event` instant — drives the pulse indicator.
    last_event_at: Option<Instant>,
    /// Now updated on each tick so render can compute pulse decay
    /// without taking a fresh time sample.
    now: Instant,
}

impl DecorationsPane {
    pub fn new() -> Self {
        Self {
            head_slot: None,
            samples: VecDeque::new(),
            standstill_anchor: None,
            last_event_at: None,
            now: Instant::now(),
        }
    }

    /// Σ signatures across the in-window samples ÷ window seconds.
    /// Returns 0 when the window has no samples yet.
    fn tx_per_second(&self) -> u64 {
        if self.samples.is_empty() {
            return 0;
        }
        let sum: u64 = self.samples.iter().map(|s| s.sigs).sum();
        let secs = TX_WINDOW.as_secs().max(1);
        sum / secs
    }
}

impl Default for DecorationsPane {
    fn default() -> Self {
        Self::new()
    }
}

impl Pane for DecorationsPane {
    fn on_event(&mut self, ev: &Event) {
        self.last_event_at = Some(self.now);
        match &ev.kind {
            EventKind::FirstShred { slot } => {
                self.head_slot = Some(*slot);
            }
            EventKind::BankFrozen {
                signature_count, ..
            } => {
                self.samples.push_back(BankSample {
                    at: self.now,
                    sigs: *signature_count,
                });
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

    fn tick(&mut self, now: Instant) {
        self.now = now;
        // Drop samples older than the window. `checked_sub` is the
        // safe form for `Instant - Duration` (the subtraction can
        // saturate at the platform epoch on some targets).
        let earliest_kept = now.checked_sub(TX_WINDOW);
        while let Some(front) = self.samples.front() {
            if let Some(threshold) = earliest_kept {
                if front.at < threshold {
                    self.samples.pop_front();
                    continue;
                }
            }
            break;
        }
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

        // Pulse indicator: bright while a recent event is within
        // PULSE_HOLD, dim otherwise.
        let pulse_active = self
            .last_event_at
            .is_some_and(|t| self.now.saturating_duration_since(t) < PULSE_HOLD);
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
        // 10 banks × 100 sigs = 1000 sigs in window; tx/s = 1000 / 10 = 100.
        for _ in 0..10 {
            d.on_event(&mk_event(EventKind::BankFrozen {
                slot: 1,
                hash: "h".into(),
                signature_count: 100,
            }));
        }
        assert_eq!(d.tx_per_second(), 100);
    }

    #[test]
    fn tick_drops_samples_outside_window() {
        let mut d = DecorationsPane::new();
        // Inject one fresh sample and one ancient sample directly.
        d.samples.push_back(BankSample {
            at: Instant::now()
                .checked_sub(TX_WINDOW + Duration::from_secs(5))
                .unwrap(),
            sigs: 9999,
        });
        d.samples.push_back(BankSample {
            at: Instant::now(),
            sigs: 100,
        });
        d.tick(Instant::now());
        assert_eq!(d.samples.len(), 1);
        assert_eq!(d.samples.front().unwrap().sigs, 100);
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
    fn pulse_active_immediately_after_event() {
        let mut d = DecorationsPane::new();
        d.tick(Instant::now());
        let was_silent = d
            .last_event_at
            .is_none_or(|t| d.now.saturating_duration_since(t) >= PULSE_HOLD);
        assert!(was_silent);
        d.on_event(&mk_event(EventKind::FirstShred { slot: 1 }));
        // After an event, last_event_at is set; pulse is active until PULSE_HOLD elapses.
        assert!(d
            .last_event_at
            .is_some_and(|t| d.now.saturating_duration_since(t) < PULSE_HOLD));
    }
}
