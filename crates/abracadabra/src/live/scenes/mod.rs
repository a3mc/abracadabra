//! Concrete scenes that compose the Live tab.
//!
//! A [`SceneEngine`] owns one or more [`crate::live::animation::Pane`]
//! implementations plus a layout constraint per pane. Its job:
//!
//! 1. Drain new events from the shared tail buffer since the last tick.
//! 2. Dispatch each event to every pane's `on_event`.
//! 3. Tick every pane.
//! 4. Render every pane into its constraint-sized sub-rect of the
//!    container area.
//!
//! The cursor that tracks "events I have already seen" lives here, not
//! on individual panes — every pane in the engine sees the same event
//! stream exactly once per pane per event.

pub mod decorations;
pub mod shred_ingress;
pub mod slot_lifecycle;

use std::sync::{Arc, Mutex};
use std::time::Instant;

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::Frame;

use crate::live::animation::Pane;
use crate::live::tail::{LiveBuffer, TailHandle, RECENT_CAPACITY};
use crate::parser::Event;

/// One pane plus the vertical-layout constraint allocated to it inside
/// the engine's render area.
struct PaneSlot {
    pane: Box<dyn Pane>,
    constraint: Constraint,
}

/// Composite scene engine for the Live tab.
///
/// Constructed alongside the [`TailHandle`] on SPACEBAR-start; dropped
/// alongside it on SPACEBAR-stop. Single-threaded, lives on the render
/// thread.
/// No-op pane used as a layout filler so the breathing strip between
/// the named panes and the decorations strip is allocated explicitly.
/// Future strips (voting, leader window, etc.) will replace this slot.
struct EmptyPane;

impl crate::live::animation::Pane for EmptyPane {
    fn on_event(&mut self, _ev: &crate::parser::Event) {}
    fn tick(&mut self, _now: Instant) {}
    fn render(&self, _frame: &mut Frame<'_>, _area: Rect) {}
}

pub struct SceneEngine {
    slots: Vec<PaneSlot>,
    /// `LiveBuffer::total_events` value as of the last successful drain.
    /// New events are everything after this cursor, capped by the
    /// buffer's `RECENT_CAPACITY` (older events evict before we can
    /// see them under sustained burst load — counters in `LiveBuffer`
    /// still reflect the honest totals).
    cursor: u64,
}

impl SceneEngine {
    /// Build the default Live-tab composite: two compact strips at the
    /// top (shred ingress, slot lifecycle), breathing room reserved
    /// below for future strips, decorations at the bottom.
    pub fn default_layout() -> Self {
        Self {
            slots: vec![
                PaneSlot {
                    pane: Box::new(shred_ingress::ShredIngressPane::new()),
                    constraint: Constraint::Length(shred_ingress::PANE_HEIGHT),
                },
                PaneSlot {
                    pane: Box::new(slot_lifecycle::SlotLifecyclePane::new()),
                    constraint: Constraint::Length(slot_lifecycle::PANE_HEIGHT),
                },
                PaneSlot {
                    pane: Box::new(EmptyPane),
                    constraint: Constraint::Min(0),
                },
                PaneSlot {
                    pane: Box::new(decorations::DecorationsPane::new()),
                    constraint: Constraint::Length(3),
                },
            ],
            cursor: 0,
        }
    }

    /// Drain newly-appeared events from `tail`, dispatch them to every
    /// pane in order, then call `tick(now)` on every pane.
    ///
    /// Lock on the tail buffer is briefly held for the drain only; pane
    /// state is mutated entirely outside the lock so render and
    /// long-running pane logic do not block the tail thread's
    /// publishes.
    pub fn tick(&mut self, tail: &TailHandle, now: Instant) {
        let new_events = drain_since(&tail.buffer, &mut self.cursor);
        for ev in &new_events {
            for slot in &mut self.slots {
                slot.pane.on_event(ev);
            }
        }
        for slot in &mut self.slots {
            slot.pane.tick(now);
        }
    }

    /// Render the Live-tab layout:
    ///
    /// ```text
    /// ┌────────────────────────────────────────────────────────────┐
    /// │ shred ingress          │ recent slots                      │  ← top row, 50/50 split
    /// │ (pane 0)               │ (pane 1)                          │
    /// ├────────────────────────────────────────────────────────────┤
    /// │                                                            │  ← reserved breathing room
    /// │ (future strips land here as a horizontal stack)            │     (pane 2 = EmptyPane)
    /// ├────────────────────────────────────────────────────────────┤
    /// │ decorations strip                                          │  ← pane 3
    /// └────────────────────────────────────────────────────────────┘
    /// ```
    ///
    /// The top-row height is the maximum of the two top panes'
    /// declared row heights; future strips live in the breathing slot
    /// in the middle.
    pub fn render(&self, frame: &mut Frame<'_>, area: Rect) {
        // For the current Live-tab layout: slots[0] = shred ingress,
        // slots[1] = slot lifecycle, slots[2] = empty filler,
        // slots[3] = decorations. If any are missing, fall back to a
        // simple vertical-stack render so this method stays robust to
        // smaller compositions during development.
        if self.slots.len() < 4 {
            self.render_vertical_stack(frame, area);
            return;
        }

        let top_height = constraint_max_rows(self.slots[0].constraint, area.height)
            .max(constraint_max_rows(self.slots[1].constraint, area.height));
        let v_chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(top_height),
                Constraint::Min(0),
                self.slots[3].constraint,
            ])
            .split(area);

