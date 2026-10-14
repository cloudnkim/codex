//! Cached workspace Git graph. Only explicit clicks fetch remotes or update the checkout.
use std::cell::Cell;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

use crossterm::event::MouseButton;
use crossterm::event::MouseEvent;
use crossterm::event::MouseEventKind;
use ratatui::buffer::Buffer;
use ratatui::layout::Alignment;
use ratatui::layout::Rect;
use ratatui::style::Color;
use ratatui::style::Modifier;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::text::Span;
use ratatui::widgets::Paragraph;
use ratatui::widgets::Widget;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use crate::render::renderable::Renderable;
use crate::tui::FrameRequester;
use crate::workspace_command::WorkspaceCommand;
use crate::workspace_command::WorkspaceCommandRunner;

const POLL: Duration = Duration::from_secs(3);
const DEFAULT_ROWS: u16 = 10;
const MIN_ROWS: u16 = 3;
const PALETTE: [Color; 6] = [
    Color::Indexed(81),
    Color::Indexed(114),
    Color::Indexed(215),
    Color::Indexed(177),
    Color::Indexed(209),
    Color::Indexed(117),
];

#[derive(Clone, Copy, PartialEq, Eq)]
enum GitAction {
    Fetch,
    Update,
}

impl GitAction {
    fn name(self) -> &'static str {
        match self {
            Self::Fetch => "fetch",
            Self::Update => "update",
        }
    }

    fn args(self) -> &'static [&'static str] {
        match self {
            Self::Fetch => &["fetch", "--all"],
            // Respect the current branch's upstream; never create a merge, rebase, or stash.
            // Force the fetch phase to accept moved remote tags; HEAD still requires fast-forward.
            Self::Update => &[
                "pull",
                "--ff-only",
                "--no-rebase",
                "--no-autostash",
                "--tags",
                "--force",
            ],
        }
    }
}

struct Snapshot {
    fingerprint: String,
    branch: String,
    refs: Vec<String>,
    lines: Option<Vec<GraphRow>>,
    action_status: Option<String>,
}

#[derive(Clone, Copy)]
struct ResizeDrag {
    start_row: u16,
    rows: u16,
    max_rows: u16,
}

enum PickerEntry<'a> {
    FollowHead,
    Group(&'static str),
    Branch(&'a str),
}

fn picker_entries(refs: &[String]) -> Vec<PickerEntry<'_>> {
    let mut entries = vec![PickerEntry::FollowHead];
    for (prefix, label) in [("refs/heads/", "로컬"), ("refs/remotes/", "원격")] {
        let mut branches = refs
            .iter()
            .filter(|reference| reference.starts_with(prefix))
            .peekable();
        if branches.peek().is_some() {
            entries.push(PickerEntry::Group(label));
            entries.extend(branches.map(|reference| PickerEntry::Branch(reference)));
        }
    }
    entries
}

pub(super) struct GitGraphPanel {
    frame: FrameRequester,
    cwd: PathBuf,
    runner: Option<WorkspaceCommandRunner>,
    task: Option<JoinHandle<()>>,
    result: Option<oneshot::Receiver<Result<Option<Snapshot>, String>>>,
    next_poll: Instant,
    fingerprint: String,
    branch: String,
    refs: Vec<String>,
    lines: Vec<GraphRow>,
    selected: Option<String>,
    error: Option<String>,
    action_status: Option<String>,
    enabled: bool,
    collapsed: bool,
    picker: bool,
    scroll: Cell<usize>,
    picker_scroll: Cell<usize>,
    area: Cell<Rect>,
    visible_rows: u16,
    resize_drag: Cell<Option<ResizeDrag>>,
    action: Option<GitAction>,
}

impl GitGraphPanel {
    pub(super) fn new(frame: FrameRequester) -> Self {
        Self {
            frame,
            cwd: PathBuf::new(),
            runner: None,
            task: None,
            result: None,
            next_poll: Instant::now(),
            fingerprint: String::new(),
            branch: String::new(),
            refs: Vec::new(),
            lines: Vec::new(),
            selected: None,
            error: None,
            action_status: None,
            enabled: false,
            collapsed: false,
            picker: false,
            scroll: Cell::new(0),
            picker_scroll: Cell::new(0),
            area: Cell::new(Rect::default()),
            visible_rows: DEFAULT_ROWS,
            resize_drag: Cell::new(None),
            action: None,
        }
    }

    fn cancel(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
        self.result = None;
        self.action = None;
    }

    pub(super) fn hide(&self) {
        self.area.set(Rect::default());
        self.resize_drag.set(None);
    }

