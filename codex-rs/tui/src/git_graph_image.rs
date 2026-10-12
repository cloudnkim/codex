//! Pixel Git lanes, ported from clig_tree's gittree_graphics.py.
//! The mailbox lives only between the synchronous Tui frame begin/end calls.
use std::cell::RefCell;
use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, Ordering};

use base64::Engine as _;
use ratatui::layout::Rect;

const IMAGE_ID: u32 = 0x474954;
const PALETTE: [[u8; 3]; 6] = [
    [95, 215, 255],
    [135, 215, 135],
    [255, 175, 95],
    [215, 135, 255],
    [255, 135, 95],
    [135, 215, 255],
];
static VISIBLE: AtomicBool = AtomicBool::new(false);
thread_local! {
    static FRAME: RefCell<Option<Frame>> = const { RefCell::new(None) };
}
struct Frame {
    cell: (u16, u16),
    request: Option<Request>,
}
#[derive(Clone, PartialEq, Eq)]
struct Request {
    // Include one neighbor at each edge, even when it is outside the viewport.
    lanes: Vec<Vec<char>>,
    heads: Vec<bool>,
    area: Rect,
    cell: (u16, u16),
}
#[derive(Default)]
pub(crate) struct Renderer {
    cached: Option<Request>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(lanes: &[&str], heads: Vec<bool>) -> Request {
        Request {
            lanes: lanes.iter().map(|s| s.chars().collect()).collect(),
            heads,
            area: Rect::new(0, 0, 3, 1),
            cell: (10, 20),
        }
    }
    #[test]
    fn node_connects_to_offscreen_neighbors_at_its_center() {
        let image = raster(&request(&["│", "●", "│"], vec![false; 3]));
        assert!(image.get_pixel(4, 0)[3] > 0);
        assert!(image.get_pixel(4, 19)[3] > 0);
        assert_eq!(image.get_pixel(20, 10)[3], 0);
    }
    #[test]
    fn isolated_root_does_not_grow_edges_and_head_has_white_ring() {
        let image = raster(&request(&["", "●", ""], vec![false, true, false]));
        assert_eq!(image.get_pixel(4, 0)[3], 0);
        assert_eq!(image.get_pixel(4, 19)[3], 0);
        assert!(image.pixels().any(|pixel| pixel.0[..3] == [245, 245, 245]));
    }
    #[test]
    fn clipped_diagonal_enters_from_column_outside_viewport() {
        let mut request = request(&["", " ╲", ""], vec![false; 3]);
        request.area.width = 1;
        let image = raster(&request);
        assert!(image.get_pixel(6, 1)[3] > 0);
    }
    #[test]
    fn frame_guard_discards_requests_after_early_return() {
        FRAME.with(|slot| {
            *slot.borrow_mut() = Some(Frame {
                cell: (10, 20),
                request: None,
            })
        });
        let guard = FrameGuard;
        assert!(submit(
            vec!["".into(), "●".into(), "".into()],
            vec![false; 3],
            Rect::new(0, 0, 1, 1)
        ));
        drop(guard);
        assert!(FRAME.with(|slot| slot.borrow().is_none()));
    }
    #[test]
    fn frame_mailbox_is_consumed_and_reset() {
        FRAME.with(|slot| {
            *slot.borrow_mut() = Some(Frame {
                cell: (10, 20),
                request: None,
            })
        });
        assert!(submit(
            vec!["".into(), "●".into(), "".into()],
            vec![false; 3],
            Rect::new(0, 0, 1, 1)
        ));
        assert!(
            FRAME
                .with(|slot| slot.borrow_mut().take())
                .unwrap()
                .request
                .is_some()
        );
        assert!(!submit(Vec::new(), Vec::new(), Rect::default()));
    }
}

pub(crate) fn clear(writer: &mut impl Write) -> io::Result<()> {
    if VISIBLE.load(Ordering::Relaxed) {
        write!(writer, "\x1b_Ga=d,d=I,i={IMAGE_ID},q=2;\x1b\\")?;
        VISIBLE.store(false, Ordering::Relaxed);
    }
    Ok(())
}

