//! Fixed-width Matrix turn state for the passive composer footer.

use std::time::Duration;
use std::time::Instant;

use ratatui::style::Color;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::text::Span;

use crate::terminal_palette::StdoutColorLevel;

pub(super) const FRAME_TICK: Duration = Duration::from_millis(34);
const COMPLETE_HOLD: Duration = Duration::from_secs(3);

#[derive(Default)]
pub(super) struct MatrixActivity {
    running: bool,
    progress: Option<(usize, usize)>,
    completed_at: Option<Instant>,
    started_at: Option<Instant>,
}

impl MatrixActivity {
    pub(super) fn start(&mut self) {
        self.running = true;
        self.progress = None;
        self.completed_at = None;
        self.started_at = Some(Instant::now());
    }

    pub(super) fn stop(&mut self) {
        self.running = false;
        self.progress = None;
        self.completed_at = None;
    }

    pub(super) fn stop_unless_complete(&mut self) {
        if self.completed_at.is_none() {
            self.stop();
        }
    }

    pub(super) fn complete(&mut self) {
        if self.running {
            self.running = false;
            self.completed_at = Some(Instant::now());
        }
    }

    pub(super) fn set_progress(&mut self, completed: usize, total: usize) {
        if self.running {
            self.progress = (total > 0).then_some((completed.min(total), total));
        }
    }

    pub(super) fn next_frame_in(&self, now: Instant, animated: bool) -> Option<Duration> {
        if self.running && animated {
            Some(FRAME_TICK)
        } else {
            self.completed_at
                .and_then(|at| COMPLETE_HOLD.checked_sub(now.saturating_duration_since(at)))
                .filter(|remaining| !remaining.is_zero())
        }
    }

    pub(super) fn line(&self, now: Instant, animated: bool) -> Line<'static> {
        let complete = self
            .completed_at
            .is_some_and(|at| now.saturating_duration_since(at) < COMPLETE_HOLD);
        let label = if complete || self.running {
            "CODEX ÉXITO"
        } else {
            "CODEX READY"
        };
        let reveal = if complete {
            11
        } else if self.running {
            self.progress.map_or(0, |(done, total)| 10 * done / total)
        } else {
            11
        };
        let phase = self.started_at.map_or(0.0, |at| {
            now.saturating_duration_since(at).as_secs_f32() * 2.6
        });
        let color_level = crate::terminal_palette::effective_stdout_color_level();
        let decode_tick = if animated { (phase * 8.0) as usize } else { 0 };
        let hex = ['0', '1', 'A', 'F', '7', '9'];
        Line::from(
            label
                .chars()
                .enumerate()
                .map(|(index, character)| {
                    let distance = (index as f32 - (5.0 + 5.0 * phase.cos())).abs();
                    let glow = ((1.0 - distance / 2.0).max(0.0) * 85.0) as u8;
                    let (r, g, b) = if self.running && index >= reveal && character != ' ' {
                        (
                            18,
                            55u8.saturating_add(if animated { glow * 2 } else { 0 }),
                            31,
                        )
                    } else {
                        (
                            46,
                            145u8.saturating_add(if animated && self.running { glow } else { 0 }),
                            81,
                        )
                    };
                    let color = match color_level {
                        StdoutColorLevel::Ansi256 => Color::Indexed(if g > 185 {
                            48
                        } else if g > 95 {
                            34
                        } else {
                            22
                        }),
                        _ => Color::Rgb(r, g, b),
                    };
                    let display = if self.running && index >= reveal && character != ' ' {
                        hex[(index + decode_tick) % hex.len()]
                    } else {
                        character
                    };
                    Span::styled(display.to_string(), Style::default().fg(color))
                })
                .collect::<Vec<_>>(),
        )
    }

    pub(super) fn prefix_status_line(
        &self,
        now: Instant,
        animated: bool,
        mut status_line: Line<'static>,
    ) -> Line<'static> {
        let mut spans = self.line(now, animated).spans;
        spans.push("  ".into());
        spans.append(&mut status_line.spans);
        status_line.spans = spans;
        status_line
    }
}