    pub(super) fn sync(
        &mut self,
        cwd: &Path,
        runner: Option<WorkspaceCommandRunner>,
        enabled: bool,
    ) {
        let same_runner = match (&self.runner, &runner) {
            (Some(old), Some(new)) => Arc::ptr_eq(old, new),
            (None, None) => true,
            _ => false,
        };
        if self.cwd != cwd || !same_runner {
            // Dropping the receiver also prevents already-completed old-session results leaking in.
            self.cancel();
            self.cwd = cwd.to_path_buf();
            self.runner = runner;
            self.lines.clear();
            self.refs.clear();
            self.fingerprint.clear();
            self.selected = None;
            self.error = None;
            self.action_status = None;
            self.picker = false;
            self.scroll.set(0);
            self.picker_scroll.set(0);
            self.next_poll = Instant::now();
            self.hide();
        }
        self.enabled = enabled && self.runner.is_some();
        if !self.enabled {
            self.hide();
            return;
        }
        if let Some(receiver) = self.result.as_mut() {
            match receiver.try_recv() {
                Ok(result) => {
                    self.result = None;
                    self.task = None;
                    self.action = None;
                    match result {
                        Ok(Some(snapshot)) => {
                            self.fingerprint = snapshot.fingerprint;
                            self.branch = snapshot.branch;
                            self.refs = snapshot.refs;
                            if let Some(lines) = snapshot.lines {
                                self.lines = lines;
                            }
                            self.error = None;
                            if snapshot.action_status.is_some() {
                                self.action_status = snapshot.action_status;
                            }
                            if self
                                .selected
                                .as_ref()
                                .is_some_and(|selected| !self.refs.contains(selected))
                            {
                                self.selected = None;
                                self.fingerprint.clear();
                            }
                        }
                        Ok(None) => {
                            self.lines.clear();
                            self.refs.clear();
                            self.fingerprint.clear();
                            self.hide();
                        }
                        Err(error) => self.error = Some(error),
                    }
                    self.next_poll = Instant::now() + POLL;
                    self.frame.schedule_frame_in(POLL);
                }
                Err(oneshot::error::TryRecvError::Empty) => {}
                Err(oneshot::error::TryRecvError::Closed) => {
                    self.cancel();
                    self.next_poll = Instant::now() + POLL;
                    self.frame.schedule_frame_in(POLL);
                }
            }
        }
        if self.result.is_none() && Instant::now() >= self.next_poll {
            self.start(None);
        }
    }

    fn start(&mut self, action: Option<GitAction>) {
        let Some(runner) = self.runner.clone() else {
            return;
        };
        if action.is_some() && self.action.is_none() {
            self.cancel();
        }
        if self.result.is_some() {
            return;
        }
        if action == Some(GitAction::Update) {
            // Updating always targets the checkout, not a branch selected for inspection.
            self.selected = None;
            self.picker = false;
            self.scroll.set(0);
        }
        let cwd = self.cwd.clone();
        let selected = self.selected.clone();
        let fingerprint = self.fingerprint.clone();
        let frame = self.frame.clone();
        let (tx, rx) = oneshot::channel();
        self.result = Some(rx);
        self.action = action;
        if action.is_some() {
            self.action_status = None;
            self.error = None;
        }
        self.task = Some(tokio::spawn(async move {
            let result = read_snapshot(runner, cwd, selected, fingerprint, action).await;
            let _ = tx.send(result);
            frame.schedule_frame();
        }));
    }

    fn status(&self) -> Option<(&str, &str, Color)> {
        match self.action {
            Some(GitAction::Fetch) => Some(("RUNNING", "fetching…", Color::Yellow)),
            Some(GitAction::Update) => Some(("RUNNING", "updating HEAD…", Color::Yellow)),
            None => {
                if let Some(error) = self.error.as_deref() {
                    Some(("ERROR", error, Color::Red))
                } else {
                    self.action_status.as_deref().map(|status| {
                        if status.starts_with("fetch failed") || status.starts_with("update failed")
                        {
                            ("FAILED", status, Color::Red)
                        } else {
                            ("OK", status, Color::Green)
                        }
                    })
                }
            }
        }
    }

    fn chrome_height(&self) -> u16 {
        1 + u16::from(self.status().is_some())
    }

    pub(super) fn mouse(&mut self, event: MouseEvent) -> bool {
        let area = self.area.get();
        if !self.enabled || area.is_empty() {
            self.resize_drag.set(None);
            return false;
        }
        // Capture moves and release outside the panel as its top edge moves upward.
        if let Some(drag) = self.resize_drag.get() {
            match event.kind {
                MouseEventKind::Drag(MouseButton::Left) => {
                    self.visible_rows = (i32::from(drag.rows) + i32::from(drag.start_row)
                        - i32::from(event.row))
                    .clamp(i32::from(MIN_ROWS), i32::from(drag.max_rows))
                        as u16;
                    self.frame.schedule_frame();
                    return true;
                }
                MouseEventKind::Up(MouseButton::Left) => {
                    self.resize_drag.set(None);
                    return true;
                }
                MouseEventKind::Down(_) => self.resize_drag.set(None),
                _ => return true,
            }
        }
        if event.column < area.x
            || event.column >= area.right()
            || event.row < area.y
            || event.row >= area.bottom()
        {
            return false;
        }
        let chrome_height = self.chrome_height();
        let visible = usize::from(area.height.saturating_sub(chrome_height));
        let (scroll, count) = if self.picker {
            (&self.picker_scroll, picker_entries(&self.refs).len())
        } else {
            (&self.scroll, self.lines.len())
        };
        match event.kind {
            MouseEventKind::ScrollDown => {
                scroll.set((scroll.get() + 3).min(count.saturating_sub(visible)))
            }
            MouseEventKind::ScrollUp => scroll.set(scroll.get().saturating_sub(3)),
            MouseEventKind::Down(MouseButton::Left) if event.row == area.y => {
                let x = event.column - area.x;
                if x < 4 {
                    self.collapsed = !self.collapsed;
                    self.picker = false;
                } else if x < 14 {
                    self.picker = !self.picker;
                    self.collapsed = false;
                } else if x < 22 {
                    self.start(Some(GitAction::Fetch));
                } else if x < 31 {
                    self.start(Some(GitAction::Update));
                } else if !self.collapsed {
                    self.resize_drag.set(Some(ResizeDrag {
                        start_row: event.row,
                        // Start with the rendered size, including when the terminal clipped it.
                        rows: area.height.saturating_sub(chrome_height),
                        max_rows: area.bottom().saturating_sub(chrome_height).max(MIN_ROWS),
                    }));
                }
            }
            MouseEventKind::Down(MouseButton::Left)
                if self.picker
                    && self.action.is_none()
                    && event.row >= area.y.saturating_add(chrome_height) =>
            {
                let index =
                    self.picker_scroll.get() + usize::from(event.row - area.y - chrome_height);
                let selection = match picker_entries(&self.refs).get(index) {
                    Some(PickerEntry::FollowHead) => Some(None),
                    Some(PickerEntry::Branch(reference)) => Some(Some((*reference).to_owned())),
                    Some(PickerEntry::Group(_)) | None => None,
                };
                if let Some(selected) = selection {
                    self.selected = selected;
                    self.picker = false;
                    self.scroll.set(0);
                    self.fingerprint.clear();
                    self.cancel();
                    self.start(None);
                }
            }
            _ => {}
        }
        self.frame.schedule_frame();
        true
    }
}

