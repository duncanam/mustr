//! Terminal-native dashboard. Default foreground/background always come from the terminal.

use std::{
    io::{self, IsTerminal},
    time::Duration,
};

use color_eyre::eyre::{Result, ensure};
use crossterm::event::{Event, EventStream, KeyCode, KeyEventKind, KeyModifiers};
use futures_util::StreamExt;
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Cell, Paragraph, Row, Table, Tabs, Wrap},
};

use crate::{
    protocol::*,
    service::{Service, now},
};

#[derive(Clone, Default)]
struct View {
    selected: Option<SpaceId>,
    initial_space: Option<SpaceId>,
    scroll: u16,
    help: bool,
    notice: String,
}

impl View {
    fn in_space(space: SpaceId) -> Self {
        Self {
            initial_space: Some(space),
            ..Self::default()
        }
    }

    fn reconcile(&mut self, snapshot: &Snapshot) {
        if !snapshot
            .spaces
            .iter()
            .any(|s| Some(&s.id) == self.selected.as_ref())
        {
            // Keep the startup preference through the actor's initial empty
            // snapshot, consume it on the first populated topology only.
            self.selected = snapshot.spaces.first().map(|first| {
                self.initial_space
                    .take()
                    .filter(|id| snapshot.spaces.iter().any(|s| &s.id == id))
                    .unwrap_or_else(|| first.id.clone())
            });
            self.scroll = 0;
        }
    }

    fn navigate(&mut self, key: KeyCode, snapshot: &Snapshot) -> bool {
        match key {
            KeyCode::Tab | KeyCode::Right | KeyCode::Char('l') => self.advance(snapshot, true),
            KeyCode::BackTab | KeyCode::Left | KeyCode::Char('h') => self.advance(snapshot, false),
            KeyCode::Down | KeyCode::Char('j') => self.scroll = self.scroll.saturating_add(1),
            KeyCode::Up | KeyCode::Char('k') => self.scroll = self.scroll.saturating_sub(1),
            _ => return false,
        }
        true
    }

    fn advance(&mut self, snapshot: &Snapshot, forward: bool) {
        if snapshot.spaces.is_empty() {
            return;
        }
        let current = snapshot
            .spaces
            .iter()
            .position(|s| Some(&s.id) == self.selected.as_ref())
            .unwrap_or(0);
        let count = snapshot.spaces.len();
        let index = if forward {
            (current + 1) % count
        } else {
            (current + count - 1) % count
        };
        self.selected = Some(snapshot.spaces[index].id.clone());
        self.scroll = 0;
        self.notice.clear();
    }
}

pub(crate) async fn run(service: Service, initial_space: SpaceId, color: bool) -> Result<()> {
    ensure!(
        io::stdin().is_terminal() && io::stdout().is_terminal(),
        "dashboard needs a terminal; use mustr run --headless"
    );
    // color-eyre's panic hook is installed first; Ratatui wraps it to restore terminal state.
    let mut terminal = ratatui::try_init()?;
    let result = event_loop(&mut terminal, service, initial_space, color).await;
    let restored = ratatui::try_restore();
    result?;
    restored?;
    Ok(())
}

async fn event_loop(
    terminal: &mut ratatui::DefaultTerminal,
    service: Service,
    initial_space: SpaceId,
    color: bool,
) -> Result<()> {
    let mut events = EventStream::new();
    let mut snapshots = service.snapshots.clone();
    let mut view = View::in_space(initial_space);
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    loop {
        let snapshot = snapshots.borrow().clone();
        view.reconcile(&snapshot);
        terminal.draw(|frame| draw(frame, &snapshot, &mut view, color))?;
        tokio::select! {
            _ = tokio::signal::ctrl_c() => break,
            _ = terminate.recv() => break,
            _ = tick.tick() => {},
            changed = snapshots.changed() => { if changed.is_err() { break; } },
            event = events.next() => match event {
                Some(Ok(Event::Key(key))) if key.kind == KeyEventKind::Press => {
                    if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) { break; }
                    match key.code {
                        KeyCode::Char('q') => break,
                        KeyCode::Char('?') => view.help = !view.help,
                        KeyCode::Esc => view.help = false,
                        code if view.navigate(code, &snapshot) => {},
                        KeyCode::Char(key @ ('t' | 'c' | 'p')) => {
                            if let Some(space) = &view.selected {
                                let automation = snapshot.automation.get(space).cloned().unwrap_or_default();
                                let (mode, enabled) = match key { 't' => (Mode::Todo, !automation.todo_enabled), 'c' => (Mode::Checkin, !automation.checkin_enabled), _ => (Mode::Pr, !automation.pr_enabled) };
                                view.notice = match service.mode(space.clone(), mode, enabled).await {
                                    Ok(()) => format!("{mode:?} {} for this space; already-dispatched work is not cancelled", if enabled { "on" } else { "off" }),
                                    Err(error) => error.to_string(),
                                };
                            }
                        }
                        _ => {},
                    }
                }
                Some(Err(error)) => return Err(error.into()),
                None => break,
                _ => {},
            }
        }
    }
    Ok(())
}

fn clean(text: &str) -> String {
    text.chars().filter(|c| !c.is_control()).collect()
}
fn label<T: std::fmt::Debug>(value: T) -> String {
    format!("{value:?}").to_lowercase()
}
fn accent(color: bool) -> Style {
    if color {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default()
    }
}
fn muted() -> Style {
    Style::default().add_modifier(Modifier::DIM)
}

fn bold() -> Style {
    Style::default().add_modifier(Modifier::BOLD)
}

fn tone(color: bool, foreground: Color) -> Style {
    if color {
        Style::default().fg(foreground)
    } else {
        Style::default()
    }
}

fn height(lines: usize) -> u16 {
    u16::try_from(lines).unwrap_or(u16::MAX)
}