pub(crate) struct FrameGuard;

impl Drop for FrameGuard {
    fn drop(&mut self) {
        FRAME.with(|slot| *slot.borrow_mut() = None);
    }
}

impl Renderer {
    pub(crate) fn begin(&mut self, writer: &mut impl Write) -> io::Result<FrameGuard> {
        FRAME.with(|slot| *slot.borrow_mut() = None);
        // Delete placement before terminal scrolling or buffer switches can move it.
        if VISIBLE.load(Ordering::Relaxed) {
            write!(writer, "\x1b_Ga=d,d=i,i={IMAGE_ID},p=1,q=2;\x1b\\")?;
        }
        let cmux = std::env::var_os("CMUX_SURFACE_ID").is_some()
            && std::env::var_os("TMUX").is_none()
            && std::env::var_os("TMUX_PANE").is_none()
            && std::env::var_os("ZELLIJ").is_none()
            && std::env::var_os("ZELLIJ_SESSION_NAME").is_none();
        let supported = cmux
            || matches!(
                crate::pets::image_protocol::detect_pet_image_support().protocol(),
                Some(
                    crate::pets::image_protocol::ImageProtocol::Kitty
                        | crate::pets::image_protocol::ImageProtocol::KittyLocalFile
                )
            );
        if supported && let Ok(size) = crossterm::terminal::window_size() {
            let cell = (
                size.width.checked_div(size.columns).unwrap_or(0),
                size.height.checked_div(size.rows).unwrap_or(0),
            );
            if (1..=256).contains(&cell.0) && (1..=512).contains(&cell.1) {
                FRAME.with(|slot| {
                    *slot.borrow_mut() = Some(Frame {
                        cell,
                        request: None,
                    })
                });
            }
        }
        Ok(FrameGuard)
    }

    pub(crate) fn end(&mut self, writer: &mut impl Write) -> io::Result<()> {
        let request = FRAME
            .with(|slot| slot.borrow_mut().take())
            .and_then(|frame| frame.request);
        let Some(request) = request else {
            clear(writer)?;
            self.cached = None;
            return Ok(());
        };
        let changed = self.cached.as_ref() != Some(&request) || !VISIBLE.load(Ordering::Relaxed);
        let mut command = format!("\x1b7\x1b[{};{}H", request.area.y + 1, request.area.x + 1);
        if changed {
            let image = raster(&request);
            let mut png = io::Cursor::new(Vec::new());
            image
                .write_to(&mut png, image::ImageFormat::Png)
                .map_err(io::Error::other)?;
            let encoded = base64::engine::general_purpose::STANDARD.encode(png.into_inner());
            let chunks = encoded.as_bytes().chunks(4096).collect::<Vec<_>>();
            for (index, chunk) in chunks.iter().enumerate() {
                let more = u8::from(index + 1 < chunks.len());
                if index == 0 {
                    command.push_str(&format!("\x1b_Ga=t,f=100,t=d,i={IMAGE_ID},q=2,m={more};"));
                } else {
                    command.push_str(&format!("\x1b_Gm={more},q=2;"));
                }
                command.push_str(std::str::from_utf8(chunk).expect("base64 is ASCII"));
                command.push_str("\x1b\\");
            }
        }
        command.push_str(&format!(
            "\x1b_Ga=p,i={IMAGE_ID},p=1,q=2,C=1,z=-1,c={},r={};\x1b\\\x1b8",
            request.area.width, request.area.height
        ));
        // Mark before writing so partial writes are still cleaned up on exit.
        VISIBLE.store(true, Ordering::Relaxed);
        let result = writer.write_all(command.as_bytes());
        if result.is_err() {
            let _ = writer.write_all(b"\x1b8");
            self.cached = None;
        } else {
            self.cached = Some(request);
        }
        result
    }
}

