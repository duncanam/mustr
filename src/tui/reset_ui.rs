//! Human-only reset confirmation and progress. Tokens never enter this view.
use super::*;
use ratatui::widgets::Clear;

#[derive(Clone)]
enum Page {
    Confirm(ResetPreview),
    Progress(SpaceId),
    Abandon { space: SpaceId, plan: String },
    Error(String),
}

#[derive(Clone)]
pub(super) struct Dialog {
    page: Page,
    typed: String,
    scroll: u16,
    read_all: bool,
}

pub(super) enum Action {
    None,
    Close,
    Start(ResetPreview),
    Abandon(SpaceId, String),
}

impl Dialog {
    fn new(page: Page) -> Self {
        Self {
            page,
            typed: String::new(),
            scroll: 0,
            read_all: false,
        }
    }

    pub(super) fn confirm(preview: ResetPreview) -> Self {
        Self::new(Page::Confirm(preview))
    }
    pub(super) fn progress(space: SpaceId) -> Self {
        Self::new(Page::Progress(space))
    }
    pub(super) fn error(error: String) -> Self {
        Self::new(Page::Error(error))
    }
    pub(super) fn hide(&mut self) {
        self.read_all = false;
    }

    pub(super) fn key(&mut self, key: KeyCode, snapshot: &Snapshot) -> Action {
        match key {
            KeyCode::Esc => return Action::Close,
            KeyCode::Down => self.scroll = self.scroll.saturating_add(1),
            KeyCode::Up => self.scroll = self.scroll.saturating_sub(1),
            KeyCode::PageDown => self.scroll = self.scroll.saturating_add(8),
            KeyCode::PageUp => self.scroll = self.scroll.saturating_sub(8),
            KeyCode::Home => self.scroll = 0,
            KeyCode::End => self.scroll = u16::MAX,
            KeyCode::Backspace => {
                self.typed.pop();
            }
            KeyCode::Enter => {
                if self.typed.is_empty() {
                    return Action::Close;
                }
                if !self.read_all {
                    return Action::None;
                }
                return match &self.page {
                    Page::Confirm(preview) if self.typed == "RESET" => {
                        Action::Start(preview.clone())
                    }
                    Page::Abandon { space, plan } if self.typed == "ABANDON" => {
                        Action::Abandon(space.clone(), plan.clone())
                    }
                    _ => Action::None,
                };
            }
            KeyCode::Char('a') if matches!(self.page, Page::Progress(_)) => {
                if let Page::Progress(space) = &self.page
                    && let Some(plan) = snapshot
                        .reset
                        .as_ref()
                        .filter(|plan| &plan.space_id == space)
                    && matches!(plan.state, ResetPhase::Prepared | ResetPhase::Incomplete)
                {
                    *self = Self::new(Page::Abandon {
                        space: space.clone(),
                        plan: plan.plan_id.clone(),
                    });
                }
            }
            KeyCode::Char(ch)
                if matches!(self.page, Page::Confirm(_) | Page::Abandon { .. })
                    && ch.is_ascii_alphabetic()
                    && self.typed.len() < 8 =>
            {
                self.typed.push(ch.to_ascii_uppercase());
            }
            _ => {}
        }
        Action::None
    }