// Measure terminal columns rather than bytes, including for space labels from herdr.
fn elide(text: &str, width: u16) -> String {
    if Line::raw(text).width() <= usize::from(width) {
        return text.to_owned();
    }
    if width == 0 {
        return String::new();
    }
    let mut result = String::new();
    let mut used = 0;
    for ch in text.chars() {
        let columns = Span::raw(ch.to_string()).width();
        if used + columns >= usize::from(width) {
            break;
        }
        result.push(ch);
        used += columns;
    }
    result.push('…');
    result
}

fn mode_span(key: char, name: &str, on: bool, color: bool) -> Span<'static> {
    Span::styled(
        format!("[{key}] {name} {}  ", if on { "ON" } else { "off" }),
        if on {
            accent(color).add_modifier(Modifier::BOLD)
        } else {
            muted()
        },
    )
}

fn draw(frame: &mut Frame, snapshot: &Snapshot, view: &mut View, color: bool) {
    let area = frame.area();
    frame.render_widget(Block::default().style(Style::reset()), area);
    if area.width < 35 || area.height < 12 {
        frame.render_widget(
            Paragraph::new("mustr · enlarge terminal (35×12 minimum)\nq quits"),
            area,
        );
        return;
    }
    let area = Rect::new(area.x + 1, area.y, area.width - 2, area.height);
    let automation = view
        .selected
        .as_ref()
        .and_then(|space| snapshot.automation.get(space))
        .cloned()
        .unwrap_or_default();
    let mode_height = if view.selected.is_none() {
        1
    } else {
        u16::from(area.width < 60)
            + 1
            + u16::from(automation.todo.is_some() || automation.pr.is_some())
    };
    let [header, modes, _, body, footer] = Layout::vertical([
        Constraint::Length(2),
        Constraint::Length(mode_height),
        Constraint::Length(u16::from(area.height >= 18)),
        Constraint::Min(1),
        Constraint::Length(2),
    ])
    .areas(area);
    draw_header(frame, header, snapshot, view, color);
    if view.selected.is_some() {
        draw_modes(frame, modes, &automation, color);
    } else {
        frame.render_widget(
            Paragraph::new("herdr · waiting for spaces").style(muted()),
            modes,
        );
    }
    if view.help {
        draw_help(frame, body, color);
    } else if let Some(space) = view.selected.clone() {
        draw_space(frame, body, snapshot, &space, &automation, view, color);
    } else {
        frame.render_widget(
            Paragraph::new(vec![
                Line::styled("No herdr spaces yet.", bold()),
                Line::raw(""),
                Line::raw("Open a herdr space, then join from Pi with /role NAME."),
                Line::raw(""),
                Line::styled("No agent sessions are read or launched.", muted()),
            ])
            .wrap(Wrap { trim: false }),
            body,
        );
    }
    draw_footer(frame, footer, snapshot, view, color);
}

fn draw_header(frame: &mut Frame, area: Rect, snapshot: &Snapshot, view: &View, color: bool) {
    let [brand, navigation] = Layout::vertical([Constraint::Length(1); 2]).areas(area);
    let [name, context] = Layout::horizontal([
        Constraint::Min(1),
        Constraint::Length(if area.width >= 65 { 20 } else { 0 }),
    ])
    .areas(brand);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("mustr", accent(color).add_modifier(Modifier::BOLD)),
            Span::styled("  /  agent coordination", muted()),
        ])),
        name,
    );
    frame.render_widget(
        Paragraph::new(format!(
            "herdr · {} {}",
            snapshot.spaces.len(),
            if snapshot.spaces.len() == 1 {
                "space"
            } else {
                "spaces"
            }
        ))
        .style(muted())
        .right_aligned(),
        context,
    );
    let selected = snapshot
        .spaces
        .iter()
        .position(|s| Some(&s.id) == view.selected.as_ref());
    let count = selected.map_or_else(String::new, |index| {
        format!("{}/{}", index + 1, snapshot.spaces.len())
    });
    let [label, tabs, count_area] = Layout::horizontal([
        Constraint::Length(7),
        Constraint::Min(1),
        Constraint::Length(height(count.len() + 1)),
    ])
    .areas(navigation);
    frame.render_widget(Paragraph::new("spaces").style(muted()), label);
    frame.render_widget(
        Paragraph::new(count).style(muted()).right_aligned(),
        count_area,
    );
    if snapshot.spaces.is_empty() {
        frame.render_widget(Paragraph::new("—").style(muted()), tabs);
        return;
    }
    let titles: Vec<_> = snapshot
        .spaces
        .iter()
        .map(|space| {
            elide(
                &format!("{} · {}", clean(&space.label), clean(space.id.as_str())),
                tabs.width,
            )
        })
        .map(Line::raw)
        .collect();
    // Keep the selected tab in view even when there are more spaces than columns.
    let mut start = 0;
    let selected = selected.unwrap_or(0);
    while start < selected
        && titles[start..=selected]
            .iter()
            .map(Line::width)
            .sum::<usize>()
            + (selected - start) * 2
            > usize::from(tabs.width)
    {
        start += 1;
    }
    frame.render_widget(
        Tabs::new(titles.into_iter().skip(start))
            .select(selected - start)
            .padding("", "")
            .divider("  ")
            .highlight_style(accent(color).add_modifier(Modifier::BOLD | Modifier::UNDERLINED)),
        tabs,
    );
}

fn draw_modes(frame: &mut Frame, area: Rect, automation: &Automation, color: bool) {
    let todo = mode_span('t', "TODO", automation.todo_enabled, color);
    let checkin = mode_span('c', "Check-in", automation.checkin_enabled, color);
    let pr = mode_span('p', "PR", automation.pr_enabled, color);
    let mut lines = if area.width >= 60 {
        vec![Line::from(vec![
            Span::styled("auto  ", muted()),
            todo,
            checkin,
            pr,
        ])]
    } else {
        vec![Line::from(vec![todo, checkin]), Line::from(pr)]
    };
    let mut details = Vec::new();
    if let Some(todo) = &automation.todo {
        details.push(format!("TODO {}/{} open", todo.remaining, todo.total));
    }
    if let Some(pr) = &automation.pr {
        details.push(format!("PR poll {}s", pr.next_at.saturating_sub(now())));
    }
    if !details.is_empty() {
        lines.push(Line::styled(details.join(" · "), muted()));
    }
    frame.render_widget(Paragraph::new(lines), area);
}