        let top_h = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(v_chunks[0]);
        self.slots[0].pane.render(frame, top_h[0]);
        self.slots[1].pane.render(frame, top_h[1]);
        // Middle breathing strip: render the EmptyPane into v_chunks[1]
        // so it can grow into a real strip later by swapping the pane.
        self.slots[2].pane.render(frame, v_chunks[1]);
        self.slots[3].pane.render(frame, v_chunks[2]);
    }

    /// Vertical stack fallback used when the composition does not
    /// match the canonical Live-tab layout.
    fn render_vertical_stack(&self, frame: &mut Frame<'_>, area: Rect) {
        let constraints: Vec<Constraint> = self.slots.iter().map(|s| s.constraint).collect();
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints(constraints)
            .split(area);
        for (slot, rect) in self.slots.iter().zip(chunks.iter()) {
            slot.pane.render(frame, *rect);
        }
    }
}

/// Resolve a `Constraint` against an available row budget into the
/// expected row count it would consume. `Length` returns the literal
/// value; `Percentage` resolves against `area_rows`; `Min` falls back
/// to its own minimum. Used by the canonical Live-tab layout to size
/// the top row.
fn constraint_max_rows(c: Constraint, area_rows: u16) -> u16 {
    match c {
        Constraint::Length(n) => n,
        Constraint::Percentage(p) => (u32::from(area_rows) * u32::from(p) / 100) as u16,
        Constraint::Min(n) => n,
        _ => 0,
    }
}

/// Pull events from the buffer that appeared after `cursor`. Updates
/// `cursor` to the buffer's current `total_events` so the next drain
/// only sees newer events.
fn drain_since(buffer: &Arc<Mutex<LiveBuffer>>, cursor: &mut u64) -> Vec<Event> {
    let Ok(buf) = buffer.lock() else {
        return Vec::new();
    };
    let current = buf.total_events;
    if current <= *cursor {
        return Vec::new();
    }
    // How many events are new since the last drain. Cap at the buffer's
    // visible capacity — sustained bursts that evict events before we
    // see them are accepted (counters survive on the buffer itself).
    let new_count = (current - *cursor).min(RECENT_CAPACITY as u64) as usize;
    let new_count = new_count.min(buf.recent.len());
    let start = buf.recent.len().saturating_sub(new_count);
    let events: Vec<Event> = buf.recent.iter().skip(start).cloned().collect();
    *cursor = current;
    events
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::live::tail::LiveBuffer;
    use crate::parser::{Event, EventKind};

    fn ev(slot: u64) -> Event {
        Event {
            ts: time::OffsetDateTime::UNIX_EPOCH,
            kind: EventKind::FirstShred { slot },
        }
    }

    #[test]
    fn drain_returns_nothing_when_cursor_caught_up() {
        let buf = Arc::new(Mutex::new(LiveBuffer::default()));
        let mut cursor = 0u64;
        let out = drain_since(&buf, &mut cursor);
        assert!(out.is_empty());
    }

    #[test]
    fn drain_advances_cursor_and_returns_new_events() {
        let buf = Arc::new(Mutex::new(LiveBuffer::default()));
        {
            let mut b = buf.lock().unwrap();
            for i in 0..3 {
                b.recent.push_back(ev(i));
                b.total_events += 1;
            }
        }
        let mut cursor = 0u64;
        let out = drain_since(&buf, &mut cursor);
        assert_eq!(out.len(), 3);
        assert_eq!(cursor, 3);
        // Second call sees no new events.
        let out2 = drain_since(&buf, &mut cursor);
        assert!(out2.is_empty());
        assert_eq!(cursor, 3);
    }

    #[test]
    fn drain_returns_only_events_added_since_last_call() {
        let buf = Arc::new(Mutex::new(LiveBuffer::default()));
        let mut cursor = 0u64;
        {
            let mut b = buf.lock().unwrap();
            b.recent.push_back(ev(1));
            b.total_events += 1;
        }
        let out1 = drain_since(&buf, &mut cursor);
        assert_eq!(out1.len(), 1);
        {
            let mut b = buf.lock().unwrap();
            b.recent.push_back(ev(2));
            b.recent.push_back(ev(3));
            b.total_events += 2;
        }
        let out2 = drain_since(&buf, &mut cursor);
        assert_eq!(out2.len(), 2);
        assert_eq!(cursor, 3);
    }

    #[test]
    fn drain_caps_at_recent_capacity_when_burst_evicted_older() {
        // Simulate a burst larger than RECENT_CAPACITY: cursor at 0,
        // total_events = capacity + 50, recent holds just the last
        // `RECENT_CAPACITY` items. We should see at most RECENT_CAPACITY
        // events and the cursor jumps to the current total.
        let buf = Arc::new(Mutex::new(LiveBuffer::default()));
        {
            let mut b = buf.lock().unwrap();
            for i in 0..(RECENT_CAPACITY + 50) {
                if b.recent.len() == RECENT_CAPACITY {
                    b.recent.pop_front();
                }
                b.recent.push_back(ev(i as u64));
                b.total_events += 1;
            }
        }
        let mut cursor = 0u64;
        let out = drain_since(&buf, &mut cursor);
        assert!(out.len() <= RECENT_CAPACITY);
        assert_eq!(cursor as usize, RECENT_CAPACITY + 50);
    }
}