    fn confirmation_prompt(&self, word: &str) -> Vec<Line<'static>> {
        vec![
            Line::styled(format!("Type {word}, then press Enter to confirm"), bold()),
            Line::styled(
                format!(
                    "Confirmation: {}",
                    if self.typed.is_empty() {
                        "_"
                    } else {
                        &self.typed
                    }
                ),
                bold(),
            ),
            Line::raw("Esc or empty Enter: Cancel"),
        ]
    }

    fn text(&self, snapshot: &Snapshot) -> (&'static str, Vec<Line<'static>>, Vec<Line<'static>>) {
        match &self.page {
            Page::Confirm(preview) => {
                let space_name = snapshot
                    .space
                    .as_ref()
                    .filter(|s| s.id == preview.space_id)
                    .map(|s| clean(&s.label))
                    .unwrap_or_else(|| clean(preview.space_id.as_str()));
                let mut lines = vec![
                    Line::styled(
                        format!(
                            "Start fresh in {space_name} ({})?",
                            clean(preview.space_id.as_str())
                        ),
                        bold(),
                    ),
                    Line::raw(format!(
                        "Clear {} tasks and {} messages.",
                        preview.task_count, preview.message_count
                    )),
                    Line::raw(
                        "Detach TODO/PR and turn ALL automation off. TODO/project files are NOT changed.",
                    ),
                    Line::raw(format!(
                        "Create fresh Pi sessions for these {} connected agents only:",
                        preview.participants.len()
                    )),
                ];
                for participant in &preview.participants {
                    lines.push(Line::raw(format!(
                        "  {} · {} · {}",
                        clean(&participant.name),
                        clean(&participant.role),
                        clean(&participant.pane_id)
                    )));
                }
                lines.extend([
                    Line::raw(format!("{} offline identities: their saved context is NOT remotely cleared; old identities are retired.", preview.offline_count)),
                    Line::raw("Old Pi sessions remain saved. Policies/tools and approved roles remain; conversation history is not copied."),
                    Line::styled("Running tools/external jobs are NOT stopped. Reconcile them first.", bold()),
                    Line::raw("A partial reset cannot undo sessions already replaced. Failed/vetoed agents leave history retained and the space frozen for review."),
                    Line::raw("This affects only this space. Read all details before confirming."),
                ]);
                (" Start fresh? ", lines, self.confirmation_prompt("RESET"))
            }
            Page::Progress(space) => {
                let mut lines = vec![Line::styled(
                    format!("Fresh-start status: {}", clean(space.as_str())),
                    bold(),
                )];
                if let Some(plan) = snapshot
                    .reset
                    .as_ref()
                    .filter(|plan| &plan.space_id == space)
                {
                    lines.push(Line::raw(format!("State: {:?}", plan.state)));
                    lines.push(Line::raw(clean(&plan.note)));
                    for progress in &plan.participants {
                        lines.push(Line::raw(format!(
                            "{} · {:?}: {}",
                            clean(&progress.participant.name),
                            progress.state,
                            clean(&progress.note)
                        )));
                    }
                    if matches!(plan.state, ResetPhase::Prepared | ResetPhase::Incomplete) {
                        lines.push(Line::raw("Space frozen; no automatic repeat of attempted resets. [a] review abandoning this attempt (keeps hub history)."));
                    }
                } else {
                    lines.push(Line::raw(
                        "No reset status available. Do not assume agents were cleared.",
                    ));
                }
                (
                    " Fresh-start progress ",
                    lines,
                    vec![
                        Line::raw("Enter/Esc: close"),
                        Line::raw("Closing does not cancel reset."),
                    ],
                )
            }
            Page::Abandon { space, .. } => (
                " Abandon incomplete reset? ",
                vec![
                    Line::styled(
                        format!("Abandon reset for {}?", clean(space.as_str())),
                        bold(),
                    ),
                    Line::raw(
                        "Keep hub history and bindings; automation remains OFF. No old deliveries will be replayed. Work needs explicit reconciliation.",
                    ),
                    Line::raw(
                        "This does NOT restore old Pi contexts or undo fresh sessions. Already-issued replacements may still finish, but their old tickets are rejected.",
                    ),
                    Line::raw(
                        "Affected agents may need manual /new followed by /role NAME. Check every agent and external job before starting another reset.",
                    ),
                ],
                self.confirmation_prompt("ABANDON"),
            ),
            Page::Error(error) => (
                " Fresh start unavailable ",
                vec![Line::raw(clean(error))],
                vec![
                    Line::raw("Enter/Esc: close"),
                    Line::raw("Inspect reset status before retry."),
                ],
            ),
        }
    }

    pub(super) fn draw(&mut self, frame: &mut Frame, snapshot: &Snapshot) {
        let screen = frame.area();
        let width = screen.width.min(90);
        let rows = screen.height.min(28);
        let area = Rect::new(
            screen.x + (screen.width - width) / 2,
            screen.y + (screen.height - rows) / 2,
            width,
            rows,
        );
        frame.render_widget(Clear, area);
        let (title, lines, prompt) = self.text(snapshot);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(muted())
            .title(Line::styled(title, bold()))
            .padding(Padding::horizontal(1));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        // Reserve the actual wrapped instruction/input/cancel height. At the
        // supported minimum this keeps every action visible, not clipped behind
        // the warning body or bottom border.
        let prompt = Paragraph::new(prompt).wrap(Wrap { trim: false });
        let prompt_height = height(prompt.line_count(inner.width));
        let [body, prompt_area, controls] = Layout::vertical([
            Constraint::Min(1),
            Constraint::Length(prompt_height),
            Constraint::Length(1),
        ])
        .areas(inner);
        let paragraph = Paragraph::new(lines).wrap(Wrap { trim: false });
        let total = paragraph.line_count(body.width);
        self.scroll = self
            .scroll
            .min(height(total.saturating_sub(usize::from(body.height))));
        self.read_all = usize::from(self.scroll) + usize::from(body.height) >= total;
        frame.render_widget(paragraph.scroll((self.scroll, 0)), body);
        frame.render_widget(prompt, prompt_area);
        let confirmation = matches!(self.page, Page::Confirm(_) | Page::Abandon { .. });
        let hint = match (confirmation, self.read_all) {
            (true, false) => "Read more before confirming ↓",
            (true, true) => "↑↓ scroll · all details read",
            (false, false) => "↑↓/End: read more",
            (false, true) => "↑↓ scroll",
        };
        frame.render_widget(Paragraph::new(hint), controls);
    }
}
