//! Human-only checkout selection and PR discovery feedback.
use super::*;
use crate::service::PrSelection;
use ratatui::{layout::Alignment, widgets::Clear};

fn clean_controls(text: &str) -> String {
    text.chars()
        .map(|ch| if ch.is_control() { ' ' } else { ch })
        .collect()
}

pub(super) fn eligible(snapshot: &Snapshot, space: &SpaceId) -> Vec<PrSelection> {
    snapshot
        .members
        .iter()
        .filter(|member| {
            &member.space_id == space
                && !matches!(member.activity, Activity::Offline | Activity::Paused)
                && member.cwd.as_ref().is_some_and(|cwd| cwd.is_absolute())
        })
        .map(|member| PrSelection {
            epoch: snapshot.epoch,
            agent_id: member.id.clone(),
            name: member.name.clone(),
            role: member.role.clone(),
            pane_id: member.pane_id.clone(),
            cwd: member.cwd.clone().expect("eligible members have a cwd"),
        })
        .collect()
}

#[derive(Clone)]
enum Page {
    Choose(Vec<PrSelection>),
    Error(String),
}

#[derive(Clone)]
pub(super) struct Dialog {
    page: Page,
    selected: usize,
    rendered: bool,
}

pub(super) enum Action {
    None,
    Close,
    Select(Box<PrSelection>),
}

impl Dialog {
    pub(super) fn choose(candidates: Vec<PrSelection>) -> Self {
        Self {
            page: Page::Choose(candidates),
            selected: 0,
            rendered: false,
        }
    }

    pub(super) fn error(message: String) -> Self {
        Self {
            page: Page::Error(message),
            selected: 0,
            rendered: false,
        }
    }

    pub(super) fn hide(&mut self) {
        self.rendered = false;
    }

    pub(super) fn key(&mut self, key: KeyCode) -> Action {
        match (&self.page, key) {
            (_, KeyCode::Esc) => Action::Close,
            (Page::Error(_), KeyCode::Enter) => Action::Close,
            (Page::Choose(candidates), KeyCode::Enter) if self.rendered => candidates
                .get(self.selected)
                .cloned()
                .map(|selection| Action::Select(Box::new(selection)))
                .unwrap_or(Action::Close),
            (Page::Choose(_), KeyCode::Enter) => Action::None,
            (Page::Choose(candidates), KeyCode::Down | KeyCode::Char('j')) => {
                self.selected = self
                    .selected
                    .saturating_add(1)
                    .min(candidates.len().saturating_sub(1));
                Action::None
            }
            (Page::Choose(_), KeyCode::Up | KeyCode::Char('k')) => {
                self.selected = self.selected.saturating_sub(1);
                Action::None
            }
            _ => Action::None,
        }
    }

    pub(super) fn draw(&mut self, frame: &mut Frame, color: bool) {
        let screen = frame.area();
        let width = screen.width.min(90);
        let desired_rows = match &self.page {
            Page::Choose(candidates) => {
                height(candidates.len().saturating_mul(2).saturating_add(7))
            }
            Page::Error(_) => 12,
        };
        let rows = screen.height.min(desired_rows.max(8));
        let area = Rect::new(
            screen.x + (screen.width - width) / 2,
            screen.y + (screen.height - rows) / 2,
            width,
            rows,
        );
        frame.render_widget(Clear, area);
        let (title, lines, controls) = match &self.page {
            Page::Choose(candidates) => {
                let lines = candidates
                    .iter()
                    .enumerate()
                    .flat_map(|(index, candidate)| {
                        let selected = index == self.selected;
                        let marker = if selected { "›" } else { " " };
                        let title_style = if selected {
                            bold().add_modifier(Modifier::UNDERLINED)
                        } else {
                            bold()
                        };
                        let title = elide(
                            &format!(
                                "{} · {} · {}",
                                clean(&candidate.name),
                                clean(&candidate.role),
                                clean(&candidate.pane_id)
                            ),
                            width.saturating_sub(6),
                        );
                        [
                            Line::from(vec![
                                Span::styled(
                                    format!("{marker} "),
                                    accent(color).add_modifier(Modifier::BOLD),
                                ),
                                Span::styled(title, title_style),
                            ]),
                            Line::styled(
                                format!(
                                    "  {}",
                                    elide(
                                        &clean(&candidate.cwd.to_string_lossy()),
                                        width.saturating_sub(6)
                                    )
                                ),
                                muted(),
                            ),
                        ]
                    })
                    .collect::<Vec<_>>();
                (
                    " Select PR checkout ",
                    lines,
                    vec![
                        Line::raw("↑↓ / jk choose · Enter discover · Esc cancel"),
                        Line::styled(
                            "The selected participant owns this checkout's PR watch.",
                            muted(),
                        ),
                    ],
                )
            }
            Page::Error(message) => (
                " PR discovery unavailable ",
                vec![Line::styled(
                    clean_controls(message),
                    tone(color, Color::Red),
                )],
                vec![
                    Line::raw("Enter/Esc: close"),
                    Line::styled("Correct the reported cause before retrying p.", muted()),
                ],
            ),
        };
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(muted())
            .title(Line::styled(title, bold()))
            .padding(Padding::horizontal(1));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        let controls_widget = Paragraph::new(controls)
            .alignment(Alignment::Left)
            .wrap(Wrap { trim: false });
        let controls_height = height(controls_widget.line_count(inner.width));
        let [body, controls_area] =
            Layout::vertical([Constraint::Min(1), Constraint::Length(controls_height)])
                .areas(inner);
        let paragraph = Paragraph::new(lines).wrap(Wrap { trim: false });
        let scroll = match &self.page {
            Page::Choose(_) => {
                let capacity = usize::from((body.height / 2).max(1));
                let offset = self.selected.saturating_sub(capacity.saturating_sub(1));
                height(offset.saturating_mul(2))
            }
            Page::Error(_) => 0,
        };
        frame.render_widget(paragraph.scroll((scroll, 0)), body);
        frame.render_widget(controls_widget, controls_area);
        self.rendered = true;
    }
}