impl Drop for GitGraphPanel {
    fn drop(&mut self) {
        self.cancel();
    }
}

impl Renderable for GitGraphPanel {
    fn desired_height(&self, _width: u16) -> u16 {
        if !self.enabled || (self.lines.is_empty() && self.status().is_none()) {
            0
        } else if self.collapsed || self.lines.is_empty() {
            self.chrome_height()
        } else {
            self.visible_rows.saturating_add(self.chrome_height())
        }
    }
    fn render(&self, area: Rect, buf: &mut Buffer) {
        if area.is_empty() || self.desired_height(area.width) == 0 {
            self.hide();
            return;
        }
        self.area.set(area);
        // The label belongs to the displayed snapshot, not an in-flight selection.
        let branch = &self.branch;
        let header = format!(
            "[{}] [branch]  [fetch] [update] ↕ GitTree · {}",
            if self.collapsed { "+" } else { "−" },
            clean(branch)
        );
        Paragraph::new(Line::styled(header, Style::default().fg(Color::Cyan)))
            .render(Rect::new(area.x, area.y, area.width, 1), buf);
        if let Some((label, message, color)) = self.status()
            && area.height > 1
        {
            Paragraph::new(format!(" [{label}] {}", clean(message)))
                .style(
                    Style::default()
                        .fg(Color::Black)
                        .bg(color)
                        .add_modifier(Modifier::BOLD),
                )
                .render(Rect::new(area.x, area.y + 1, area.width, 1), buf);
        }
        let chrome_height = self.chrome_height();
        let height = area.height.saturating_sub(chrome_height);
        if self.collapsed || height == 0 {
            return;
        }
        let body = Rect::new(area.x, area.y + chrome_height, area.width, height);
        if self.picker {
            let entries = picker_entries(&self.refs);
            let offset = self
                .picker_scroll
                .get()
                .min(entries.len().saturating_sub(usize::from(height)));
            self.picker_scroll.set(offset);
            Paragraph::new(
                entries
                    .into_iter()
                    .skip(offset)
                    .take(usize::from(height))
                    .map(|entry| match entry {
                        PickerEntry::FollowHead => Line::raw("  Follow HEAD (current checkout)"),
                        PickerEntry::Group(label) => Line::styled(
                            format!("  {label}"),
                            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
                        ),
                        PickerEntry::Branch(reference) => {
                            let short = reference
                                .strip_prefix("refs/heads/")
                                .or_else(|| reference.strip_prefix("refs/remotes/"))
                                .unwrap_or(reference);
                            Line::raw(format!("    {}", clean(short)))
                        }
                    })
                    .collect::<Vec<_>>(),
            )
            .render(body, buf);
        } else {
            let offset = self
                .scroll
                .get()
                .min(self.lines.len().saturating_sub(usize::from(height)));
            self.scroll.set(offset);
            let author_width = self
                .lines
                .iter()
                .filter(|row| !row.author.is_empty())
                .map(|row| row.author.width() + 2)
                .max()
                .unwrap_or(0)
                .min(20);
            // Use the displayed snapshot's selection, never an in-flight picker request.
            let selected = self
                .branch
                .starts_with("refs/")
                .then_some(self.branch.as_str());
            let rows = usize::from(height).min(self.lines.len().saturating_sub(offset));
            let main_width = if body.width >= 64 {
                body.width - 32
            } else {
                body.width
            };
            let columns = self
                .lines
                .iter()
                .map(|row| row.lanes.trim_end().chars().count())
                .max()
                .unwrap_or(0)
                .min(usize::from(main_width));
            let neighbors = (0..rows + 2)
                .map(|index| {
                    offset
                        .checked_add(index)
                        .and_then(|index| index.checked_sub(1))
                        .and_then(|index| self.lines.get(index))
                })
                .collect::<Vec<_>>();
            let pixels = crate::git_graph_image::submit(
                neighbors
                    .iter()
                    .map(|row| row.map(|row| row.lanes.clone()).unwrap_or_default())
                    .collect(),
                neighbors
                    .iter()
                    .map(|row| row.is_some_and(|row| row.head))
                    .collect(),
                Rect::new(body.x, body.y, columns as u16, rows as u16),
            );
            for (index, row) in self
                .lines
                .iter()
                .skip(offset)
                .take(usize::from(height))
                .enumerate()
            {
                let row_area = Rect::new(body.x, body.y + index as u16, body.width, 1);
                let background = if (offset + index) % 2 == 1 {
                    crate::terminal_palette::default_bg()
                        .map(crate::style::user_message_bg)
                        .unwrap_or(Color::Rgb(40, 40, 40))
                } else {
                    Color::Reset
                };
                buf.set_style(row_area, Style::default().bg(background));
                row.render_pixels(row_area, buf, author_width, selected, pixels);
            }
        }
    }
}