// A single rule gives sections a hierarchy without framing empty space in big boxes.
fn section(frame: &mut Frame, area: Rect, name: &str, count: usize, hint: &str) -> Rect {
    let title = Line::from(vec![
        Span::styled(format!(" {name} "), bold()),
        Span::styled(format!("{count} "), muted()),
    ]);
    let mut block = Block::default()
        .borders(Borders::TOP)
        .border_style(muted())
        .title(title.clone());
    if !hint.is_empty() && title.width() + hint.len() + 4 < usize::from(area.width) {
        block = block.title(Line::styled(format!(" {hint} "), muted()).right_aligned());
    }
    let inner = block.inner(area);
    frame.render_widget(block, area);
    inner
}

fn draw_space(
    frame: &mut Frame,
    area: Rect,
    snapshot: &Snapshot,
    space: &SpaceId,
    automation: &Automation,
    view: &mut View,
    color: bool,
) {
    // Keep historical members in the snapshot for task/message name resolution,
    // but only connected agents belong in the active roster and its layout/count.
    let members: Vec<_> = snapshot
        .members
        .iter()
        .filter(|m| &m.space_id == space && m.activity != Activity::Offline)
        .collect();
    let tasks: Vec<_> = snapshot
        .tasks
        .iter()
        .filter(|t| &t.space_id == space)
        .collect();
    let [participants, work] = if area.width >= 96 && area.height >= 8 {
        let [participants, _, work] = Layout::horizontal([
            Constraint::Length((area.width / 3).clamp(29, 36)),
            Constraint::Length(3),
            Constraint::Min(1),
        ])
        .areas(area);
        [participants, work]
    } else {
        let wanted = if members.is_empty() {
            3
        } else {
            height(members.len() * 2 + 2)
        };
        let [participants, _, work] = Layout::vertical([
            Constraint::Length(wanted.min((area.height / 3).max(2))),
            Constraint::Length(u16::from(area.height >= 16)),
            Constraint::Min(2),
        ])
        .areas(area);
        [participants, work]
    };
    let task_height = if tasks.is_empty() {
        3
    } else {
        height(tasks.len() * usize::from(task_row_height(&tasks)) + 1)
    };
    let [task_area, _, messages] = Layout::vertical([
        Constraint::Length(task_height.min((work.height / 2).max(2))),
        Constraint::Length(u16::from(work.height >= 12)),
        Constraint::Min(2),
    ])
    .areas(work);
    draw_members(frame, participants, &members, color);
    draw_tasks(frame, task_area, snapshot, &tasks, color);
    draw_messages(frame, messages, snapshot, space, automation, view, color);
}

fn activity_style(activity: Activity, color: bool) -> Style {
    match activity {
        Activity::Working => accent(color).add_modifier(Modifier::BOLD),
        Activity::Paused => tone(color, Color::Yellow),
        Activity::Idle | Activity::Offline => muted(),
    }
}

fn task_style(state: TaskState, color: bool) -> Style {
    match state {
        TaskState::Blocked => tone(color, Color::Red).add_modifier(Modifier::BOLD),
        TaskState::Review | TaskState::Waiting => {
            tone(color, Color::Yellow).add_modifier(Modifier::BOLD)
        }
        TaskState::Active | TaskState::Assigned => accent(color).add_modifier(Modifier::BOLD),
        TaskState::Accepted => tone(color, Color::Green),
        TaskState::Paused | TaskState::Cancelled => muted(),
    }
}

// Reserve a line for an explicit overflow count instead of silently clipping the roster/tasks.
fn visible_rows(area: Rect, count: usize, row_height: u16) -> usize {
    let capacity = usize::from(area.height / row_height);
    if count > capacity {
        usize::from(area.height.saturating_sub(1) / row_height)
    } else {
        count
    }
}

fn overflow(
    frame: &mut Frame,
    area: Rect,
    visible: usize,
    count: usize,
    row_height: u16,
    noun: &str,
) {
    if visible < count && area.height > 0 {
        frame.render_widget(
            Paragraph::new(format!("+{} more {noun} · enlarge to see", count - visible))
                .style(muted()),
            Rect::new(area.x, area.y + height(visible) * row_height, area.width, 1),
        );
    }
}

fn draw_members(frame: &mut Frame, area: Rect, members: &[&Member], color: bool) {
    let inner = section(frame, area, "Participants", members.len(), "");
    if members.is_empty() {
        frame.render_widget(
            Paragraph::new(vec![
                Line::styled("No connected participants.", muted()),
                Line::raw("/role NAME in Pi to join."),
            ]),
            inner,
        );
        return;
    }
    let note_height = u16::from(inner.height >= 5);
    let rows_area = Rect::new(inner.x, inner.y, inner.width, inner.height - note_height);
    let row_height = if rows_area.height < 2 { 1 } else { 2 };
    let visible = visible_rows(rows_area, members.len(), row_height);
    let rows = members.iter().take(visible).map(|member| {
        Row::new([
            Cell::from(Text::from(vec![
                Line::styled(clean(&member.name), bold()),
                Line::styled(
                    format!("{} · {}", clean(&member.role), clean(&member.pane_id)),
                    muted(),
                ),
            ])),
            Cell::from(label(member.activity)).style(activity_style(member.activity, color)),
        ])
        .height(row_height)
    });
    frame.render_widget(
        Table::new(rows, [Constraint::Min(1), Constraint::Length(7)]).column_spacing(1),
        rows_area,
    );
    overflow(
        frame,
        rows_area,
        visible,
        members.len(),
        row_height,
        "participants",
    );
    if note_height > 0 {
        let used = (height(visible) * row_height + u16::from(visible < members.len()))
            .min(rows_area.height);
        frame.render_widget(
            Paragraph::new("Activity ≠ completion").style(muted()),
            Rect::new(inner.x, inner.y + used, inner.width, 1),
        );
    }
}

