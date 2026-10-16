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
    layout::{Constraint, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Cell, Paragraph, Row, Table, Tabs, Wrap},
};

use crate::{
    protocol::*,
    service::{Service, now},
};

#[derive(Default)]
struct View {
    selected: Option<SpaceId>,
    scroll: u16,
    help: bool,
    notice: String,
}

impl View {
    fn reconcile(&mut self, snapshot: &Snapshot) {
        if !snapshot
            .spaces
            .iter()
            .any(|s| Some(&s.id) == self.selected.as_ref())
        {
            self.selected = snapshot.spaces.first().map(|s| s.id.clone());
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

pub(crate) async fn run(service: Service, color: bool) -> Result<()> {
    ensure!(
        io::stdin().is_terminal() && io::stdout().is_terminal(),
        "dashboard needs a terminal; use mustr run --headless"
    );
    // color-eyre's panic hook is installed first; Ratatui wraps it to restore terminal state.
    let mut terminal = ratatui::try_init()?;
    let result = event_loop(&mut terminal, service, color).await;
    let restored = ratatui::try_restore();
    result?;
    restored?;
    Ok(())
}

async fn event_loop(
    terminal: &mut ratatui::DefaultTerminal,
    service: Service,
    color: bool,
) -> Result<()> {
    let mut events = EventStream::new();
    let mut snapshots = service.snapshots.clone();
    let mut view = View::default();
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    loop {
        let snapshot = snapshots.borrow().clone();
        view.reconcile(&snapshot);
        terminal.draw(|frame| draw(frame, &snapshot, &view, color))?;
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
fn mode_span(key: &str, on: bool, color: bool) -> Span<'static> {
    Span::styled(
        format!(" [{key}] {} ", if on { "ON" } else { "off" }),
        if on {
            accent(color).add_modifier(Modifier::BOLD)
        } else {
            Style::default().add_modifier(Modifier::DIM)
        },
    )
}

fn draw(frame: &mut Frame, snapshot: &Snapshot, view: &View, color: bool) {
    let area = frame.area();
    frame.render_widget(Block::default().style(Style::reset()), area);
    if area.width < 35 || area.height < 12 {
        frame.render_widget(
            Paragraph::new("mustr · enlarge terminal (35×12 minimum)\nq quits"),
            area,
        );
        return;
    }
    let regions = Layout::vertical([
        Constraint::Length(3),
        Constraint::Length(2),
        Constraint::Min(3),
        Constraint::Length(2),
    ])
    .split(area);
    let selected = snapshot
        .spaces
        .iter()
        .position(|s| Some(&s.id) == view.selected.as_ref());
    let tabs = Tabs::new(
        snapshot
            .spaces
            .iter()
            .map(|s| format!("{} · {}", clean(&s.label), s.id)),
    )
    .select(selected)
    .highlight_style(Style::default().add_modifier(Modifier::REVERSED | Modifier::BOLD))
    .block(
        Block::default()
            .title(" mustr · herdr spaces ")
            .borders(Borders::ALL),
    );
    frame.render_widget(tabs, regions[0]);
    if view.help {
        frame.render_widget(Paragraph::new("Tab / ← →   change space\n↑ ↓ / k j     scroll communication\nt             TODO/continuation on/off\nc             periodic check-ins on/off\np             PR watcher on/off\nq             stop hub (agents remain open)\n\nAttach a TODO or PR from the intended Pi recipient:\n/mustr-todo [path/to/TODO.md]\n/mustr-pr https://github.com/OWNER/REPO/pull/N\n\nModes are off after hub restart. Turning a mode off cancels undelivered prompts, not already-running work. ? or Esc closes help.").wrap(Wrap { trim: false }).block(Block::bordered().title(" Controls ")), regions[2]);
    } else if let Some(space) = &view.selected {
        let automation = snapshot.automation.get(space).cloned().unwrap_or_default();
        let mut modes = vec![
            mode_span("t TODO", automation.todo_enabled, color),
            mode_span("c Check-in", automation.checkin_enabled, color),
            mode_span("p PR", automation.pr_enabled, color),
        ];
        if let Some(todo) = &automation.todo {
            modes.push(Span::raw(format!(
                " · TODO {}/{} open",
                todo.remaining, todo.total
            )));
        }
        if let Some(pr) = &automation.pr {
            modes.push(Span::raw(format!(
                " · PR poll {}s",
                pr.next_at.saturating_sub(now())
            )));
        }
        frame.render_widget(Paragraph::new(Line::from(modes)), regions[1]);
        let panels = Layout::vertical([
            Constraint::Percentage(32),
            Constraint::Percentage(28),
            Constraint::Min(3),
        ])
        .split(regions[2]);
        let members = snapshot.members.iter().filter(|m| &m.space_id == space);
        let rows = members.map(|m| {
            Row::new([
                clean(&m.name),
                clean(&m.role),
                label(m.activity),
                m.pane_id.clone(),
            ])
        });
        frame.render_widget(
            Table::new(
                rows,
                [
                    Constraint::Percentage(35),
                    Constraint::Percentage(25),
                    Constraint::Percentage(20),
                    Constraint::Percentage(20),
                ],
            )
            .header(
                Row::new(["Participant", "Role", "Activity", "Pane"])
                    .style(Style::default().add_modifier(Modifier::BOLD)),
            )
            .block(Block::bordered().title(" Participants · activity ≠ completion ")),
            panels[0],
        );
        let rows = snapshot
            .tasks
            .iter()
            .filter(|t| &t.space_id == space)
            .map(|t| {
                let owner = snapshot
                    .members
                    .iter()
                    .find(|m| m.id == t.owner)
                    .map(|m| m.name.as_str())
                    .unwrap_or(t.owner.as_str());
                Row::new([
                    Cell::from(format!(
                        "{} r{}",
                        t.id.as_str().chars().take(8).collect::<String>(),
                        t.revision
                    )),
                    Cell::from(clean(&t.title)),
                    Cell::from(clean(owner)),
                    Cell::from(label(t.state)),
                ])
            });
        frame.render_widget(
            Table::new(
                rows,
                [
                    Constraint::Length(13),
                    Constraint::Percentage(45),
                    Constraint::Percentage(25),
                    Constraint::Min(9),
                ],
            )
            .block(Block::bordered().title(" Tasks ")),
            panels[1],
        );
        let mut lines = Vec::new();
        if let Some(todo) = &automation.todo
            && let Some(error) = &todo.error
        {
            lines.push(Line::raw(format!("TODO: {}", clean(error))));
        }
        if let Some(pr) = &automation.pr
            && let Some(error) = &pr.error
        {
            lines.push(Line::raw(format!("PR: {}", clean(error))));
        }
        for message in snapshot.messages.iter().filter(|m| &m.space_id == space) {
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
            lines.push(Line::from(vec![Span::styled(
                format!(
                    "{} → {} · {} · {}",
                    clean(from),
                    clean(to),
                    label(message.kind),
                    label(message.delivery)
                ),
                accent(color),
            )]));
            lines.push(Line::raw(format!(
                "  {}",
                clean(&message.text).chars().take(240).collect::<String>()
            )));
        }
        if lines.is_empty() {
            lines.push(Line::raw("No messages. Join from Pi with /role NAME."));
        }
        frame.render_widget(
            Paragraph::new(lines)
                .scroll((view.scroll, 0))
                .wrap(Wrap { trim: false })
                .block(Block::bordered().title(" Addressed communication · newest first ")),
            panels[2],
        );
    } else {
        frame.render_widget(
            Paragraph::new("Waiting for herdr spaces. No agent sessions are read or launched."),
            regions[2],
        );
    }
    let footer = snapshot
        .error
        .as_deref()
        .filter(|s| !s.is_empty())
        .unwrap_or(&view.notice);
    frame.render_widget(
        Paragraph::new(format!(
            "Tab/←→ spaces · ↑↓ scroll · t TODO · c check-ins · p PR · ? help · q quit\n{}",
            clean(footer)
        )),
        regions[3],
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend};

    #[test]
    fn follows_terminal_colors_and_handles_small_sizes() {
        for (width, height) in [(10, 4), (40, 12), (100, 30)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            let snapshot = Snapshot {
                spaces: vec![Space {
                    id: SpaceId::new("w1"),
                    label: "Project".into(),
                }],
                ..Snapshot::default()
            };
            let mut view = View::default();
            view.reconcile(&snapshot);
            terminal.draw(|f| draw(f, &snapshot, &view, false)).unwrap();
            for cell in terminal.backend().buffer().content() {
                assert_eq!(cell.bg, Color::Reset);
                assert_eq!(cell.fg, Color::Reset);
            }
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