fn clean(value: &str) -> String {
    value.chars().filter(|c| !c.is_control()).collect()
}

#[derive(Clone)]
struct GraphRow {
    lanes: String,
    hash: Option<String>,
    author: String,
    subject: String,
    refs: Vec<String>,
    head: bool,
}

impl GraphRow {
    #[cfg(test)]
    fn main_line(&self, author_width: usize) -> Line<'static> {
        self.main_line_pixels(author_width, false)
    }

    fn main_line_pixels(&self, author_width: usize, pixels: bool) -> Line<'static> {
        let lanes = if self.hash.is_some() {
            format!("{}    ", self.lanes.trim_end())
        } else {
            self.lanes.clone()
        };
        let mut spans = lanes
            .chars()
            .enumerate()
            .map(|(column, c)| {
                let mut style = Style::default().fg(PALETTE[(column / 2) % PALETTE.len()]);
                if c == '●' {
                    style = style.add_modifier(Modifier::BOLD);
                    if self.head {
                        style = style.add_modifier(Modifier::REVERSED);
                    }
                }
                Span::styled(
                    if pixels {
                        " ".to_owned()
                    } else {
                        c.to_string()
                    },
                    if pixels { Style::default() } else { style },
                )
            })
            .collect::<Vec<_>>();
        let style = if self.head {
            Style::default().add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };
        if let Some(hash) = &self.hash {
            spans.push(Span::styled(hash.clone(), style.fg(Color::Yellow)));
            if !self.author.is_empty() {
                let author = middle_truncate(&format!("[{}]", self.author), author_width);
                let padding = " ".repeat(author_width.saturating_sub(author.width()));
                spans.push(Span::styled(
                    format!(" {author}{padding}"),
                    style.fg(Color::Rgb(135, 255, 215)),
                ));
            }
            if !self.subject.is_empty() {
                spans.push(Span::styled(format!(" {}", self.subject), style));
            }
        }
        Line::from(spans)
    }

    fn badge(&self, selected: Option<&str>, width: usize) -> String {
        if width == 0 {
            return String::new();
        }
        let short = selected.map(|reference| {
            reference
                .strip_prefix("refs/heads/")
                .or_else(|| reference.strip_prefix("refs/remotes/"))
                .unwrap_or(reference)
        });
        let refs = self
            .refs
            .iter()
            .filter(|reference| {
                Some(reference.as_str()) != selected && Some(reference.as_str()) != short
            })
            .collect::<Vec<_>>();
        let head = if self.head && width >= 6 {
            "[HEAD]"
        } else {
            ""
        };
        let Some(first) = refs.first() else {
            return head.into();
        };
        let suffix = if refs.len() > 1 {
            format!(" +{}", refs.len() - 1)
        } else {
            String::new()
        };
        let prefix = if head.is_empty() {
            String::new()
        } else {
            format!("{head} ")
        };
        let available = width.saturating_sub(prefix.width() + suffix.width() + 2);
        if available == 0 {
            return head.into();
        }
        format!("{prefix}[{}]{suffix}", middle_truncate(first, available))
    }

    #[cfg(test)]
    fn render(&self, area: Rect, buf: &mut Buffer, author_width: usize, selected: Option<&str>) {
        self.render_pixels(area, buf, author_width, selected, false);
    }

    fn render_pixels(
        &self,
        area: Rect,
        buf: &mut Buffer,
        author_width: usize,
        selected: Option<&str>,
        pixels: bool,
    ) {
        let (main_width, badge_width) = if area.width >= 64 {
            (area.width - 32, 30)
        } else {
            (area.width, 0)
        };
        Paragraph::new(self.main_line_pixels(author_width, pixels))
            .render(Rect::new(area.x, area.y, main_width, 1), buf);
        if badge_width > 0 {
            let mut style = Style::default().fg(Color::Green);
            if self.head {
                style = style.add_modifier(Modifier::BOLD);
            }
            Paragraph::new(Line::styled(
                self.badge(selected, usize::from(badge_width)),
                style,
            ))
            .alignment(Alignment::Right)
            .render(
                Rect::new(area.right() - badge_width, area.y, badge_width, 1),
                buf,
            );
        }
    }
}

fn middle_truncate(text: &str, width: usize) -> String {
    let size = text.width();
    if size <= width {
        return text.into();
    }
    if width == 0 {
        return String::new();
    }
    let left = width / 2;
    let right = width - left - 1;
    format!(
        "{}…{}",
        cell_slice(text, 0, left),
        cell_slice(text, size - right, right)
    )
}

fn cell_slice(text: &str, start: usize, width: usize) -> String {
    let mut result = String::new();
    let mut position = 0;
    for glyph in text.graphemes(true) {
        let end = position + glyph.width();
        if position >= start + width {
            break;
        }
        if end > start {
            if position >= start && end <= start + width {
                result.push_str(glyph);
            } else {
                result.push_str(&" ".repeat(end.min(start + width) - position.max(start)));
            }
        }
        position = end;
    }
    result
}