fn task_row_height(tasks: &[&Task]) -> u16 {
    if tasks.iter().any(|t| t.state == TaskState::Paused) {
        4
    } else {
        2
    }
}

fn draw_tasks(frame: &mut Frame, area: Rect, snapshot: &Snapshot, tasks: &[&Task], color: bool) {
    let inner = section(frame, area, "Tasks", tasks.len(), "");
    if tasks.is_empty() {
        frame.render_widget(
            Paragraph::new(vec![
                Line::styled("No tasks assigned.", muted()),
                Line::raw("A joined agent can assign work to an idle participant."),
            ])
            .wrap(Wrap { trim: false }),
            inner,
        );
        return;
    }
    let row_height = task_row_height(tasks).min(inner.height.max(1));
    let visible = visible_rows(inner, tasks.len(), row_height);
    let rows = tasks.iter().take(visible).map(|task| {
        let owner = snapshot
            .members
            .iter()
            .find(|m| m.id == task.owner)
            .map(|m| m.name.as_str())
            .unwrap_or(task.owner.as_str());
        let mut details = vec![
            Line::styled(clean(&task.title), bold()),
            Line::styled(
                format!(
                    "{} · {} r{}",
                    clean(owner),
                    clean(task.id.as_str()).chars().take(8).collect::<String>(),
                    task.revision
                ),
                muted(),
            ),
        ];
        if task.state == TaskState::Paused {
            let requester = snapshot
                .members
                .iter()
                .find(|m| m.id == task.requester)
                .map(|m| m.name.as_str())
                .unwrap_or(task.requester.as_str());
            details.push(Line::styled(
                elide(&clean(&task.note), inner.width.saturating_sub(10)),
                tone(color, Color::Yellow),
            ));
            details.push(Line::styled(
                format!("Ask {}: reconcile, then guide/cancel", clean(requester)),
                tone(color, Color::Yellow),
            ));
        }
        Row::new([
            Cell::from(label(task.state)).style(task_style(task.state, color)),
            Cell::from(Text::from(details)),
        ])
        .height(row_height)
    });
    frame.render_widget(
        Table::new(rows, [Constraint::Length(9), Constraint::Min(1)]).column_spacing(1),
        inner,
    );
    overflow(frame, inner, visible, tasks.len(), row_height, "tasks");
}

fn draw_messages(
    frame: &mut Frame,
    area: Rect,
    snapshot: &Snapshot,
    space: &SpaceId,
    automation: &Automation,
    view: &mut View,
    color: bool,
) {
    let messages: Vec<_> = snapshot
        .messages
        .iter()
        .filter(|m| &m.space_id == space)
        .collect();
    let inner = section(
        frame,
        area,
        "Messages",
        messages.len(),
        "addressed · newest first",
    );
    let mut lines = Vec::new();
    for (name, error) in [
        (
            "TODO",
            automation
                .todo
                .as_ref()
                .and_then(|todo| todo.error.as_deref()),
        ),
        (
            "PR",
            automation.pr.as_ref().and_then(|pr| pr.error.as_deref()),
        ),
    ] {
        if let Some(error) = error {
            lines.push(Line::styled(
                format!("! {name}: {}", clean(error)),
                tone(color, Color::Red),
            ));
        }
    }
    for (index, message) in messages.iter().enumerate() {
        if index > 0 {
            lines.push(Line::raw(""));
        }
        let from = message
            .from
            .as_ref()
            .and_then(|id| snapshot.members.iter().find(|m| &m.id == id))
            .map(|m| m.name.as_str())
            .unwrap_or("hub");
        let to = snapshot
            .members
            .iter()
            .find(|m| m.id == message.to)
            .map(|m| m.name.as_str())
            .unwrap_or(message.to.as_str());
        let delivery_style = match message.delivery {
            Delivery::Uncertain => tone(color, Color::Red),
            Delivery::Pending => tone(color, Color::Yellow),
            Delivery::Offered => accent(color),
            Delivery::Received | Delivery::Cancelled => muted(),
        };
        lines.push(Line::from(vec![
            Span::styled(format!("{} → {}", clean(from), clean(to)), bold()),
            Span::styled(format!(" · {} · ", label(message.kind)), muted()),
            Span::styled(
                if message.delivery == Delivery::Cancelled {
                    "cancelled notification (not task)".into()
                } else {
                    label(message.delivery)
                },
                delivery_style,
            ),
        ]));
        lines.push(Line::raw(format!("  {}", message_preview(&message.text))));
    }
    if messages.is_empty() {
        lines.push(Line::styled("No addressed messages yet.", bold()));
        lines.push(Line::styled(
            "Join from Pi with /role NAME, then ask an agent to coordinate.",
            muted(),
        ));
    }
    let paragraph = Paragraph::new(lines).wrap(Wrap { trim: false });
    // Use the renderer's actual wrapping (including Unicode), not byte counts.
    let max_scroll = height(
        paragraph
            .line_count(inner.width)
            .saturating_sub(usize::from(inner.height)),
    );
    view.scroll = view.scroll.min(max_scroll);
    frame.render_widget(paragraph.scroll((view.scroll, 0)), inner);
}

fn message_preview(text: &str) -> String {
    let text = clean(text);
    let mut chars = text.chars();
    let mut preview: String = chars.by_ref().take(240).collect();
    if chars.next().is_some() {
        preview.push_str("… [truncated]");
    }
    preview
}

