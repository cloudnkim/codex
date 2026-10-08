//! Cached workspace Git graph. Rendering never runs Git; explicit clicks alone fetch remotes.
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
const PALETTE: [Color; 6] = [
    Color::Indexed(81),
    Color::Indexed(114),
    Color::Indexed(215),
    Color::Indexed(177),
    Color::Indexed(209),
    Color::Indexed(117),
];

struct Snapshot {
    fingerprint: String,
    branch: String,
    refs: Vec<String>,
    lines: Option<Vec<GraphRow>>,
    fetch_error: Option<String>,
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
    enabled: bool,
    collapsed: bool,
    picker: bool,
    scroll: Cell<usize>,
    picker_scroll: Cell<usize>,
    area: Cell<Rect>,
    fetching: bool,
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
            enabled: false,
            collapsed: false,
            picker: false,
            scroll: Cell::new(0),
            picker_scroll: Cell::new(0),
            area: Cell::new(Rect::default()),
            fetching: false,
        }
    }

    fn cancel(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
        self.result = None;
        self.fetching = false;
    }

    pub(super) fn hide(&self) {
        self.area.set(Rect::default());
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
                    let fetching = self.fetching;
                    self.fetching = false;
                    match result {
                        Ok(Some(snapshot)) => {
                            self.fingerprint = snapshot.fingerprint;
                            self.branch = snapshot.branch;
                            self.refs = snapshot.refs;
                            if let Some(lines) = snapshot.lines {
                                self.lines = lines;
                            }
                            if fetching
                                || snapshot.fetch_error.is_some()
                                || self
                                    .error
                                    .as_ref()
                                    .is_none_or(|error| !error.starts_with("fetch:"))
                            {
                                self.error = snapshot.fetch_error;
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
            self.start(false);
        }
    }

    fn start(&mut self, fetch: bool) {
        let Some(runner) = self.runner.clone() else {
            return;
        };
        if fetch && !self.fetching {
            self.cancel();
        }
        if self.result.is_some() {
            return;
        }
        let cwd = self.cwd.clone();
        let selected = self.selected.clone();
        let fingerprint = self.fingerprint.clone();
        let frame = self.frame.clone();
        let (tx, rx) = oneshot::channel();
        self.result = Some(rx);
        self.fetching = fetch;
        self.task = Some(tokio::spawn(async move {
            let result = read_snapshot(runner, cwd, selected, fingerprint, fetch).await;
            let _ = tx.send(result);
            frame.schedule_frame();
        }));
    }

    pub(super) fn mouse(&mut self, event: MouseEvent) -> bool {
        let area = self.area.get();
        if !self.enabled
            || area.is_empty()
            || event.column < area.x
            || event.column >= area.right()
            || event.row < area.y
            || event.row >= area.bottom()
        {
            return false;
        }
        let visible = usize::from(area.height.saturating_sub(1));
        let (scroll, count) = if self.picker {
            (&self.picker_scroll, self.refs.len() + 1)
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
                    self.start(true);
                }
            }
            MouseEventKind::Down(MouseButton::Left) if self.picker && !self.fetching => {
                let index = self.picker_scroll.get() + usize::from(event.row - area.y - 1);
                if index <= self.refs.len() {
                    self.selected = index
                        .checked_sub(1)
                        .and_then(|index| self.refs.get(index))
                        .cloned();
                    self.picker = false;
                    self.scroll.set(0);
                    self.fingerprint.clear();
                    self.cancel();
                    self.start(false);
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
        if !self.enabled || self.lines.is_empty() {
            0
        } else if self.collapsed {
            1
        } else {
            8
        }
    }
    fn render(&self, area: Rect, buf: &mut Buffer) {
        self.hide();
        if area.is_empty() || self.desired_height(area.width) == 0 {
            return;
        }
        self.area.set(area);
        // The label belongs to the displayed snapshot, not an in-flight selection.
        let branch = &self.branch;
        let state = if self.fetching {
            "fetching…"
        } else {
            self.error.as_deref().unwrap_or("")
        };
        let header = format!(
            "[{}] [branch]  [fetch] GitTree · {} {}",
            if self.collapsed { "+" } else { "−" },
            clean(branch),
            clean(state)
        );
        Paragraph::new(Line::styled(header, Style::default().fg(Color::Cyan)))
            .render(Rect::new(area.x, area.y, area.width, 1), buf);
        let height = area.height.saturating_sub(1);
        if self.collapsed || height == 0 {
            return;
        }
        let body = Rect::new(area.x, area.y + 1, area.width, height);
        if self.picker {
            let mut entries = vec![Line::raw("  Follow HEAD (current checkout)")];
            entries.extend(
                self.refs
                    .iter()
                    .map(|reference| Line::raw(format!("  {}", clean(reference)))),
            );
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
            for (index, row) in self
                .lines
                .iter()
                .skip(offset)
                .take(usize::from(height))
                .enumerate()
            {
                row.render(
                    Rect::new(body.x, body.y + index as u16, body.width, 1),
                    buf,
                    author_width,
                    selected,
                );
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
    fn main_line(&self, author_width: usize) -> Line<'static> {
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
                Span::styled(c.to_string(), style)
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
                    style.fg(Color::DarkGray),
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

    fn render(&self, area: Rect, buf: &mut Buffer, author_width: usize, selected: Option<&str>) {
        let (main_width, badge_width) = if area.width >= 64 {
            (area.width - 32, 30)
        } else {
            (area.width, 0)
        };
        Paragraph::new(self.main_line(author_width))
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
    fetch: bool,
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
        .env("GIT_SSH_COMMAND", "ssh -oBatchMode=yes")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .timeout(timeout)
    };
    let args = |values: &[&str]| values.iter().map(|v| (*v).to_owned()).collect::<Vec<_>>();
    let mut fetch_error = None;
    if fetch {
        match runner
            .run(command(args(&["fetch", "--all"]), Duration::from_secs(30)))
            .await
        {
            Ok(output) if output.success() => {}
            Ok(output) => {
                fetch_error = Some(format!(
                    "fetch: {}",
                    clean(output.stderr.lines().next().unwrap_or("failed"))
                ))
            }
            Err(error) => fetch_error = Some(format!("fetch: {error}")),
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
    let lines = if fetch || fingerprint != previous {
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
        fetch_error,
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
        panel.fetching = true;
        panel.mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 1,
            row: 1,
            modifiers: crossterm::event::KeyModifiers::NONE,
        });
        assert_eq!(panel.selected.as_deref(), Some("refs/heads/other"));
        assert!(panel.fetching);
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