fn graph_line(value: &str) -> GraphRow {
    let mut fields = value.splitn(5, '\u{1f}');
    let lanes = clean(fields.next().unwrap_or_default())
        .chars()
        .map(|c| match c {
            '*' => '●',
            '|' => '│',
            '/' => '╱',
            '\\' => '╲',
            '_' | '-' => '─',
            other => other,
        })
        .collect();
    let hash = fields.next().map(clean);
    let decorations = fields.next().unwrap_or_default();
    let subject = clean(fields.next().unwrap_or_default());
    let author = clean(fields.next().unwrap_or_default());
    let mut head = false;
    let mut refs = Vec::new();
    for token in decorations.split(", ") {
        let token = clean(token.trim());
        let reference = if token == "HEAD" {
            head = true;
            continue;
        } else if let Some(reference) = token.strip_prefix("HEAD -> ") {
            head = true;
            reference
        } else {
            token.as_str()
        };
        if !reference.is_empty() && !refs.iter().any(|existing| existing == reference) {
            refs.push(reference.to_owned());
        }
    }
    GraphRow {
        lanes,
        hash,
        author,
        subject,
        refs,
        head,
    }
}

async fn read_snapshot(
    runner: WorkspaceCommandRunner,
    cwd: PathBuf,
    selected: Option<String>,
    previous: String,
    action: Option<GitAction>,
) -> Result<Option<Snapshot>, String> {
    let command = |args: Vec<String>, timeout| {
        WorkspaceCommand::new(
            ["git", "--no-pager", "-c", "core.hooksPath=/dev/null"]
                .into_iter()
                .map(str::to_owned)
                .chain(args),
        )
        .cwd(cwd.clone())
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GCM_INTERACTIVE", "Never")
        .env("GIT_ASKPASS", "true")
        .env("SSH_ASKPASS", "true")
        // Preserve core.sshCommand and inherited SSH identity/proxy configuration.
        .env("SSH_ASKPASS_REQUIRE", "force")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .timeout(timeout)
    };
    let args = |values: &[&str]| values.iter().map(|v| (*v).to_owned()).collect::<Vec<_>>();
    let mut action_status = None;
    if let Some(action) = action {
        let name = action.name();
        match runner
            .run(command(args(action.args()), Duration::from_secs(120)))
            .await
        {
            Ok(output) if output.success() => action_status = Some(format!("{name}: complete")),
            Ok(output) => {
                let detail = output
                    .stderr
                    .lines()
                    .find(|line| line.starts_with("fatal:") || line.starts_with("error:"))
                    .or_else(|| {
                        output
                            .stderr
                            .lines()
                            .rev()
                            .find(|line| !line.trim().is_empty())
                    });
                action_status = Some(match detail {
                    Some(detail) => format!("{name} failed: {}", clean(detail)),
                    None => format!("{name} failed (exit {})", output.exit_code),
                });
            }
            Err(error) => action_status = Some(format!("{name} failed: {error}")),
        }
    }
    let head = runner
        .run(command(
            args(&["rev-parse", "--verify", "HEAD"]),
            Duration::from_secs(5),
        ))
        .await
        .map_err(|e| e.to_string())?;
    if !head.success() {
        if previous.is_empty()
            || head.stderr.contains("not a git repository")
            || head.stderr.contains("Needed a single revision")
        {
            return Ok(None);
        }
        return Err("Git HEAD unavailable".into());
    }
    let refs = runner
        .run(command(
            args(&[
                "for-each-ref",
                "--format=%(refname)%09%(objectname)",
                "refs/heads/",
                "refs/remotes/",
                "refs/tags/",
            ]),
            Duration::from_secs(5),
        ))
        .await
        .map_err(|e| e.to_string())?;
    if !refs.success() {
        return Err("Git refs unavailable".into());
    }
    let branch = runner
        .run(command(
            args(&["symbolic-ref", "--quiet", "--short", "HEAD"]),
            Duration::from_secs(5),
        ))
        .await
        .map_err(|e| e.to_string())?;
    let branch = if branch.success() {
        clean(branch.stdout.trim())
    } else {
        "detached HEAD".into()
    };
    let references = refs
        .stdout
        .lines()
        .filter_map(|line| line.split_once('\t').map(|(name, _)| name.to_owned()))
        .filter(|name| valid_ref(name))
        .collect::<Vec<_>>();
    let selected = selected.filter(|name| references.contains(name));
    let fingerprint = format!("{}{}{}{:?}", head.stdout, refs.stdout, branch, selected);
    // Tags affect decorations even though the picker contains branches only.
    let lines = if action.is_some() || fingerprint != previous {
        let mut log = args(&[
            "log",
            "--graph",
            "--color=never",
            "--decorate=short",
            "--max-count=200",
            "--format=%x1f%h%x1f%D%x1f%s%x1f%aN",
        ]);
        log.push(selected.as_deref().unwrap_or("HEAD").to_owned());
        log.push("--".into());
        let output = runner
            .run(command(log, Duration::from_secs(10)))
            .await
            .map_err(|e| e.to_string())?;
        if !output.success() {
            return Err("Git graph unavailable".into());
        }
        Some(output.stdout.lines().map(graph_line).collect())
    } else {
        None
    };
    Ok(Some(Snapshot {
        fingerprint,
        branch: selected.unwrap_or(branch),
        refs: references,
        lines,
        action_status,
    }))
}