fn draw_help(frame: &mut Frame, area: Rect, color: bool) {
    let controls = vec![
        Line::styled("Keyboard", accent(color).add_modifier(Modifier::BOLD)),
        Line::raw("Tab / → / l     next space"),
        Line::raw("Shift-Tab / ← / h  previous space"),
        Line::raw("↑ ↓ / k j       scroll messages"),
        Line::raw("t  TODO / continuation on/off"),
        Line::raw("c  periodic check-ins on/off"),
        Line::raw("p  PR watcher on/off"),
        Line::raw("? / Esc         close help"),
        Line::raw("q / Ctrl-C      stop hub"),
    ];
    let workflow = vec![
        Line::styled(
            "Connect your workflow",
            accent(color).add_modifier(Modifier::BOLD),
        ),
        Line::raw("Join each Pi session with /role NAME."),
        Line::raw("Attach from the intended Pi recipient:"),
        Line::raw("/mustr-todo [path/to/TODO.md]"),
        Line::raw("/mustr-pr https://github.com/OWNER/REPO/pull/N"),
        Line::raw(""),
        Line::styled("Opt-in, space-local automation", bold()),
        Line::raw(
            "Modes are off after hub restart. Turning a mode off cancels undelivered prompts, not already-running work.",
        ),
        Line::raw(""),
        Line::styled("Activity ≠ completion. Done ≠ accepted.", bold()),
        Line::raw("Stopping the hub leaves agents and external jobs running."),
    ];
    if area.width >= 76 {
        let [keys, _, workflow_area] = Layout::horizontal([
            Constraint::Length(34),
            Constraint::Length(3),
            Constraint::Min(1),
        ])
        .areas(area);
        frame.render_widget(Paragraph::new(controls).wrap(Wrap { trim: false }), keys);
        frame.render_widget(
            Paragraph::new(workflow).wrap(Wrap { trim: false }),
            workflow_area,
        );
    } else {
        let lines: Vec<_> = controls
            .into_iter()
            .chain([Line::raw("")])
            .chain(workflow)
            .collect();
        frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
    }
}

