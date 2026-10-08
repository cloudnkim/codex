//! Optional, quiet character rain in the owned transcript viewport.

use std::time::Duration;
use std::time::Instant;

use crate::terminal_palette::StdoutColorLevel;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Color;
use ratatui::style::Modifier;
use ratatui::style::Style;
use std::sync::OnceLock;

pub(super) const FRAME_INTERVAL: Duration = Duration::from_millis(100);
pub(crate) const EMPTY_OUTPUT_CELL: &str = "\u{fdd0}";

static OUTPUT_RAIN: OnceLock<Option<MatrixRain>> = OnceLock::new();

pub(crate) fn output_rain() -> Option<&'static MatrixRain> {
    OUTPUT_RAIN
        .get_or_init(|| {
            MatrixRain::from_environment(crate::terminal_palette::effective_stdout_color_level())
        })
        .as_ref()
}

#[derive(Clone, Copy)]
struct ColumnState {
    head: i32,
    trail: u32,
    cycle: u32,
    step_ticks: u32,
}

fn mix(mut value: u32) -> u32 {
    value ^= value >> 16;
    value = value.wrapping_mul(0x7feb_352d);
    value ^= value >> 15;
    value = value.wrapping_mul(0x846c_a68b);
    value ^ (value >> 16)
}

fn column_state(x: u16, height: u16, tick: u128) -> ColumnState {
    let seed = mix(u32::from(x).wrapping_add(0x9e37_79b9));
    let cycle_length = |cycle: u32| {
        let variation = mix(seed ^ cycle.wrapping_mul(0x85eb_ca6b));
        let step_ticks = 2 + variation % 4;
        let trail = 2 + (variation >> 4) % 3;
        let gap = 5 + (variation >> 8) % 12;
        let duration = (u32::from(height) + 2 * trail) * step_ticks + gap;
        (duration, step_ticks, trail)
    };
    let total = (0..4).map(|cycle| cycle_length(cycle).0).sum::<u32>();
    let phase = mix(seed ^ 0xa511_e9b3) % total;
    let mut elapsed = ((tick as u64).wrapping_add(u64::from(phase)) % u64::from(total)) as u32;
    for cycle in 0..4 {
        let (duration, step_ticks, trail) = cycle_length(cycle);
        if elapsed < duration {
            return ColumnState {
                head: (elapsed / step_ticks) as i32 - trail as i32,
                trail,
                cycle,
                step_ticks,
            };
        }
        elapsed -= duration;
    }
    unreachable!("the selected frame must belong to a cycle")
}

pub(super) struct MatrixRain {
    started_at: Instant,
    color_level: StdoutColorLevel,
}

impl MatrixRain {
    fn from_environment(color_level: StdoutColorLevel) -> Option<Self> {
        let enabled = std::env::var("CODEX_MATRIX_RAIN").is_ok_and(|value| value == "1");
        if !enabled
            || !matches!(
                color_level,
                StdoutColorLevel::TrueColor | StdoutColorLevel::Ansi256
            )
        {
            return None;
        }
        Some(Self {
            started_at: Instant::now(),
            color_level,
        })
    }

    /// Consume every sentinel in the output area after all transcript controls have rendered.
    pub(crate) fn render(&self, area: Rect, buf: &mut Buffer, excluded: &[Rect]) {
        let tick = self.started_at.elapsed().as_millis() / FRAME_INTERVAL.as_millis();
        self.render_frame(area, buf, excluded, tick);
    }

    fn render_frame(&self, area: Rect, buf: &mut Buffer, excluded: &[Rect], tick: u128) {
        if area.is_empty() {
            return;
        }
        for x in area.left()..area.right() {
            let column = column_state(x, area.height, tick);
            let seed = mix(u32::from(x).wrapping_add(0x9e37_79b9));
            for y in area.top()..area.bottom() {
                let Some(cell) = buf.cell_mut((x, y)) else {
                    continue;
                };
                if cell.symbol() != EMPTY_OUTPUT_CELL {
                    continue;
                }
                // Highlighted blank cells belong to selection and search, not decoration.
                if excluded.iter().any(|rect| rect.contains((x, y).into()))
                    || cell.bg != Color::Reset
                    || cell.modifier.contains(Modifier::REVERSED)
                {
                    cell.set_symbol(" ");
                    continue;
                }
                let distance = column.head - i32::from(y - area.top());
                if distance < 0 || distance >= column.trail as i32 {
                    cell.set_symbol(" ");
                    continue;
                }
                let color = match (self.color_level, distance) {
                    (StdoutColorLevel::TrueColor, 0) => Color::Rgb(48, 108, 57),
                    (StdoutColorLevel::TrueColor, 1..=2) => Color::Rgb(30, 78, 39),
                    (StdoutColorLevel::TrueColor, 3..=4) => Color::Rgb(21, 55, 29),
                    (StdoutColorLevel::Ansi256, 0) => Color::Indexed(28),
                    (StdoutColorLevel::Ansi256, 1..=4) => Color::Indexed(22),
                    _ => {
                        cell.set_symbol(" ");
                        continue;
                    }
                };
                let glyph_delay = 2 + column.step_ticks + mix(seed ^ u32::from(y)) % 5;
                let glyph_index = mix(seed
                    ^ u32::from(y).wrapping_mul(0xc2b2_ae35)
                    ^ column.cycle.wrapping_mul(0x27d4_eb2d)
                    ^ ((tick as u32 / glyph_delay).wrapping_mul(0x1656_67b1)))
                    % 36;
                let glyph = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ"[glyph_index as usize] as char;
                cell.set_char(glyph).set_style(Style::default().fg(color));
            }
        }
    }
}