pub(crate) fn submit(lanes: Vec<String>, heads: Vec<bool>, area: Rect) -> bool {
    FRAME.with(|slot| {
        let mut slot = slot.borrow_mut();
        let Some(frame) = slot.as_mut() else {
            return false;
        };
        if area.is_empty() {
            return false;
        }
        frame.request = Some(Request {
            lanes: lanes.into_iter().map(|s| s.chars().collect()).collect(),
            heads,
            area,
            cell: frame.cell,
        });
        true
    })
}

fn raster(request: &Request) -> image::RgbaImage {
    let (cw, ch) = (f32::from(request.cell.0), f32::from(request.cell.1));
    let mut image = image::RgbaImage::new(
        u32::from(request.area.width) * u32::from(request.cell.0),
        u32::from(request.area.height) * u32::from(request.cell.1),
    );
    let glyph = |row: usize, col: isize| {
        request
            .lanes
            .get(row)
            .and_then(|line| usize::try_from(col).ok().and_then(|col| line.get(col)))
            .copied()
            .unwrap_or(' ')
    };
    let touches = |row, col, bottom| {
        matches!(glyph(row, col), '│' | '●')
            || glyph(row, col - 1) == if bottom { '╲' } else { '╱' }
            || glyph(row, col + 1) == if bottom { '╱' } else { '╲' }
    };
    let stroke = (cw / 7.0).max(1.0) / 2.0;
    for visible in 0..usize::from(request.area.height) {
        let row = visible + 1;
        let y0 = visible as f32 * ch;
        let y1 = y0 + ch;
        let cy = (y0 + y1) / 2.0;
        for col in 0..=usize::from(request.area.width) {
            let x = (col as f32 + 0.5) * cw;
            let color = PALETTE[(col / 2) % PALETTE.len()];
            match glyph(row, col as isize) {
                '●' => {
                    if touches(row - 1, col as isize, true) {
                        segment(&mut image, (x, y0), (x, cy), stroke, color);
                    }
                    if touches(row + 1, col as isize, false) {
                        segment(&mut image, (x, cy), (x, y1), stroke, color);
                    }
                    let radius = (cw * 0.36).min(ch * 0.2).max(2.0);
                    if request.heads.get(row).copied().unwrap_or(false) {
                        segment(&mut image, (x, cy), (x, cy), radius + 1.0, [245; 3]);
                    }
                    segment(&mut image, (x, cy), (x, cy), radius, color);
                }
                '│' => segment(&mut image, (x, y0), (x, y1), stroke, color),
                '╲' => segment(&mut image, (x - cw, y0), (x + cw, y1), stroke, color),
                '╱' => segment(&mut image, (x + cw, y0), (x - cw, y1), stroke, color),
                '─' => segment(
                    &mut image,
                    (x - cw / 2.0, cy),
                    (x + cw / 2.0, cy),
                    stroke,
                    color,
                ),
                _ => {}
            }
        }
    }
    image
}

fn segment(
    image: &mut image::RgbaImage,
    a: (f32, f32),
    b: (f32, f32),
    radius: f32,
    color: [u8; 3],
) {
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    let length = dx * dx + dy * dy;
    let right = ((a.0.max(b.0) + radius + 2.0).max(0.0) as u32).min(image.width());
    let bottom = ((a.1.max(b.1) + radius + 2.0).max(0.0) as u32).min(image.height());
    for y in (a.1.min(b.1) - radius - 1.0).max(0.0) as u32..bottom {
        for x in (a.0.min(b.0) - radius - 1.0).max(0.0) as u32..right {
            let (px, py) = (x as f32 + 0.5 - a.0, y as f32 + 0.5 - a.1);
            let t = if length > 0.0 {
                ((px * dx + py * dy) / length).clamp(0.0, 1.0)
            } else {
                0.0
            };
            let distance = ((px - t * dx).powi(2) + (py - t * dy).powi(2)).sqrt();
            let alpha = (255.0 * (radius + 0.5 - distance).clamp(0.0, 1.0)).round() as u8;
            let pixel = image.get_pixel_mut(x, y);
            if alpha >= pixel[3] {
                *pixel = image::Rgba([color[0], color[1], color[2], alpha]);
            }
        }
    }
}