fn draw_footer(frame: &mut Frame, area: Rect, snapshot: &Snapshot, view: &View, color: bool) {
    let keys = if view.help {
        "? / Esc close help · q quit"
    } else if area.width < 60 {
        "Tab space ↑↓ msgs  ? help  q quit"
    } else if area.width < 96 {
        "Tab/←→ spaces · ↑↓/jk messages · ? help · q quit"
    } else {
        "Tab/←→/hl spaces   ↑↓/jk messages   t TODO  c Check-ins  p PR   ? help   q quit"
    };
    let error = snapshot.error.as_deref().filter(|s| !s.is_empty());
    let status = if let Some(error) = error {
        Line::styled(
            format!("! {}", clean(error)),
            tone(color, Color::Red).add_modifier(Modifier::BOLD),
        )
    } else if !view.notice.is_empty() {
        Line::raw(clean(&view.notice))
    } else if snapshot
        .tasks
        .iter()
        .any(|t| Some(&t.space_id) == view.selected.as_ref() && t.state == TaskState::Paused)
    {
        Line::styled(
            if area.width < 60 {
                "Paused task: ask requester to guide"
            } else {
                "Paused work: requester must reconcile then guide/cancel; automation will not resume it"
            },
            tone(color, Color::Yellow),
        )
    } else {
        Line::styled(
            if area.width < 60 {
                "Quit leaves agents open."
            } else {
                "Modes are opt-in · activity is not completion · quit leaves agents open"
            },
            muted(),
        )
    };
    frame.render_widget(
        Paragraph::new(vec![Line::styled(keys, muted()), status]),
        area,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend, buffer::Buffer};

    mod reliability;

    fn empty_space() -> Snapshot {
        Snapshot {
            spaces: vec![Space {
                id: SpaceId::new("w1"),
                label: "Orchard".into(),
            }],
            ..Snapshot::default()
        }
    }

    fn populated() -> Snapshot {
        let mut snapshot = empty_space();
        snapshot.spaces.push(Space {
            id: SpaceId::new("w2"),
            label: "Sandbox".into(),
        });
        snapshot.members = vec![
            Member {
                id: AgentId::new("ada"),
                name: "Ada".into(),
                role: "architect".into(),
                pane_id: "w1:p1".into(),
                space_id: SpaceId::new("w1"),
                activity: Activity::Idle,
                last_seen: 0,
                idle_since: 0,
            },
            Member {
                id: AgentId::new("lin"),
                name: "Lin".into(),
                role: "implementor".into(),
                pane_id: "w1:p2".into(),
                space_id: SpaceId::new("w1"),
                activity: Activity::Working,
                last_seen: 0,
                idle_since: 0,
            },
        ];
        snapshot.tasks = vec![
            Task {
                id: TaskId::new("a1b2c3d4-1111"),
                space_id: SpaceId::new("w1"),
                requester: AgentId::new("ada"),
                owner: AgentId::new("lin"),
                title: "Refine dashboard".into(),
                description: "Keep it compact.".into(),
                state: TaskState::Active,
                revision: 2,
                updated_at: 0,
                checkin_at: 0,
                nudges: 0,
                note: String::new(),
            },
            Task {
                id: TaskId::new("e5f6a7b8-2222"),
                space_id: SpaceId::new("w1"),
                requester: AgentId::new("lin"),
                owner: AgentId::new("ada"),
                title: "Review runner logs".into(),
                description: "Needs a decision.".into(),
                state: TaskState::Blocked,
                revision: 1,
                updated_at: 0,
                checkin_at: 0,
                nudges: 0,
                note: String::new(),
            },
        ];
        snapshot.messages = vec![
            Message {
                id: MessageId::new("message-1"),
                space_id: SpaceId::new("w1"),
                from: Some(AgentId::new("ada")),
                to: AgentId::new("lin"),
                task_id: Some(snapshot.tasks[0].id.clone()),
                revision: Some(2),
                kind: MessageKind::Assignment,
                text: "Keep the dashboard compact and terminal-native.".into(),
                wake: true,
                delivery: Delivery::Received,
                created_at: 0,
                pr_ack: None,
            },
            Message {
                id: MessageId::new("message-2"),
                space_id: SpaceId::new("w1"),
                from: None,
                to: AgentId::new("ada"),
                task_id: None,
                revision: None,
                kind: MessageKind::Info,
                text: "Waiting on an explicit review decision.".into(),
                wake: false,
                delivery: Delivery::Pending,
                created_at: 0,
                pr_ack: None,
            },
        ];
        snapshot.automation.insert(
            SpaceId::new("w1"),
            Automation {
                todo_enabled: true,
                todo: Some(crate::watchers::TodoWatch {
                    path: "TODO.md".into(),
                    driver: AgentId::new("lin"),
                    total: 5,
                    remaining: 2,
                    next: "Refine dashboard".into(),
                    signature: String::new(),
                    nudges: 0,
                    next_at: 0,
                    error: None,
                }),
                pr: Some(crate::watchers::PrWatch {
                    url: "https://github.com/example/project/pull/42".into(),
                    cwd: ".".into(),
                    driver: AgentId::new("ada"),
                    branch: None,
                    head: None,
                    baseline: None,
                    next_at: 0,
                    error: None,
                }),
                ..Automation::default()
            },
        );
        snapshot
    }

    fn selected_view(snapshot: &Snapshot) -> View {
        let mut view = View::default();
        view.reconcile(snapshot);
        view
    }

    fn render(snapshot: &Snapshot, view: &View, width: u16, height: u16, color: bool) -> Buffer {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        let mut view = view.clone();
        terminal
            .draw(|frame| draw(frame, snapshot, &mut view, color))
            .unwrap();
        terminal.backend().buffer().clone()
    }

    fn screen(buffer: &Buffer) -> String {
        (0..buffer.area.height)
            .map(|y| {
                let line: String = (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect();
                line.trim_end().to_owned()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn position(screen: &str, text: &str) -> (usize, usize) {
        screen
            .lines()
            .enumerate()
            .find_map(|(row, line)| {
                line.find(text)
                    .map(|column| (Line::raw(&line[..column]).width(), row))
            })
            .unwrap_or_else(|| panic!("missing {text:?} in:\n{screen}"))
    }

    #[test]
    fn empty_space_is_compact_and_actionable_without_empty_boxes() {
        let snapshot = empty_space();
        let text = screen(&render(&snapshot, &selected_view(&snapshot), 100, 24, true));
        assert!(text.contains("mustr  /  agent coordination"));
        assert!(text.contains("Orchard · w1"));
        assert!(text.contains("[t] TODO off  [c] Check-in off  [p] PR off"));
        assert!(text.contains("/role NAME in Pi to join."));
        assert!(text.contains("No tasks assigned."));
        assert!(text.contains("No addressed messages yet."));
        let (_, roster_row) = position(&text, "Participants 0");
        let (_, task_row) = position(&text, "Tasks 0");
        let (_, message_row) = position(&text, "Messages 0");
        assert_eq!(roster_row, task_row);
        assert_eq!(message_row - task_row, 4);
        assert!(!text.contains(['│', '┌', '┐', '└', '┘']));
    }

    #[test]
    fn no_spaces_explains_waiting_instead_of_showing_empty_tables() {
        let snapshot = Snapshot::default();
        let text = screen(&render(&snapshot, &View::default(), 80, 24, true));
        for expected in [
            "No herdr spaces yet.",
            "Open a herdr space",
            "/role NAME",
            "No agent sessions are read or launched.",
        ] {
            assert!(text.contains(expected), "missing {expected:?}:\n{text}");
        }
        assert!(!text.contains("Participants"));
        assert!(!text.contains("Tasks"));
    }

    #[test]
    fn populated_wide_layout_preserves_the_status_and_message_hierarchy() {
        let snapshot = populated();
        let buffer = render(&snapshot, &selected_view(&snapshot), 120, 30, true);
        let text = screen(&buffer);
        for (status, foreground) in [
            ("working", Color::Cyan),
            ("active", Color::Cyan),
            ("blocked", Color::Red),
        ] {
            let (x, y) = position(&text, status);
            assert_eq!(buffer[(height(x), height(y))].fg, foreground);
        }
        for expected in [
            "Ada",
            "Lin",
            "architect · w1:p1",
            "implementor · w1:p2",
            "idle",
            "working",
            "Activity ≠ completion",
            "active",
            "blocked",
            "Refine dashboard",
            "Review runner logs",
            "Lin · a1b2c3d4 r2",
            "Ada · e5f6a7b8 r1",
            "Ada → Lin · assignment · received",
            "hub → Ada · info · pending",
            "Keep the dashboard compact and terminal-native.",
            "Waiting on an explicit review decision.",
            "TODO 2/5 open · PR poll 0s",
        ] {
            assert!(text.contains(expected), "missing {expected:?}:\n{text}");
        }
        let roster = position(&text, "Participants 2");
        let tasks = position(&text, "Tasks 2");
        let messages = position(&text, "Messages 2");
        assert_eq!(roster.1, tasks.1);
        assert!(roster.0 < tasks.0);
        assert_eq!(tasks.0, messages.0);
        assert!(tasks.1 < messages.1);
    }

    #[test]
    fn narrow_layout_stacks_sections_and_keeps_message_bodies_readable() {
        let snapshot = populated();
        let text = screen(&render(&snapshot, &selected_view(&snapshot), 56, 34, true));
        let roster = position(&text, "Participants 2");
        let tasks = position(&text, "Tasks 2");
        let messages = position(&text, "Messages 2");
        assert_eq!(roster.0, tasks.0);
        assert_eq!(tasks.0, messages.0);
        assert!(roster.1 < tasks.1 && tasks.1 < messages.1);
        for expected in [
            "architect · w1:p1",
            "implementor · w1:p2",
            "Refine dashboard",
            "Review runner logs",
            "Lin · a1b2c3d4 r2",
            "Ada → Lin · assignment · received",
            "Keep the dashboard compact and terminal-native.",
            "Waiting on an explicit review decision.",
            "[p] PR off",
            "q quit",
        ] {
            assert!(text.contains(expected), "missing {expected:?}:\n{text}");
        }
    }

    #[test]
    fn no_color_resets_every_cell_and_color_uses_only_terminal_ansi_accents() {
        let snapshot = populated();
        for (width, height) in [(10, 4), (35, 12), (56, 34), (120, 30)] {
            for help in [false, true] {
                let mut view = selected_view(&snapshot);
                view.help = help;
                let plain = render(&snapshot, &view, width, height, false);
                let colored = render(&snapshot, &view, width, height, true);
                assert_eq!(screen(&plain), screen(&colored));
                for cell in plain.content() {
                    assert_eq!(cell.bg, Color::Reset);
                    assert_eq!(cell.fg, Color::Reset);
                }
                for cell in colored.content() {
                    assert_eq!(cell.bg, Color::Reset);
                    assert!(matches!(
                        cell.fg,
                        Color::Reset | Color::Cyan | Color::Red | Color::Yellow | Color::Green
                    ));
                    assert!(!cell.modifier.contains(Modifier::REVERSED));
                }
            }
        }
    }

    #[test]
    fn help_explains_controls_and_automation_safety_at_both_widths() {
        let snapshot = populated();
        let mut view = selected_view(&snapshot);
        view.help = true;
        for (width, height) in [(120, 30), (80, 24), (64, 40)] {
            let text = screen(&render(&snapshot, &view, width, height, false));
            for expected in [
                "Tab / → / l",
                "Shift-Tab / ← / h",
                "↑ ↓ / k j",
                "t  TODO / continuation on/off",
                "c  periodic check-ins on/off",
                "p  PR watcher on/off",
                "q / Ctrl-C",
                "? / Esc",
                "/mustr-todo [path/to/TODO.md]",
                "/mustr-pr",
                "https://github.com/OWNER/REPO/pull/N",
                "Modes are off after hub restart.",
                "undelivered prompts",
                "not already-running work.",
                "Done ≠ accepted.",
                "external jobs running.",
            ] {
                assert!(text.contains(expected), "missing {expected:?}:\n{text}");
            }
            assert!(!text.contains("Refine dashboard"));
        }
    }

    #[test]
    fn only_selected_space_data_is_rendered() {
        let mut snapshot = populated();
        let mut foreign_member = snapshot.members[0].clone();
        foreign_member.id = AgentId::new("foreign");
        foreign_member.space_id = SpaceId::new("w2");
        foreign_member.name = "FOREIGN_MEMBER".into();
        snapshot.members.push(foreign_member);
        let mut foreign_task = snapshot.tasks[0].clone();
        foreign_task.space_id = SpaceId::new("w2");
        foreign_task.title = "FOREIGN_TASK".into();
        snapshot.tasks.push(foreign_task);
        let mut foreign_message = snapshot.messages[0].clone();
        foreign_message.space_id = SpaceId::new("w2");
        foreign_message.text = "FOREIGN_MESSAGE".into();
        snapshot.messages.push(foreign_message);
        let mut view = selected_view(&snapshot);
        let text = screen(&render(&snapshot, &view, 120, 30, false));
        assert!(!text.contains("FOREIGN_"));
        view.advance(&snapshot, true);
        let text = screen(&render(&snapshot, &view, 120, 30, false));
        for expected in ["FOREIGN_MEMBER", "FOREIGN_TASK", "FOREIGN_MESSAGE"] {
            assert!(text.contains(expected), "missing {expected:?}:\n{text}");
        }
        assert!(!text.contains("Refine dashboard"));
        assert!(text.contains("[t] TODO off"));
    }

    #[test]
    fn external_text_is_sanitized_including_identifiers_and_panes() {
        let mut snapshot = populated();
        snapshot.spaces[0].label = "Orc\u{1b}hard\n".into();
        snapshot.members[0].name = "A\u{7}da".into();
        snapshot.members[0].role = "archi\ttect".into();
        snapshot.members[0].pane_id = "w1:\u{1b}p1".into();
        snapshot.tasks[0].id = TaskId::new("a1b2\u{1b}c3d4");
        snapshot.tasks[0].title = "Refine\r dashboard".into();
        snapshot.messages[0].text = "Safe\u{1b}[31m text\nno bell\u{7}".into();
        snapshot.error = Some("dis\nconnected\u{7}".into());
        let buffer = render(&snapshot, &selected_view(&snapshot), 120, 30, true);
        let text = screen(&buffer);
        for expected in [
            "Orchard · w1",
            "architect · w1:p1",
            "Lin · a1b2c3d4 r2",
            "Refine dashboard",
            "Safe[31m textno bell",
            "! disconnected",
        ] {
            assert!(text.contains(expected), "missing {expected:?}:\n{text}");
        }
        assert!(
            buffer
                .content()
                .iter()
                .all(|cell| cell.symbol().chars().all(|ch| !ch.is_control()))
        );
    }

    #[test]
    fn watcher_errors_and_notices_remain_visible_and_sanitized() {
        let mut snapshot = populated();
        let automation = snapshot.automation.get_mut(&SpaceId::new("w1")).unwrap();
        automation.todo.as_mut().unwrap().error = Some("TODO\n unavailable".into());
        automation.pr.as_mut().unwrap().error = Some("PR\u{1b} unavailable".into());
        let mut view = selected_view(&snapshot);
        view.notice = "TODO off; dispatched work is not cancelled\u{7}".into();
        let text = screen(&render(&snapshot, &view, 120, 30, false));
        assert!(text.contains("! TODO: TODO unavailable"));
        assert!(text.contains("! PR: PR unavailable"));
        assert!(text.contains("TODO off; dispatched work is not cancelled"));
        snapshot.error = Some("connection failed".into());
        let text = screen(&render(&snapshot, &view, 120, 30, false));
        assert!(text.contains("! connection failed"));
        assert!(!text.contains("dispatched work is not cancelled"));
    }

    #[test]
    fn overflowing_roster_and_tasks_are_explicit_and_never_overwrite_footer() {
        let mut snapshot = populated();
        for index in 0..8 {
            let mut member = snapshot.members[0].clone();
            member.id = AgentId::new(format!("agent-{index}"));
            snapshot.members.push(member);
            let mut task = snapshot.tasks[0].clone();
            task.id = TaskId::new(format!("task-{index}"));
            snapshot.tasks.push(task);
        }
        let view = selected_view(&snapshot);
        let text = screen(&render(&snapshot, &view, 120, 18, false));
        assert!(text.contains("more participants"), "{text}");
        assert!(text.contains("more tasks"), "{text}");
        let smallest = screen(&render(&snapshot, &view, 35, 12, false));
        assert!(
            smallest.lines().nth(10).unwrap().contains("q quit"),
            "{smallest}"
        );
        assert!(
            smallest
                .lines()
                .nth(11)
                .unwrap()
                .contains("Quit leaves agents open.")
        );
    }

    #[test]
    fn selected_tab_stays_visible_in_a_crowded_navigation_rail() {
        let mut snapshot = empty_space();
        for index in 2..=12 {
            snapshot.spaces.push(Space {
                id: SpaceId::new(format!("w{index}")),
                label: format!("Project {index}"),
            });
        }
        let mut view = selected_view(&snapshot);
        view.selected = Some(SpaceId::new("w12"));
        let text = screen(&render(&snapshot, &view, 40, 24, false));
        assert!(text.contains("Project 12 · w12"), "{text}");
        assert!(text.contains("12/12"));
        assert_eq!(elide("工具 / Orchard", 8), "工具 / …");
        assert_eq!(elide("hello", 0), "");
    }

    #[test]
    fn scrolling_moves_only_addressed_communication() {
        let mut snapshot = populated();
        for index in 0..20 {
            let mut message = snapshot.messages[0].clone();
            message.text = format!("Older message {index}");
            snapshot.messages.push(message);
        }
        let mut view = selected_view(&snapshot);
        let before = screen(&render(&snapshot, &view, 120, 30, false));
        for _ in 0..8 {
            view.navigate(KeyCode::Down, &snapshot);
        }
        let after = screen(&render(&snapshot, &view, 120, 30, false));
        assert!(before.contains("Keep the dashboard compact"));
        assert!(!after.contains("Keep the dashboard compact"));
        assert!(after.contains("Older message"));
        let header_rows = position(&before, "Messages 22").1;
        assert_eq!(
            before.lines().take(header_rows).collect::<Vec<_>>(),
            after.lines().take(header_rows).collect::<Vec<_>>()
        );
    }

    #[test]
    fn tiny_and_short_terminals_do_not_panic_or_leak_styles() {
        for snapshot in [Snapshot::default(), empty_space(), populated()] {
            for width in [0, 1, 10, 34, 35, 40, 60, 96, 98, 120] {
                for height in [0, 1, 4, 11, 12, 13, 18, 24] {
                    let buffer = render(&snapshot, &selected_view(&snapshot), width, height, false);
                    assert!(
                        buffer
                            .content()
                            .iter()
                            .all(|cell| cell.bg == Color::Reset && cell.fg == Color::Reset)
                    );
                }
            }
        }
    }

    // Run `cargo test tui::tests::render_previews -- --nocapture` for reproducible visual review.
    #[test]
    fn render_previews() {
        for (name, snapshot, width, height, help) in [
            ("EMPTY", empty_space(), 100, 24, false),
            ("POPULATED", populated(), 120, 30, false),
            ("NARROW", populated(), 56, 34, false),
            ("HELP", populated(), 120, 30, true),
        ] {
            let mut view = selected_view(&snapshot);
            view.help = help;
            let preview = screen(&render(&snapshot, &view, width, height, false));
            assert_eq!(
                preview,
                screen(&render(&snapshot, &view, width, height, false))
            );
            println!("\n--- {name} {width}x{height} ---\n{preview}");
        }
    }

    #[test]
    fn arrows_and_vim_keys_have_identical_navigation() {
        let snapshot = Snapshot {
            spaces: vec![
                Space {
                    id: SpaceId::new("w1"),
                    label: "A".into(),
                },
                Space {
                    id: SpaceId::new("w2"),
                    label: "B".into(),
                },
            ],
            ..Snapshot::default()
        };
        for (arrow, vim) in [
            (KeyCode::Left, 'h'),
            (KeyCode::Right, 'l'),
            (KeyCode::Up, 'k'),
            (KeyCode::Down, 'j'),
        ] {
            let mut a = View::default();
            let mut b = View::default();
            a.reconcile(&snapshot);
            b.reconcile(&snapshot);
            a.scroll = 3;
            b.scroll = 3;
            assert!(a.navigate(arrow, &snapshot));
            assert!(b.navigate(KeyCode::Char(vim), &snapshot));
            assert_eq!(a.selected, b.selected);
            assert_eq!(a.scroll, b.scroll);
        }
        let mut view = View::default();
        view.navigate(KeyCode::Up, &snapshot);
        assert_eq!(view.scroll, 0);
        assert!(!view.navigate(KeyCode::Char('q'), &snapshot));
    }

    #[test]
    fn selection_survives_reordering() {
        let mut snapshot = Snapshot {
            spaces: vec![
                Space {
                    id: SpaceId::new("w1"),
                    label: "A".into(),
                },
                Space {
                    id: SpaceId::new("w2"),
                    label: "B".into(),
                },
            ],
            ..Snapshot::default()
        };
        let mut view = View::default();
        view.reconcile(&snapshot);
        view.advance(&snapshot, true);
        snapshot.spaces.reverse();
        view.reconcile(&snapshot);
        assert_eq!(view.selected, Some(SpaceId::new("w2")));
        snapshot.spaces.clear();
        view.reconcile(&snapshot);
        assert!(view.selected.is_none());
    }
}