fn valid_ref(name: &str) -> bool {
    (name.starts_with("refs/heads/") || name.starts_with("refs/remotes/"))
        && !name.chars().any(|c| c.is_control() || c.is_whitespace())
        && !name.contains("..")
        && !name.contains("@{")
        && !name.contains(['~', '^', ':', '?', '*', '[', '\\'])
        && !name.ends_with('/')
        && !name.ends_with('.')
        && !name
            .split('/')
            .any(|part| part.is_empty() || part.starts_with('.') || part.ends_with(".lock"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drag_resizes_outside_panel_and_stops_on_release_or_hide() {
        let mut panel = GitGraphPanel::new(FrameRequester::test_dummy());
        panel.enabled = true;
        panel.lines = vec![graph_line("*"); 30];
        assert_eq!(panel.desired_height(80), 11);
        let area = Rect::new(0, 20, 80, 11);
        panel.render(area, &mut Buffer::empty(area));
        let mut event = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 40,
            row: 20,
            modifiers: crossterm::event::KeyModifiers::NONE,
        };
        assert!(panel.mouse(event));
        event.kind = MouseEventKind::Drag(MouseButton::Left);
        event.row = 15;
        event.column = 90;
        assert!(panel.mouse(event));
        assert_eq!(panel.visible_rows, 15);
        let enlarged = Rect::new(0, 15, 80, 16);
        panel.render(enlarged, &mut Buffer::empty(enlarged));
        assert!(panel.resize_drag.get().is_some());
        event.row = 0;
        assert!(panel.mouse(event));
        assert_eq!(panel.visible_rows, 30);
        event.row = 100;
        assert!(panel.mouse(event));
        assert_eq!(panel.visible_rows, MIN_ROWS);
        event.kind = MouseEventKind::Up(MouseButton::Left);
        assert!(panel.mouse(event));
        assert!(panel.resize_drag.get().is_none());
        event.kind = MouseEventKind::Drag(MouseButton::Left);
        assert!(!panel.mouse(event));
        assert_eq!(panel.visible_rows, MIN_ROWS);

        event.kind = MouseEventKind::Down(MouseButton::Left);
        event.column = 40;
        event.row = enlarged.y;
        assert!(panel.mouse(event));
        panel.hide();
        assert!(panel.resize_drag.get().is_none());
        panel.collapsed = true;
        assert_eq!(panel.desired_height(80), 1);
        panel.collapsed = false;
        assert_eq!(panel.desired_height(80), MIN_ROWS + 1);
    }

    #[test]
    fn clipped_header_click_preserves_height_and_stripes_follow_scroll() {
        let mut panel = GitGraphPanel::new(FrameRequester::test_dummy());
        panel.enabled = true;
        panel.lines = vec![graph_line("*\u{1f}abc123\u{1f}\u{1f}subject\u{1f}author"); 20];
        let area = Rect::new(0, 20, 80, 4);
        let mut buffer = Buffer::empty(area);
        panel.render(area, &mut buffer);
        let stripe = buffer[(79, 22)].bg;
        assert_ne!(stripe, Color::Reset);
        assert_eq!(buffer[(79, 21)].bg, Color::Reset);
        let mut event = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 40,
            row: 20,
            modifiers: crossterm::event::KeyModifiers::NONE,
        };
        panel.mouse(event);
        event.kind = MouseEventKind::Up(MouseButton::Left);
        panel.mouse(event);
        assert_eq!(panel.visible_rows, DEFAULT_ROWS);
        panel.scroll.set(1);
        panel.render(area, &mut buffer);
        assert_eq!(buffer[(79, 21)].bg, stripe);
        assert_eq!(buffer[(79, 22)].bg, Color::Reset);
    }

    #[tokio::test]
    async fn actions_preserve_ssh_configuration_and_report_success_or_failure() {
        use crate::workspace_command::WorkspaceCommandError;
        use crate::workspace_command::WorkspaceCommandExecutor;
        use crate::workspace_command::WorkspaceCommandOutput;
        use std::future::Future;
        use std::pin::Pin;

        struct Runner {
            action_exit: i32,
        }
        impl WorkspaceCommandExecutor for Runner {
            fn run(
                &self,
                command: WorkspaceCommand,
            ) -> Pin<
                Box<
                    dyn Future<Output = Result<WorkspaceCommandOutput, WorkspaceCommandError>>
                        + Send
                        + '_,
                >,
            > {
                assert!(!command.env.contains_key("GIT_SSH_COMMAND"));
                let (exit_code, stdout, stderr) = match command.argv[4].as_str() {
                    "fetch" => {
                        assert_eq!(&command.argv[4..], &["fetch", "--all"]);
                        assert_eq!(command.timeout, Duration::from_secs(120));
                        (self.action_exit, "", "remote: progress\nfatal: denied\n")
                    }
                    "pull" => {
                        assert_eq!(
                            &command.argv[4..],
                            &[
                                "pull",
                                "--ff-only",
                                "--no-rebase",
                                "--no-autostash",
                                "--tags",
                                "--force",
                            ]
                        );
                        assert_eq!(command.timeout, Duration::from_secs(120));
                        (self.action_exit, "", "remote: progress\nfatal: denied\n")
                    }
                    "rev-parse" => (0, "abc123\n", ""),
                    "for-each-ref" => (0, "refs/heads/main\tabc123\n", ""),
                    "symbolic-ref" => (0, "main\n", ""),
                    "log" => (
                        0,
                        "*\u{1f}abc123\u{1f}HEAD -> main\u{1f}subject\u{1f}author",
                        "",
                    ),
                    other => panic!("unexpected command: {other}"),
                };
                Box::pin(async move {
                    Ok(WorkspaceCommandOutput {
                        exit_code,
                        stdout: stdout.into(),
                        stderr: stderr.into(),
                    })
                })
            }
        }

        for (action, action_exit, expected) in [
            (GitAction::Fetch, 0, "fetch: complete"),
            (GitAction::Fetch, 128, "fetch failed: fatal: denied"),
            (GitAction::Update, 0, "update: complete"),
            (GitAction::Update, 128, "update failed: fatal: denied"),
        ] {
            let snapshot = read_snapshot(
                Arc::new(Runner { action_exit }),
                PathBuf::from("/workspace"),
                None,
                String::new(),
                Some(action),
            )
            .await
            .unwrap()
            .unwrap();
            assert_eq!(snapshot.action_status.as_deref(), Some(expected));
            assert_eq!(snapshot.lines.unwrap().len(), 1);
        }
    }

    #[test]
    fn action_status_has_a_separate_colored_row_even_when_collapsed() {
        let mut panel = GitGraphPanel::new(FrameRequester::test_dummy());
        panel.enabled = true;
        panel.branch = "a-very-long-branch-name-that-fills-the-header".into();
        panel.lines = vec![graph_line("*")];
        let area = Rect::new(0, 0, 48, 2);
        for (action, status, error, expected, color) in [
            (
                Some(GitAction::Fetch),
                None,
                None,
                "[RUNNING] fetching…",
                Color::Yellow,
            ),
            (
                Some(GitAction::Update),
                None,
                None,
                "[RUNNING] updating HEAD…",
                Color::Yellow,
            ),
            (
                None,
                Some("fetch: complete"),
                None,
                "[OK] fetch: complete",
                Color::Green,
            ),
            (
                None,
                Some("update: complete"),
                None,
                "[OK] update: complete",
                Color::Green,
            ),
            (
                None,
                Some("fetch failed: denied"),
                None,
                "[FAILED] fetch failed: denied",
                Color::Red,
            ),
            (
                None,
                Some("update failed: denied"),
                None,
                "[FAILED] update failed: denied",
                Color::Red,
            ),
            (
                None,
                Some("fetch failed (exit 128)"),
                None,
                "[FAILED] fetch failed (exit 128)",
                Color::Red,
            ),
            (
                None,
                Some("fetch: complete"),
                Some("connection lost"),
                "[ERROR] connection lost",
                Color::Red,
            ),
        ] {
            panel.action = action;
            panel.action_status = status.map(str::to_owned);
            panel.error = error.map(str::to_owned);
            for collapsed in [false, true] {
                panel.collapsed = collapsed;
                assert_eq!(panel.desired_height(48), if collapsed { 2 } else { 12 });
                let mut buffer = Buffer::empty(area);
                panel.render(area, &mut buffer);
                let header = buffer.content[..48]
                    .iter()
                    .map(|cell| cell.symbol())
                    .collect::<String>();
                let banner = buffer.content[48..]
                    .iter()
                    .map(|cell| cell.symbol())
                    .collect::<String>();
                assert!(!header.contains(expected));
                assert!(banner.contains(expected));
                assert_eq!(buffer[(47, 1)].bg, color);
                assert!(buffer[(1, 1)].modifier.contains(Modifier::BOLD));
            }
        }
        panel.lines.clear();
        panel.collapsed = false;
        assert_eq!(panel.desired_height(48), 2);
    }

    #[test]
    fn status_row_is_excluded_from_picker_clicks_and_resize_rows() {
        let mut panel = GitGraphPanel::new(FrameRequester::test_dummy());
        panel.enabled = true;
        panel.lines = vec![graph_line("*"); 20];
        panel.action_status = Some("fetch: complete".into());
        panel.refs = vec!["refs/heads/other".into()];
        panel.picker = true;
        let area = Rect::new(0, 20, 80, 12);
        panel.render(area, &mut Buffer::empty(area));
        let mut event = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 40,
            row: 20,
            modifiers: crossterm::event::KeyModifiers::NONE,
        };
        panel.mouse(event);
        event.kind = MouseEventKind::Drag(MouseButton::Left);
        event.row = 18;
        panel.mouse(event);
        assert_eq!(panel.visible_rows, 12);
        event.kind = MouseEventKind::Up(MouseButton::Left);
        panel.mouse(event);
        event.kind = MouseEventKind::Down(MouseButton::Left);
        event.row = 21;
        panel.mouse(event);
        assert!(panel.picker);
        assert!(panel.selected.is_none());
        event.row = 23;
        panel.mouse(event);
        assert!(panel.picker);
        assert!(panel.selected.is_none());
        event.row = 24;
        panel.mouse(event);
        assert_eq!(panel.selected.as_deref(), Some("refs/heads/other"));
    }

    #[tokio::test]
    async fn update_button_targets_checkout_and_blocks_overlapping_fetch() {
        use crate::workspace_command::WorkspaceCommandError;
        use crate::workspace_command::WorkspaceCommandExecutor;
        use crate::workspace_command::WorkspaceCommandOutput;
        use std::future::Future;
        use std::pin::Pin;

        struct PendingRunner;
        impl WorkspaceCommandExecutor for PendingRunner {
            fn run(
                &self,
                _command: WorkspaceCommand,
            ) -> Pin<
                Box<
                    dyn Future<Output = Result<WorkspaceCommandOutput, WorkspaceCommandError>>
                        + Send
                        + '_,
                >,
            > {
                Box::pin(std::future::pending())
            }
        }

        let mut panel = GitGraphPanel::new(FrameRequester::test_dummy());
        panel.runner = Some(Arc::new(PendingRunner));
        panel.enabled = true;
        panel.selected = Some("refs/remotes/origin/other".into());
        panel.lines = vec![graph_line("*")];
        panel.picker = true;
        let area = Rect::new(3, 2, 80, 4);
        panel.render(area, &mut Buffer::empty(area));
        let mut click = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: area.x + 29, // Closing bracket of [update].
            row: area.y,
            modifiers: crossterm::event::KeyModifiers::NONE,
        };
        assert!(panel.mouse(click));
        assert!(panel.action == Some(GitAction::Update));
        assert!(panel.selected.is_none());
        assert!(!panel.picker);
        click.column = area.x + 14;
        assert!(panel.mouse(click));
        assert!(panel.action == Some(GitAction::Update));
    }

    #[test]
    fn parses_graph_fields_and_preserves_merge_lanes() {
        let line = graph_line("* |\u{1f}abc123\u{1f}HEAD -> main\u{1f}hello\u{1b}\u{1f}author\t");
        let text = line
            .main_line(8)
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert_eq!(text, "● │    abc123 [author] hello");
        assert_eq!(line.badge(None, 30), "[HEAD] [main]");
        let merge = graph_line("|\\  ");
        assert_eq!(
            merge
                .main_line(0)
                .spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>(),
            "│╲  "
        );
    }

    #[test]
    fn badges_hide_on_narrow_rows_and_exclude_selected_branch() {
        let row = graph_line(
            "*\u{1f}abc123\u{1f}HEAD -> main, origin/main, tag: v1\u{1f}subject\u{1f}author",
        );
        assert_eq!(
            row.badge(Some("refs/heads/main"), 30),
            "[HEAD] [origin/main] +1"
        );
        let area = Rect::new(0, 0, 80, 1);
        let mut buffer = Buffer::empty(area);
        row.render(area, &mut buffer, 8, Some("refs/heads/main"));
        let text = buffer
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(text.starts_with("●    abc123 [author] subject"));
        assert!(text.ends_with("[HEAD] [origin/main] +1"));
        let narrow = Rect::new(0, 0, 63, 1);
        let mut buffer = Buffer::empty(narrow);
        row.render(narrow, &mut buffer, 8, None);
        assert!(
            !buffer
                .content
                .iter()
                .map(|cell| cell.symbol())
                .collect::<String>()
                .contains("[HEAD]")
        );
    }

    #[test]
    fn authors_align_by_terminal_cells_with_middle_truncation() {
        let row = graph_line("*\u{1f}abc\u{1f}\u{1f}message\u{1f}가나다라마바사아자차카타");
        let line = row.main_line(20);
        let author = line
            .spans
            .iter()
            .find(|span| span.content.starts_with(" ["))
            .expect("author span");
        assert_eq!(author.content.width(), 21);
        assert!(author.content.contains('…'));
        assert!(author.content.ends_with(']'));
        let short = graph_line("*\u{1f}abc\u{1f}\u{1f}message\u{1f}A").main_line(20);
        let short_author = short
            .spans
            .iter()
            .find(|span| span.content.starts_with(" ["))
            .expect("author span");
        assert_eq!(short_author.content.width(), author.content.width());
    }

    #[test]
    fn accepts_only_full_branch_refs() {
        for name in [
            "refs/heads/main",
            "refs/remotes/origin/main",
            "refs/heads/한글",
        ] {
            assert!(valid_ref(name));
        }
        for name in [
            "--all",
            "HEAD",
            "refs/tags/v1",
            "refs/heads/a..b",
            "refs/heads/a\n",
            "refs/heads/x.lock",
            "refs/heads/a^{commit}",
        ] {
            assert!(!valid_ref(name));
        }
    }

    #[test]
    fn pending_selection_keeps_snapshot_label_and_fetch_blocks_selection() {
        let mut panel = GitGraphPanel::new(FrameRequester::test_dummy());
        panel.enabled = true;
        panel.branch = "main".into();
        panel.selected = Some("refs/heads/other".into());
        panel.refs = vec!["refs/heads/other".into()];
        panel.lines = vec![graph_line("old snapshot")];
        let area = Rect::new(0, 0, 80, 4);
        let mut buffer = Buffer::empty(area);
        panel.render(area, &mut buffer);
        let header = buffer
            .content
            .iter()
            .take(80)
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(header.contains("main"));
        assert!(!header.contains("other"));
        panel.picker = true;
        panel.action = Some(GitAction::Fetch);
        panel.mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 1,
            row: 1,
            modifiers: crossterm::event::KeyModifiers::NONE,
        });
        assert_eq!(panel.selected.as_deref(), Some("refs/heads/other"));
        assert!(panel.action == Some(GitAction::Fetch));
    }

    #[test]
    fn clamps_scroll_and_rejects_hidden_hitboxes() {
        let mut panel = GitGraphPanel::new(FrameRequester::test_dummy());
        panel.enabled = true;
        panel.lines = (0..10).map(|n| graph_line(&n.to_string())).collect();
        panel.scroll.set(100);
        let area = Rect::new(0, 0, 40, 4);
        let mut buffer = Buffer::empty(area);
        panel.render(area, &mut buffer);
        assert_eq!(panel.scroll.get(), 7);
        assert!(panel.mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 1,
            row: 0,
            modifiers: crossterm::event::KeyModifiers::NONE
        }));
        assert_eq!(panel.desired_height(40), 1);
        panel.sync(Path::new("/different-workspace"), None, true);
        assert!(panel.lines.is_empty());
        assert_eq!(panel.desired_height(40), 0);
        assert!(panel.area.get().is_empty());
    }
}
