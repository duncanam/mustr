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
    widgets::{Block, BorderType, Borders, Cell, Padding, Paragraph, Row, Table, Wrap},
};

use crate::{
    profile_startup::{EntryOutcome, EntryState, ProfileProgress},
    protocol::*,
    service::{Service, now},
};

mod pr_ui;
mod reset_ui;

#[derive(Clone, Copy, Default, Debug, Eq, PartialEq)]
enum WorkTab {
    #[default]
    Messages,
    Tasks,
}

#[derive(Clone, Default)]
struct TaskBrowse {
    selected: Option<TaskId>,
    // The previous ordinal supplies a nearby fallback if the selected task disappears.
    index: usize,
    offset: usize,
}

impl TaskBrowse {
    fn reconcile(&mut self, tasks: &[&Task]) {
        self.index = self
            .selected
            .as_ref()
            .and_then(|id| tasks.iter().position(|task| &task.id == id))
            .unwrap_or_else(|| self.index.min(tasks.len().saturating_sub(1)));
        self.selected = tasks.get(self.index).map(|task| task.id.clone());
        self.offset = self.offset.min(tasks.len().saturating_sub(1));
        if tasks.is_empty() {
            *self = Self::default();
        }
    }

    fn advance(&mut self, tasks: &[&Task], forward: bool) {
        self.reconcile(tasks);
        self.index = if forward {
            self.index
                .saturating_add(1)
                .min(tasks.len().saturating_sub(1))
        } else {
            self.index.saturating_sub(1)
        };
        self.selected = tasks.get(self.index).map(|task| task.id.clone());
    }

    fn viewport(&mut self, count: usize, capacity: usize) -> std::ops::Range<usize> {
        if capacity == 0 {
            return self.offset..self.offset;
        }
        self.offset = self.offset.min(count.saturating_sub(capacity));
        if self.index < self.offset {
            self.offset = self.index;
        } else if self.index >= self.offset.saturating_add(capacity) {
            self.offset = self.index + 1 - capacity;
        }
        self.offset..self.offset.saturating_add(capacity).min(count)
    }
}

#[derive(Clone, Default)]
struct ProfileRow {
    role: String,
    outcome: Option<EntryOutcome>,
}

#[derive(Clone, Default)]
struct ProfileView {
    rows: Vec<ProfileRow>,
    next_finished: usize,
    scroll: usize,
    total: usize,
    visible: std::ops::Range<usize>,
    cancelling: bool,
}

impl ProfileView {
    fn apply(&mut self, progress: ProfileProgress) {
        match progress {
            ProfileProgress::Starting { index, total, role } => {
                self.total = total;
                self.rows.resize_with(total, ProfileRow::default);
                if let Some(row) = index
                    .checked_sub(1)
                    .and_then(|index| self.rows.get_mut(index))
                {
                    row.role = role;
                    row.outcome = None;
                }
            }
            ProfileProgress::Finished(outcome) => {
                let index = self.next_finished;
                if self.rows.len() <= index {
                    self.rows.resize_with(index + 1, ProfileRow::default);
                }
                let row = &mut self.rows[index];
                if row.role.is_empty() {
                    row.role = outcome.role.clone();
                }
                row.outcome = Some(outcome);
                self.next_finished = index + 1;
            }
        }
    }

    fn complete_error(&mut self) {
        for row in &mut self.rows {
            if row.outcome.is_none() && !row.role.is_empty() {
                row.outcome = Some(EntryOutcome {
                    role: row.role.clone(),
                    pane_id: None,
                    state: EntryState::Uncertain,
                    reason: "profile coordinator ended before reporting an outcome".into(),
                });
            }
        }
        self.total = self.rows.len();
        self.next_finished = self.rows.len();
    }

    fn replace_outcomes(&mut self, outcomes: Vec<EntryOutcome>) {
        self.rows = outcomes
            .into_iter()
            .map(|outcome| ProfileRow {
                role: outcome.role.clone(),
                outcome: Some(outcome),
            })
            .collect();
        self.total = self.rows.len();
        self.next_finished = self.rows.len();
        self.scroll = self.scroll.min(self.rows.len().saturating_sub(1));
    }

    fn populated_len(&self) -> usize {
        self.rows
            .iter()
            .rposition(|row| !row.role.is_empty())
            .map_or(0, |index| index + 1)
    }

    fn row_text(&self, index: usize) -> String {
        let Some(row) = self.rows.get(index) else {
            return String::new();
        };
        let (state, pane, reason) = match &row.outcome {
            Some(outcome) => (
                match outcome.state {
                    EntryState::Joined => "joined",
                    EntryState::Failed => "failed",
                    EntryState::Uncertain => "uncertain",
                },
                outcome.pane_id.as_deref().unwrap_or("not created"),
                outcome.reason.as_str(),
            ),
            None if self.cancelling => (
                "cancel requested",
                "pending",
                "waiting for the current Herdr command to finish",
            ),
            None => ("starting", "pending", "waiting for hub admission"),
        };
        format!(
            "{}/{} {} · {} · pane {} · {}",
            index + 1,
            self.total.max(self.rows.len()),
            clean(&row.role),
            state,
            clean(pane),
            clean(reason)
        )
    }

    fn row_height(&self, index: usize, width: u16) -> usize {
        let width = usize::from(width.max(1));
        Line::raw(self.row_text(index))
            .width()
            .max(1)
            .div_ceil(width)
    }

    fn visible_range(&mut self, width: u16, available_lines: usize) -> std::ops::Range<usize> {
        let populated = self.populated_len();
        self.scroll = self.scroll.min(populated.saturating_sub(1));
        let mut used = 0;
        let mut end = self.scroll;
        while end < populated {
            let height = self.row_height(end, width);
            if used + height > available_lines {
                break;
            }
            used += height;
            end += 1;
        }
        self.visible = self.scroll..end;
        self.visible.clone()
    }

    fn move_scroll(&mut self, forward: bool) {
        let last = self.populated_len().saturating_sub(1);
        self.scroll = if forward {
            self.scroll.saturating_add(1).min(last)
        } else {
            self.scroll.saturating_sub(1)
        };
    }
}

#[derive(Clone)]
struct View {
    space_id: SpaceId,
    scroll: u16,
    work_tab: WorkTab,
    tasks: TaskBrowse,
    reset_dialog: Option<reset_ui::Dialog>,
    pr_dialog: Option<pr_ui::Dialog>,
    help: bool,
    notice: String,
    profile: Option<ProfileView>,
}

impl View {
    fn in_space(space_id: SpaceId) -> Self {
        Self {
            space_id,
            scroll: 0,
            work_tab: WorkTab::Messages,
            tasks: TaskBrowse::default(),
            reset_dialog: None,
            pr_dialog: None,
            help: false,
            notice: String::new(),
            profile: None,
        }
    }

    fn space<'a>(&self, snapshot: &'a Snapshot) -> Option<&'a Space> {
        snapshot
            .space
            .as_ref()
            .filter(|space| space.id == self.space_id)
    }

    fn reset<'a>(&self, snapshot: &'a Snapshot) -> Option<&'a ResetSummary> {
        snapshot
            .reset
            .as_ref()
            .filter(|reset| reset.space_id == self.space_id)
    }

    fn reconcile(&mut self, snapshot: &Snapshot) {
        // Metadata can disappear or be renamed, but this identity never changes.
        self.tasks.reconcile(&self.space_tasks(snapshot));
    }

    fn space_tasks<'a>(&self, snapshot: &'a Snapshot) -> Vec<&'a Task> {
        snapshot
            .tasks
            .iter()
            .filter(|task| task.space_id == self.space_id)
            .collect()
    }

    // Some means consumed by the modal, even when it produced no action.
    // The event loop must never fall through to navigation/automation in that case.
    fn reset_key(&mut self, key: KeyCode, snapshot: &Snapshot) -> Option<reset_ui::Action> {
        self.reset_dialog
            .as_mut()
            .map(|dialog| dialog.key(key, snapshot))
    }

    fn pr_key(&mut self, key: KeyCode) -> Option<pr_ui::Action> {
        self.pr_dialog.as_mut().map(|dialog| dialog.key(key))
    }

    fn navigate(&mut self, key: KeyCode, snapshot: &Snapshot) -> bool {
        match key {
            KeyCode::Char('f') => {
                self.work_tab = match self.work_tab {
                    WorkTab::Messages => WorkTab::Tasks,
                    WorkTab::Tasks => WorkTab::Messages,
                };
            }
            KeyCode::Down | KeyCode::Char('j') => match self.work_tab {
                WorkTab::Messages => self.scroll = self.scroll.saturating_add(1),
                WorkTab::Tasks => self.tasks.advance(&self.space_tasks(snapshot), true),
            },
            KeyCode::Up | KeyCode::Char('k') => match self.work_tab {
                WorkTab::Messages => self.scroll = self.scroll.saturating_sub(1),
                WorkTab::Tasks => self.tasks.advance(&self.space_tasks(snapshot), false),
            },
            _ => return false,
        }
        true
    }
}

pub(crate) struct ProfileUi {
    pub progress: tokio::sync::mpsc::UnboundedReceiver<ProfileProgress>,
    pub cancel: tokio::sync::watch::Sender<bool>,
    pub coordinator: Option<tokio::task::JoinHandle<Result<Vec<EntryOutcome>>>>,
}

type ProfileCoordinatorResult =
    std::result::Result<Result<Vec<EntryOutcome>>, tokio::task::JoinError>;

async fn collect_profile_result(
    profile: &mut Option<ProfileUi>,
) -> Option<ProfileCoordinatorResult> {
    let coordinator = profile.as_mut()?.coordinator.take()?;
    Some(coordinator.await)
}

async fn cancel_and_collect_profile(
    profile: &mut Option<ProfileUi>,
) -> Option<ProfileCoordinatorResult> {
    if let Some(profile) = profile.as_mut() {
        profile.cancel.send_replace(true);
    }
    collect_profile_result(profile).await
}

fn attach_profile_result(
    error: color_eyre::eyre::Report,
    result: Option<&ProfileCoordinatorResult>,
) -> color_eyre::eyre::Report {
    match result {
        Some(Ok(Ok(outcomes))) => {
            error.wrap_err(format!("profile coordinator outcomes: {outcomes:?}"))
        }
        Some(Ok(Err(coordinator_error))) => {
            error.wrap_err(format!("profile coordinator failed: {coordinator_error:#}"))
        }
        Some(Err(join_error)) => {
            error.wrap_err(format!("profile coordinator task failed: {join_error}"))
        }
        None => error,
    }
}

pub(crate) fn ensure_terminal() -> Result<()> {
    ensure!(
        io::stdin().is_terminal() && io::stdout().is_terminal(),
        "dashboard needs a terminal; use mustr run --headless"
    );
    Ok(())
}

pub(crate) async fn run(
    service: Service,
    color: bool,
    mut profile: Option<ProfileUi>,
) -> Result<()> {
    let mut profile_result = None;
    if let Err(error) = ensure_terminal() {
        profile_result = cancel_and_collect_profile(&mut profile).await;
        return Err(attach_profile_result(error, profile_result.as_ref()));
    }
    // color-eyre's panic hook is installed first; Ratatui wraps it to restore terminal state.
    let mut terminal = match ratatui::try_init() {
        Ok(terminal) => terminal,
        Err(error) => {
            profile_result = cancel_and_collect_profile(&mut profile).await;
            return Err(attach_profile_result(error.into(), profile_result.as_ref()));
        }
    };
    let result = event_loop(
        &mut terminal,
        service,
        color,
        &mut profile,
        &mut profile_result,
    )
    .await;
    if result.is_err()
        && let Some(profile) = &profile
    {
        profile.cancel.send_replace(true);
    }
    let restored = ratatui::try_restore();
    if profile_result.is_none() {
        profile_result = collect_profile_result(&mut profile).await;
    }
    result.map_err(|error| attach_profile_result(error, profile_result.as_ref()))?;
    restored.map_err(|error| attach_profile_result(error.into(), profile_result.as_ref()))?;
    Ok(())
}

enum ProfileSignal {
    Progress(Option<ProfileProgress>),
    Coordinator(ProfileCoordinatorResult),
}

async fn next_profile_signal(profile: &mut ProfileUi, progress_open: bool) -> ProfileSignal {
    let progress = &mut profile.progress;
    let coordinator = profile.coordinator.as_mut();
    tokio::select! {
        event = async {
            if progress_open {
                progress.recv().await
            } else {
                std::future::pending().await
            }
        } => ProfileSignal::Progress(event),
        result = async {
            match coordinator {
                Some(coordinator) => coordinator.await,
                None => std::future::pending().await,
            }
        } => ProfileSignal::Coordinator(result),
    }
}

async fn event_loop(
    terminal: &mut ratatui::DefaultTerminal,
    service: Service,
    color: bool,
    profile: &mut Option<ProfileUi>,
    profile_result: &mut Option<ProfileCoordinatorResult>,
) -> Result<()> {
    let mut events = EventStream::new();
    let mut snapshots = service.snapshots.clone();
    let mut view = View::in_space(snapshots.borrow().space_id.clone());
    if profile.is_some() {
        view.profile = Some(ProfileView::default());
    }
    let mut profile_complete = profile.is_none();
    let mut profile_progress_open = true;
    let mut snapshots_open = true;
    let mut exit_requested = false;
    let mut cancel_sent = false;
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    loop {
        if exit_requested && !cancel_sent {
            if !profile_complete && let Some(profile) = profile.as_mut() {
                profile.cancel.send_replace(true);
                if let Some(view) = view.profile.as_mut() {
                    view.cancelling = true;
                }
            }
            cancel_sent = true;
        }
        let snapshot = snapshots.borrow().clone();
        view.reconcile(&snapshot);
        terminal.draw(|frame| draw(frame, &snapshot, &mut view, color))?;
        if exit_requested && profile_complete {
            break;
        }
        tokio::select! {
            _ = tokio::signal::ctrl_c(), if !exit_requested => { exit_requested = true; },
            _ = terminate.recv(), if !exit_requested => { exit_requested = true; },
            _ = tick.tick() => {},
            changed = snapshots.changed(), if snapshots_open => {
                if changed.is_err() {
                    snapshots_open = false;
                    exit_requested = true;
                }
            },
            signal = async {
                match profile.as_mut() {
                    Some(profile) => next_profile_signal(profile, profile_progress_open).await,
                    None => std::future::pending().await,
                }
            }, if !profile_complete => match signal {
                ProfileSignal::Progress(Some(progress)) => {
                    if let Some(view) = view.profile.as_mut() { view.apply(progress); }
                }
                ProfileSignal::Progress(None) => profile_progress_open = false,
                ProfileSignal::Coordinator(result) => {
                    profile_complete = true;
                    *profile_result = Some(result);
                    if let Some(view) = view.profile.as_mut() {
                        match profile_result.as_ref().expect("coordinator result was retained") {
                            Ok(Ok(outcomes)) => view.replace_outcomes(outcomes.clone()),
                            Ok(Err(_)) | Err(_) => view.complete_error(),
                        }
                    }
                }
            },
            event = events.next(), if !exit_requested => match event {
                Some(Ok(Event::Key(key))) if key.kind == KeyEventKind::Press => {
                    if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
                        exit_requested = true;
                        continue;
                    }
                    if let Some(action) = view.reset_key(key.code, &snapshot) {
                        match action {
                            reset_ui::Action::None => {},
                            reset_ui::Action::Close => view.reset_dialog = None,
                            reset_ui::Action::Start(preview) => {
                                let space = preview.space_id.clone();
                                view.reset_dialog = Some(match service.reset_start(preview).await {
                                    Ok(()) => reset_ui::Dialog::progress(space),
                                    Err(error) => reset_ui::Dialog::error(error.to_string()),
                                });
                            },
                            reset_ui::Action::Abandon(space, plan) => {
                                view.reset_dialog = Some(match service.reset_abandon(plan).await {
                                    Ok(()) => reset_ui::Dialog::progress(space),
                                    Err(error) => reset_ui::Dialog::error(error.to_string()),
                                });
                            },
                        }
                        continue;
                    }
                    if let Some(action) = view.pr_key(key.code) {
                        match action {
                            pr_ui::Action::None => {},
                            pr_ui::Action::Close => view.pr_dialog = None,
                            pr_ui::Action::Select(selection) => {
                                view.pr_dialog = None;
                                let name = clean(&selection.name);
                                view.notice = match service.enable_pr(*selection).await {
                                    Ok(()) => format!("PR discovery started for {name}; pending status appears above"),
                                    Err(error) => {
                                        view.pr_dialog = Some(pr_ui::Dialog::error(format!("Could not enable PR discovery: {error}. Refresh status and retry only after checking the bound participant.")));
                                        String::new()
                                    }
                                };
                            },
                        }
                        continue;
                    }
                    match key.code {
                        KeyCode::Char('r') => {
                            view.reset_dialog = Some(if view.reset(&snapshot).is_some_and(|plan| matches!(plan.state, ResetPhase::Prepared | ResetPhase::Incomplete)) {
                                reset_ui::Dialog::progress(view.space_id.clone())
                            } else {
                                match service.reset_preview().await {
                                    Ok(preview) if preview.space_id == view.space_id => reset_ui::Dialog::confirm(preview),
                                    Ok(_) => reset_ui::Dialog::error("Reset preview did not match this dashboard's space.".into()),
                                    Err(error) => reset_ui::Dialog::error(error.to_string()),
                                }
                            });
                        },
                        KeyCode::Char('q') => exit_requested = true,
                        KeyCode::Char('[') if view.profile.is_some() => {
                            if let Some(profile) = view.profile.as_mut() { profile.move_scroll(false); }
                        }
                        KeyCode::Char(']') if view.profile.is_some() => {
                            if let Some(profile) = view.profile.as_mut() { profile.move_scroll(true); }
                        }
                        KeyCode::Char('?') => view.help = !view.help,
                        KeyCode::Esc => view.help = false,
                        code if view.navigate(code, &snapshot) => {},
                        KeyCode::Char('t' | 'c') => {
                            let automation = &snapshot.automation;
                            let (mode, enabled) = if key.code == KeyCode::Char('t') {
                                (Mode::Todo, !automation.todo_enabled)
                            } else {
                                (Mode::Checkin, !automation.checkin_enabled)
                            };
                            view.notice = match service.mode(mode, enabled).await {
                                Ok(()) => format!("{mode:?} {} for this space; already-dispatched work is not cancelled", if enabled { "on" } else { "off" }),
                                Err(error) => error.to_string(),
                            };
                        }
                        KeyCode::Char('p') => {
                            view.notice.clear();
                            if snapshot.automation.pr.is_some() {
                                let enabled = !snapshot.automation.pr_enabled;
                                match service.mode(Mode::Pr, enabled).await {
                                    Ok(()) => {
                                        view.notice = format!("PR watcher {} for this space; already-dispatched work is not cancelled", if enabled { "on" } else { "off" });
                                    }
                                    Err(error) => {
                                        view.notice.clear();
                                        view.pr_dialog = Some(pr_ui::Dialog::error(format!("Could not update the PR watcher: {error}")));
                                    }
                                }
                            } else {
                                let candidates = pr_ui::eligible(&snapshot, &view.space_id);
                                match candidates.len() {
                                    0 => view.pr_dialog = Some(pr_ui::Dialog::error(
                                        "No connected, unpaused participant has advertised an absolute checkout path. Join Pi with /role NAME or Claude with mustr_join from the repository checkout, then press p.".into(),
                                    )),
                                    1 => {
                                        let selection = candidates.into_iter().next().expect("one candidate");
                                        let name = clean(&selection.name);
                                        view.notice = match service.enable_pr(selection).await {
                                            Ok(()) => format!("PR discovery started for {name}; pending status appears above"),
                                            Err(error) => {
                                                view.pr_dialog = Some(pr_ui::Dialog::error(format!("Could not enable PR discovery: {error}. Refresh status and retry only after checking the bound participant.")));
                                                String::new()
                                            }
                                        };
                                    }
                                    _ => view.pr_dialog = Some(pr_ui::Dialog::choose(candidates)),
                                }
                            }
                        }
                        _ => {},
                    }
                }
                Some(Err(error)) => return Err(error.into()),
                None => exit_requested = true,
                _ => {},
            }
        }
        if profile_complete && let Some(profile) = profile.as_mut() {
            profile.coordinator.take();
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

fn mode_spans(key: char, name: &str, on: bool, color: bool) -> Vec<Span<'static>> {
    vec![
        Span::styled(format!("[{key}] "), muted()),
        Span::styled(format!("{name}: "), bold()),
        // State is distinct from both the label and the keyboard shortcut.
        // Reserve the same width so toggling a mode never shifts the layout.
        Span::styled(
            if on { "ON " } else { "off" },
            if on {
                tone(color, Color::Green)
            } else {
                Style::default()
            },
        ),
    ]
}

fn draw(frame: &mut Frame, snapshot: &Snapshot, view: &mut View, color: bool) {
    let area = frame.area();
    frame.render_widget(Block::default().style(Style::reset()), area);
    if area.width < 35 || area.height < 12 {
        if let Some(dialog) = &mut view.reset_dialog {
            dialog.hide();
        }
        if let Some(dialog) = &mut view.pr_dialog {
            dialog.hide();
        }
        frame.render_widget(
            Paragraph::new(if view.reset_dialog.is_some() || view.pr_dialog.is_some() {
                "mustr · enlarge terminal (35×12 minimum)\nEsc closes dialog; Ctrl-C quits"
            } else {
                "mustr · enlarge terminal (35×12 minimum)\nq quits"
            }),
            area,
        );
        return;
    }
    let area = Rect::new(area.x + 1, area.y, area.width - 2, area.height);
    let automation = &snapshot.automation;
    let mut mode_lines = if view.help {
        Vec::new()
    } else {
        automation_lines(snapshot, area.width, color)
    };
    let spacious = area.height >= 20 && !view.help;
    // At the supported minimum size, retain the mode controls but sacrifice
    // watcher details before squeezing away the work pane's tabs/count border.
    let reserved = if spacious { 10 } else { 8 } + u16::from(area.height >= 18);
    mode_lines.truncate(usize::from(area.height.saturating_sub(reserved)));
    let header_height = if spacious { 3 } else { 2 };
    let top_height = header_height
        + u16::from(spacious)
        + height(mode_lines.len())
        + u16::from(area.height >= 18);
    let footer_budget = usize::from(area.height.saturating_sub(top_height.saturating_add(1)));
    let priority = footer_priority_status(snapshot, view, color);
    let profile_hints = if view.profile.is_some() {
        profile_footer_hints(snapshot, view, color)
    } else {
        Vec::new()
    };
    let hint_lines = profile_hints
        .iter()
        .map(|hint| hint.width().max(1).div_ceil(usize::from(area.width.max(1))))
        .sum::<usize>();
    let footer_height = if let Some(profile) = &mut view.profile {
        let notice_lines = usize::from(priority.is_none() && !view.notice.is_empty());
        let base_lines = 1 + usize::from(priority.is_some()) + notice_lines + hint_lines;
        let available = footer_budget.saturating_sub(base_lines);
        let visible = profile.visible_range(area.width, available);
        let result_lines = visible
            .map(|index| profile.row_height(index, area.width))
            .sum::<usize>();
        height((base_lines + result_lines).max(2))
    } else {
        2
    };
    let [header, _, modes, _, body, footer] = Layout::vertical([
        Constraint::Length(header_height),
        Constraint::Length(u16::from(spacious)),
        Constraint::Length(height(mode_lines.len())),
        Constraint::Length(u16::from(area.height >= 18)),
        Constraint::Min(1),
        Constraint::Length(footer_height),
    ])
    .areas(area);
    draw_header(frame, header, snapshot, view, color);
    frame.render_widget(Paragraph::new(mode_lines), modes);
    if view.help {
        draw_help(frame, body, color, view.profile.is_some());
    } else if view.space(snapshot).is_some() {
        let space = view.space_id.clone();
        draw_space(frame, body, snapshot, &space, automation, view, color);
    } else {
        frame.render_widget(
            Paragraph::new(vec![
                Line::styled(
                    format!(
                        "Bound space {} is unavailable.",
                        clean(view.space_id.as_str())
                    ),
                    bold(),
                ),
                Line::raw(""),
                Line::raw("This dashboard never switches spaces."),
                Line::raw("Reopen this space or launch mustr separately in another space."),
                Line::raw(""),
                Line::styled("No agent sessions are read or launched.", muted()),
            ])
            .wrap(Wrap { trim: false }),
            body,
        );
    }
    draw_footer(frame, footer, snapshot, view, color);
    if let Some(dialog) = &mut view.reset_dialog {
        dialog.draw(frame, snapshot);
    }
    if let Some(dialog) = &mut view.pr_dialog {
        dialog.draw(frame, color);
    }
}

fn draw_header(frame: &mut Frame, area: Rect, snapshot: &Snapshot, view: &View, color: bool) {
    let [brand, _, navigation] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(area.height.saturating_sub(2)),
        Constraint::Length(1),
    ])
    .areas(area);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("mustr", accent(color).add_modifier(Modifier::BOLD)),
            Span::styled("  /  agent coordination", muted()),
        ])),
        brand,
    );
    let [label, identity] =
        Layout::horizontal([Constraint::Length(7), Constraint::Min(1)]).areas(navigation);
    frame.render_widget(Paragraph::new("Space").style(bold()), label);
    let text = match view.space(snapshot) {
        Some(space) => format!(
            "{} · {}",
            clean(&space.label),
            clean(view.space_id.as_str())
        ),
        None => format!("{} · unavailable", clean(view.space_id.as_str())),
    };
    frame.render_widget(Paragraph::new(elide(&text, identity.width)), identity);
}

fn manager_line(snapshot: &Snapshot, width: u16, color: bool) -> Line<'static> {
    let mut managers = snapshot.members.iter().filter(|member| {
        member.space_id == snapshot.space_id
            && member.manager
            && member.activity != Activity::Offline
    });
    let manager = managers.next();
    let (text, warning) = if managers.next().is_some() {
        ("Manager: unavailable (multiple managers)".into(), true)
    } else if let Some(manager) = manager {
        let prefix = "Manager: ";
        let suffix = format!(" · {}", label(manager.activity));
        let fixed_width = Line::raw(prefix).width() + Line::raw(&suffix).width();
        let name_width = width.saturating_sub(u16::try_from(fixed_width).unwrap_or(u16::MAX));
        (
            format!(
                "{prefix}{}{suffix}",
                elide(&clean(&manager.name), name_width)
            ),
            manager.activity == Activity::Paused && snapshot.automation.checkin_enabled,
        )
    } else {
        (
            "Manager: none · join a manager=true role".into(),
            snapshot.automation.checkin_enabled,
        )
    };
    Line::styled(
        elide(&text, width),
        if warning {
            tone(color, Color::Yellow)
        } else {
            muted()
        },
    )
}

fn automation_lines(snapshot: &Snapshot, width: u16, color: bool) -> Vec<Line<'static>> {
    let automation = &snapshot.automation;
    let inline = width >= 76;
    let mut lines = Vec::new();
    let mut line = if inline {
        Line::from(Span::styled("Automation   ", bold()))
    } else {
        lines.push(Line::styled("Automation", bold()));
        Line::default()
    };
    let mut controls = 0;
    let separator = if width < 45 { " " } else { "   │   " };
    let separator_width = Line::raw(separator).width();
    for spans in [
        mode_spans('t', "TODO", automation.todo_enabled, color),
        mode_spans('c', "Check-in", automation.checkin_enabled, color),
        mode_spans('p', "PR", automation.pr_enabled, color),
    ] {
        let size: usize = spans.iter().map(Span::width).sum();
        if controls > 0 && line.width() + separator_width + size > usize::from(width) {
            lines.push(line);
            line = Line::default();
            controls = 0;
        }
        if controls > 0 {
            line.spans.push(Span::styled(separator, muted()));
        }
        line.spans.extend(spans);
        controls += 1;
    }
    lines.push(line);
    lines.push(manager_line(snapshot, width, color));
    let mut details = Line::default();
    if let Some(todo) = &automation.todo {
        details.spans.extend([
            Span::styled("TODO open: ", muted()),
            Span::raw(format!("{}/{}", todo.remaining, todo.total)),
        ]);
    }
    let mut pr_status = Vec::new();
    if let Some(pr) = &automation.pr {
        if pr.url.is_some() {
            let spans = [
                Span::styled("PR poll: ", muted()),
                Span::raw(format!("{}s", pr.next_at.saturating_sub(now()))),
            ];
            if !details.spans.is_empty() {
                if details.width() + 7 + spans.iter().map(Span::width).sum::<usize>()
                    > usize::from(width)
                {
                    lines.push(details);
                    details = Line::default();
                } else {
                    details.spans.push(Span::styled("   │   ", muted()));
                }
            }
            details.spans.extend(spans);
            if let Some(url) = &pr.url {
                pr_status.push(Line::raw(format!(
                    "PR URL: {}",
                    elide(&clean(url), width.saturating_sub(8))
                )));
            }
            if let Some(error) = &pr.error {
                pr_status.push(Line::styled(
                    format!(
                        "PR error: {}",
                        elide(&clean(error), width.saturating_sub(10))
                    ),
                    tone(color, Color::Red),
                ));
            }
        } else if let Some(error) = &pr.error {
            pr_status.push(Line::styled(
                format!(
                    "PR discovery failed: {}",
                    elide(&clean(error), width.saturating_sub(22))
                ),
                tone(color, Color::Red),
            ));
        } else if automation.pr_enabled {
            pr_status.push(Line::styled(
                "PR discovery: pending",
                tone(color, Color::Yellow),
            ));
        } else {
            pr_status.push(Line::styled("PR discovery: off", muted()));
        }
        let root_label = if pr.url.is_some() {
            "PR root: "
        } else {
            "PR checkout: "
        };
        pr_status.push(Line::styled(
            format!(
                "{root_label}{}",
                elide(
                    &clean(&pr.cwd.to_string_lossy()),
                    width.saturating_sub(root_label.len() as u16)
                )
            ),
            muted(),
        ));
    }
    lines.extend(pr_status);
    if !details.spans.is_empty() {
        lines.push(details);
    }
    lines
}

// Continuous, faint outlines keep each region legible even in a sparse space.
// Dim the borders, not the headings; inherit the terminal palette in both themes.
fn section(frame: &mut Frame, area: Rect, name: &str, count: usize, hint: &str) -> Rect {
    let title = Line::from(vec![
        Span::styled(format!(" {name} "), bold()),
        Span::styled(format!("{count} "), muted()),
    ]);
    let mut block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .padding(Padding::horizontal(1))
        .border_style(muted())
        .title_style(Style::default().remove_modifier(Modifier::DIM))
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
    let [participants, work] = if area.width >= 96 && area.height >= 8 {
        let [participants, _, work] = Layout::horizontal([
            Constraint::Length((area.width / 3).clamp(33, 40)),
            Constraint::Length(1),
            Constraint::Min(1),
        ])
        .areas(area);
        [participants, work]
    } else {
        let wanted = if members.is_empty() {
            4
        } else {
            height(members.len() * 2 + 3)
        };
        let [participants, _, work] = Layout::vertical([
            Constraint::Length(
                wanted
                    .min((area.height / 3).max(2))
                    .min(area.height.saturating_sub(2)),
            ),
            Constraint::Length(u16::from(area.height >= 16)),
            Constraint::Min(2),
        ])
        .areas(area);
        [participants, work]
    };
    draw_members(frame, participants, &members, color);
    draw_work(frame, work, snapshot, space, automation, view, color);
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

// Reserve a line for an explicit overflow count instead of silently clipping the roster.
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
                Line::raw("Claude: mustr_join + mustr_wait."),
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
                    format!(
                        "{}{} · {}",
                        if member.manager { "manager · " } else { "" },
                        clean(&member.role),
                        clean(&member.pane_id)
                    ),
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

fn work_tabs(
    tab: WorkTab,
    task_count: usize,
    message_count: usize,
    width: u16,
    color: bool,
) -> Line<'static> {
    let messages = if width < 45 { "Msgs" } else { "Messages" };
    let mut line = Line::from(vec![
        Span::raw(" "),
        Span::styled(
            format!("{messages} {message_count}"),
            if tab == WorkTab::Messages {
                accent(color).add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
            } else {
                bold()
            },
        ),
        Span::styled(" │ ", muted()),
        Span::styled(
            format!("Tasks {task_count}"),
            if tab == WorkTab::Tasks {
                accent(color).add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
            } else {
                bold()
            },
        ),
        Span::raw(" "),
    ]);
    if line.width() + 4 <= usize::from(width.saturating_sub(2)) {
        line.spans.push(Span::styled("[f] ", muted()));
    }
    line
}

fn draw_work(
    frame: &mut Frame,
    area: Rect,
    snapshot: &Snapshot,
    space: &SpaceId,
    automation: &Automation,
    view: &mut View,
    color: bool,
) {
    let tasks = view.space_tasks(snapshot);
    let message_count = snapshot
        .messages
        .iter()
        .filter(|m| &m.space_id == space)
        .count();
    view.tasks.reconcile(&tasks);
    let mut block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .padding(Padding::horizontal(1))
        .border_style(muted())
        .title_style(Style::default().remove_modifier(Modifier::DIM))
        .title(work_tabs(
            view.work_tab,
            tasks.len(),
            message_count,
            area.width,
            color,
        ));
    let inner = block.inner(area);
    let range = if view.work_tab == WorkTab::Tasks {
        let row_height = task_row_height(&tasks).min(inner.height.max(1));
        view.tasks
            .viewport(tasks.len(), usize::from(inner.height / row_height))
    } else {
        0..0
    };
    // Only requester acceptance is completion: Review (reported Done) and
    // Cancelled must never inflate this number via TaskState::finished().
    let accepted = tasks
        .iter()
        .filter(|task| task.state == TaskState::Accepted)
        .count();
    let counts = format!("{} tasks · {accepted} accepted", tasks.len());
    let summary = if view.work_tab == WorkTab::Tasks && !tasks.is_empty() {
        let visible = if range.is_empty() {
            "0".into()
        } else {
            format!("{}–{}", range.start + 1, range.end)
        };
        let full = format!("{visible}/{counts}");
        if Line::raw(&full).width() + 2 <= usize::from(area.width.saturating_sub(2)) {
            full
        } else {
            // The tabs already label the task total, so omit only the repeated
            // noun on narrow panes, never the accepted count or visible range.
            format!("{visible}/{} · {accepted} accepted", tasks.len())
        }
    } else {
        counts
    };
    let hint = match view.work_tab {
        WorkTab::Messages => "addressed · newest first",
        WorkTab::Tasks => "↑↓/jk browse only",
    };
    if Line::raw(&summary).width() + Line::raw(hint).width() + 7 < usize::from(area.width) {
        block = block.title_bottom(Line::styled(format!(" {hint} "), muted()).right_aligned());
    }
    block = block.title_bottom(Line::raw(format!(" {summary} ")));
    frame.render_widget(block, area);
    match view.work_tab {
        WorkTab::Messages => draw_messages(frame, inner, snapshot, space, automation, view, color),
        WorkTab::Tasks => draw_tasks(frame, inner, snapshot, &tasks, &view.tasks, range, color),
    }
}

fn task_row_height(tasks: &[&Task]) -> u16 {
    if tasks.iter().any(|t| t.state == TaskState::Paused) {
        4
    } else {
        2
    }
}

fn draw_tasks(
    frame: &mut Frame,
    inner: Rect,
    snapshot: &Snapshot,
    tasks: &[&Task],
    browse: &TaskBrowse,
    range: std::ops::Range<usize>,
    color: bool,
) {
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
    let rows = tasks[range].iter().map(|task| {
        let selected = browse.selected.as_ref() == Some(&task.id);
        let owner = snapshot
            .members
            .iter()
            .find(|m| m.id == task.owner)
            .map(|m| m.name.as_str())
            .unwrap_or(task.owner.as_str());
        let requester = snapshot
            .members
            .iter()
            .find(|m| m.id == task.requester)
            .map(|m| m.name.as_str())
            .unwrap_or(task.requester.as_str());
        let responsibility = match task.state {
            TaskState::Review => format!("Review by {} · ", clean(requester)),
            TaskState::Blocked => format!("Ask {} · ", clean(requester)),
            _ => String::new(),
        };
        let mut details = vec![
            Line::styled(
                clean(&task.title),
                if selected {
                    bold().add_modifier(Modifier::UNDERLINED)
                } else {
                    bold()
                },
            ),
            Line::styled(
                format!(
                    "{responsibility}{} · {} r{}",
                    clean(owner),
                    clean(task.id.as_str()).chars().take(8).collect::<String>(),
                    task.revision
                ),
                muted(),
            ),
        ];
        if task.state == TaskState::Paused {
            details.push(Line::styled(
                elide(&clean(&task.note), inner.width.saturating_sub(12)),
                tone(color, Color::Yellow),
            ));
            details.push(Line::styled(
                format!("Ask {}: reconcile, then guide/cancel", clean(requester)),
                tone(color, Color::Yellow),
            ));
        }
        Row::new([
            Cell::from(if selected { "›" } else { " " })
                .style(accent(color).add_modifier(Modifier::BOLD)),
            Cell::from(label(task.state)).style(task_style(task.state, color)),
            Cell::from(Text::from(details)),
        ])
        .height(row_height)
    });
    frame.render_widget(
        Table::new(
            rows,
            [
                Constraint::Length(1),
                Constraint::Length(9),
                Constraint::Min(1),
            ],
        )
        .column_spacing(1),
        inner,
    );
}

fn draw_messages(
    frame: &mut Frame,
    inner: Rect,
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
            "Join with /role NAME in Pi or mustr_join in Claude; Claude receives during mustr_wait.",
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

fn draw_help(frame: &mut Frame, area: Rect, color: bool, profile: bool) {
    let mut controls = vec![
        Line::styled("Keyboard", accent(color).add_modifier(Modifier::BOLD)),
        Line::raw("f               Messages / Tasks"),
        Line::raw("↑ ↓ / k j       scroll active tab"),
    ];
    if profile {
        controls.push(Line::raw("[ / ]           browse profile startup results"));
    }
    controls.extend([
        Line::raw("t  TODO / continuation on/off"),
        Line::raw("c  manager check-ins on/off"),
        Line::raw("p  PR watcher on/off or discover"),
        Line::raw("r  start fresh / reset status (confirmation required)"),
        Line::raw("? / Esc         close help"),
        Line::raw("q / Ctrl-C      stop hub"),
        Line::raw(""),
        Line::raw("One dashboard per space;"),
        Line::raw("launch mustr in other spaces."),
    ]);
    let workflow = vec![
        Line::styled(
            "Connect your workflow",
            accent(color).add_modifier(Modifier::BOLD),
        ),
        Line::raw("Join: Pi /role NAME; Claude mustr_join."),
        Line::raw("p discovers from current checkout when no PR is attached."),
        Line::raw("Explicit attach (Pi recipient):"),
        Line::raw("/mustr-todo [path/to/TODO.md]"),
        Line::raw("/mustr-pr [PR URL]"),
        Line::raw("https://github.com/OWNER/REPO/pull/N"),
        Line::raw("Check-in saved; TODO/PR restart off."),
        Line::raw("Off cancels pending; not running work."),
        Line::styled("Activity ≠ completion. Done ≠ accepted.", bold()),
        Line::raw("Accepted tasks complete; cancelled are not."),
        Line::raw("Task highlight is browse-only; no actions."),
        Line::raw("Quit leaves agents/external jobs running."),
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

fn footer_priority_status(snapshot: &Snapshot, view: &View, color: bool) -> Option<Line<'static>> {
    let error = snapshot.error.as_deref().filter(|s| !s.is_empty());
    if let Some(error) = error {
        Some(Line::styled(
            format!("! {}", clean(error)),
            tone(color, Color::Red).add_modifier(Modifier::BOLD),
        ))
    } else if view.space(snapshot).is_none() {
        Some(Line::styled(
            format!(
                "! Space {} unavailable; dashboard remains bound",
                clean(view.space_id.as_str())
            ),
            tone(color, Color::Yellow).add_modifier(Modifier::BOLD),
        ))
    } else if view
        .reset(snapshot)
        .is_some_and(|reset| matches!(reset.state, ResetPhase::Prepared | ResetPhase::Incomplete))
    {
        Some(Line::styled(
            "Reset incomplete · r for status",
            tone(color, Color::Yellow).add_modifier(Modifier::BOLD),
        ))
    } else {
        None
    }
}

fn profile_footer_hints(snapshot: &Snapshot, view: &View, color: bool) -> Vec<Line<'static>> {
    let mut hints = Vec::new();
    if snapshot
        .tasks
        .iter()
        .any(|task| task.space_id == view.space_id && task.state == TaskState::Paused)
    {
        hints.push(Line::styled(
            "Paused task: requester must guide/cancel; automation will not resume it",
            tone(color, Color::Yellow),
        ));
    }
    hints.push(Line::styled(
        "q cancels unfinished profile startup; launched agents stay open",
        muted(),
    ));
    hints
}

fn idle_footer_status(snapshot: &Snapshot, view: &View, width: u16, color: bool) -> Line<'static> {
    if !view.notice.is_empty() {
        Line::raw(clean(&view.notice))
    } else if snapshot
        .tasks
        .iter()
        .any(|task| task.space_id == view.space_id && task.state == TaskState::Paused)
    {
        Line::styled(
            if width < 60 {
                "Paused task: ask requester to guide"
            } else {
                "Paused work: requester must reconcile then guide/cancel; automation will not resume it"
            },
            tone(color, Color::Yellow),
        )
    } else {
        Line::styled(
            if width < 60 {
                "Quit leaves agents open."
            } else {
                "Check-in follows manager · activity is not completion · quit leaves agents open"
            },
            muted(),
        )
    }
}

fn draw_footer(frame: &mut Frame, area: Rect, snapshot: &Snapshot, view: &View, color: bool) {
    let keys = if view.help {
        "? / Esc close help · q quit"
    } else if view.profile.is_some() {
        "f view · [/] profile results · r reset · ? help · q quit"
    } else if area.width < 64 {
        "f view r reset ? help q quit"
    } else if area.width < 96 {
        "f view · ↑↓/jk scroll · r reset · ? help · q quit"
    } else {
        "f Msgs/Tasks  ↑↓/jk scroll  t TODO c Check-ins p PR on/off/discover  r reset  ? help  q quit"
    };
    let mut lines = vec![Line::styled(elide(keys, area.width), muted())];
    let priority = footer_priority_status(snapshot, view, color);
    let has_priority = priority.is_some();
    if let Some(status) = priority {
        lines.push(status);
    }
    if let Some(profile) = &view.profile {
        lines.extend(profile_footer_hints(snapshot, view, color));
        for index in profile.visible.clone() {
            lines.push(Line::raw(profile.row_text(index)));
        }
        if !view.notice.is_empty() && !has_priority {
            lines.push(Line::raw(clean(&view.notice)));
        }
    } else if !has_priority {
        lines.push(idle_footer_status(snapshot, view, area.width, color));
    }
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile_startup::{EntryOutcome, EntryState, ProfileProgress};
    use ratatui::{Terminal, backend::TestBackend, buffer::Buffer};

    mod manager;
    mod pr;
    mod reliability;
    mod reset;
    mod work_tabs;

    fn empty_space() -> Snapshot {
        Snapshot {
            space_id: SpaceId::new("w1"),
            space: Some(Space {
                id: SpaceId::new("w1"),
                label: "Orchard".into(),
            }),
            ..Snapshot::empty(SpaceId::new("w1"))
        }
    }

    fn populated() -> Snapshot {
        let mut snapshot = empty_space();
        snapshot.members = vec![
            Member {
                id: AgentId::new("ada"),
                name: "Ada".into(),
                role: "architect".into(),
                manager: true,
                pane_id: "w1:p1".into(),
                space_id: SpaceId::new("w1"),
                activity: Activity::Idle,
                last_seen: 0,
                idle_since: 0,
                cwd: Some(std::path::PathBuf::from("/workspace/orchard")),
            },
            Member {
                id: AgentId::new("lin"),
                name: "Lin".into(),
                role: "implementor".into(),
                manager: false,
                pane_id: "w1:p2".into(),
                space_id: SpaceId::new("w1"),
                activity: Activity::Working,
                last_seen: 0,
                idle_since: 0,
                cwd: Some(std::path::PathBuf::from("/workspace/orchard")),
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
        snapshot.automation = Automation {
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
                url: Some("https://github.com/example/project/pull/42".into()),
                cwd: ".".into(),
                driver: AgentId::new("ada"),
                branch: None,
                head: None,
                baseline: None,
                next_at: 0,
                error: None,
            }),
            ..Automation::default()
        };
        snapshot
    }

    fn selected_view(snapshot: &Snapshot) -> View {
        let mut view = View::in_space(snapshot.space_id.clone());
        view.reconcile(snapshot);
        view
    }

    fn render(snapshot: &Snapshot, view: &View, width: u16, height: u16, color: bool) -> Buffer {
        render_view(snapshot, &mut view.clone(), width, height, color)
    }

    fn render_view(
        snapshot: &Snapshot,
        view: &mut View,
        width: u16,
        height: u16,
        color: bool,
    ) -> Buffer {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| draw(frame, snapshot, view, color))
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
    fn profile_outcomes_are_all_inspectable_in_order_in_the_existing_footer() {
        let mut snapshot = populated();
        snapshot.tasks[0].state = TaskState::Paused;
        let roles = ["reviewer", "researcher", "reviewer", "implementor"];
        let mut profile = ProfileView::default();
        for (index, role) in roles.iter().enumerate() {
            profile.apply(ProfileProgress::Starting {
                index: index + 1,
                total: roles.len(),
                role: (*role).into(),
            });
            profile.apply(ProfileProgress::Finished(EntryOutcome {
                role: (*role).into(),
                pane_id: Some(format!("w1:p{}", index + 9)),
                state: if index == 2 {
                    EntryState::Failed
                } else {
                    EntryState::Joined
                },
                reason: if index == 2 {
                    "not attempted after agent rejection"
                } else {
                    "new member admitted"
                }
                .into(),
            }));
        }
        let mut view = selected_view(&snapshot);
        view.profile = Some(profile);

        let text = screen(&render(&snapshot, &view, 120, 24, false));
        let first = text.find("reviewer").expect("first role appears");
        let second = text.find("researcher").expect("second role appears");
        let third = text
            .rfind("reviewer")
            .expect("repeated role appears independently");
        let fourth = text.find("implementor").expect("fourth role appears");
        assert!(first < second && second < third && third < fourth, "{text}");
        for pane in ["w1:p9", "w1:p10", "w1:p11", "w1:p12"] {
            assert!(text.contains(pane), "missing pane {pane} in:\\n{text}");
        }
        assert!(
            text.contains("not attempted after agent rejection"),
            "{text}"
        );
        assert!(text.contains("new member admitted"), "{text}");
        assert!(
            text.contains("Paused task: requester must guide/cancel"),
            "{text}"
        );
        assert!(
            text.contains("q cancels unfinished profile startup; launched agents stay open"),
            "{text}"
        );
        assert!(
            text.contains("["),
            "profile footer should advertise outcome navigation: {text}"
        );
    }

    #[test]
    fn profile_footer_keeps_rows_and_hints_beside_priority_status() {
        let mut base = populated();
        base.tasks[0].state = TaskState::Paused;
        let mut profile = ProfileView::default();
        for (index, role) in ["researcher", "reviewer"].iter().enumerate() {
            profile.apply(ProfileProgress::Starting {
                index: index + 1,
                total: 2,
                role: (*role).into(),
            });
            profile.apply(ProfileProgress::Finished(EntryOutcome {
                role: (*role).into(),
                pane_id: Some(format!("w1:p{}", index + 9)),
                state: EntryState::Uncertain,
                reason: "admission remains uncertain".into(),
            }));
        }

        let mut error = base.clone();
        error.error = Some("fixture connection failed".into());
        let mut unavailable = base.clone();
        unavailable.space = None;
        let mut reset = base;
        reset.reset = Some(ResetSummary {
            plan_id: "plan".into(),
            space_id: SpaceId::new("w1"),
            state: ResetPhase::Prepared,
            participants: Vec::new(),
            note: "waiting for participants".into(),
        });

        for (snapshot, priority) in [
            (error, "! fixture connection failed"),
            (
                unavailable,
                "! Space w1 unavailable; dashboard remains bound",
            ),
            (reset, "Reset incomplete · r for status"),
        ] {
            let mut view = selected_view(&snapshot);
            view.profile = Some(profile.clone());
            let text = screen(&render(&snapshot, &view, 120, 30, false));
            assert!(
                text.contains(priority),
                "missing priority {priority:?}:\n{text}"
            );
            assert!(
                text.contains(
                    "Paused task: requester must guide/cancel; automation will not resume it"
                ),
                "{text}"
            );
            assert!(
                text.contains("q cancels unfinished profile startup; launched agents stay open"),
                "{text}"
            );
            assert!(text.contains("[/] profile results"), "{text}");
            for row in [
                "researcher",
                "reviewer",
                "w1:p9",
                "w1:p10",
                "admission remains uncertain",
            ] {
                assert!(
                    text.contains(row),
                    "missing profile content {row:?}:\n{text}"
                );
            }
            assert!(
                view.reset_dialog.is_none(),
                "priority status must not open an overlay"
            );
            assert!(
                !text.contains("Start fresh?"),
                "reset priority is not a confirmation panel:\n{text}"
            );
        }
    }

    #[test]
    fn profile_footer_scroll_handler_changes_and_clamps_the_small_terminal_row_range() {
        let mut base = populated();
        base.tasks[0].state = TaskState::Paused;
        base.automation = Automation::default();

        let mut error = base.clone();
        error.error = Some("fixture connection failed".into());
        let mut reset = base;
        reset.reset = Some(ResetSummary {
            plan_id: "plan".into(),
            space_id: SpaceId::new("w1"),
            state: ResetPhase::Prepared,
            participants: Vec::new(),
            note: "waiting for participants".into(),
        });

        let mut profile = ProfileView::default();
        for index in 0..8 {
            let role = format!("role-{index}");
            profile.apply(ProfileProgress::Starting {
                index: index + 1,
                total: 8,
                role: role.clone(),
            });
            profile.apply(ProfileProgress::Finished(EntryOutcome {
                role,
                pane_id: Some(format!("w1:p{}", index + 90)),
                state: EntryState::Joined,
                reason: "admitted".into(),
            }));
        }

        for (snapshot, priority) in [
            (error, "! fixture connection failed"),
            (reset, "Reset incomplete · r for status"),
        ] {
            let assert_footer_context = |text: &str, view: &View| {
                assert!(text.contains(priority), "{text}");
                assert!(
                    text.contains(
                        "Paused task: requester must guide/cancel; automation will not resume it"
                    ),
                    "{text}"
                );
                assert!(
                    text.contains(
                        "q cancels unfinished profile startup; launched agents stay open"
                    ),
                    "{text}"
                );
                assert!(text.contains("[/] profile results"), "{text}");
                assert!(!text.contains("Start fresh?"), "{text}");
                assert!(view.reset_dialog.is_none() && view.pr_dialog.is_none());
            };
            let mut view = selected_view(&snapshot);
            view.profile = Some(profile.clone());
            let first_buffer = render_view(&snapshot, &mut view, 80, 12, false);
            let first_text = screen(&first_buffer);
            let first_range = view.profile.as_ref().unwrap().visible.clone();

            assert_eq!(
                first_range.start, 0,
                "the footer starts at its first result"
            );
            assert!(
                first_range.end < 8,
                "the small terminal forces footer overflow"
            );
            assert!(first_text.contains("role-0"), "{first_text}");
            assert_footer_context(&first_text, &view);

            // This is the same row-scroll handler dispatched by the event loop's `]` key.
            view.profile.as_mut().unwrap().move_scroll(true);
            let next_buffer = render_view(&snapshot, &mut view, 80, 12, false);
            let next_text = screen(&next_buffer);
            let next_range = view.profile.as_ref().unwrap().visible.clone();
            assert_eq!(next_range.start, first_range.start + 1);
            assert!(next_text.contains("role-1"), "{next_text}");
            assert!(!next_text.contains("role-0"), "{next_text}");
            assert_footer_context(&next_text, &view);

            for _ in 0..10 {
                view.profile.as_mut().unwrap().move_scroll(true);
            }
            let bottom_buffer = render_view(&snapshot, &mut view, 80, 12, false);
            let bottom_text = screen(&bottom_buffer);
            let bottom_range = view.profile.as_ref().unwrap().visible.clone();
            assert!(bottom_range.start > 0);
            assert_eq!(bottom_range.end, 8, "the last result remains reachable");
            assert!(bottom_text.contains("role-7"), "{bottom_text}");
            assert_footer_context(&bottom_text, &view);

            view.profile.as_mut().unwrap().move_scroll(true);
            let clamped_buffer = render_view(&snapshot, &mut view, 80, 12, false);
            assert_eq!(view.profile.as_ref().unwrap().visible, bottom_range);
            assert_eq!(
                screen(&clamped_buffer),
                bottom_text,
                "downward scrolling clamps at the last result"
            );
            assert_footer_context(&screen(&clamped_buffer), &view);

            for _ in 0..10 {
                view.profile.as_mut().unwrap().move_scroll(false);
            }
            let top_buffer = render_view(&snapshot, &mut view, 80, 12, false);
            let top_range = view.profile.as_ref().unwrap().visible.clone();
            assert_eq!(top_range.start, 0);
            let top_text = screen(&top_buffer);
            assert!(top_text.contains("role-0"));
            assert_footer_context(&top_text, &view);

            view.profile.as_mut().unwrap().move_scroll(false);
            let clamped_top_buffer = render_view(&snapshot, &mut view, 80, 12, false);
            assert_eq!(view.profile.as_ref().unwrap().visible, top_range);
            assert_eq!(
                screen(&clamped_top_buffer),
                top_text,
                "upward scrolling clamps at the first result"
            );
        }
    }

    #[tokio::test]
    async fn terminal_failure_cancels_drains_and_preserves_profile_outcomes() {
        let (_progress_tx, progress) = tokio::sync::mpsc::unbounded_channel();
        let (cancel, mut cancelled) = tokio::sync::watch::channel(false);
        let coordinator = tokio::spawn(async move {
            cancelled
                .changed()
                .await
                .expect("TUI failure must cancel profile startup");
            Ok(vec![EntryOutcome {
                role: "researcher".into(),
                pane_id: Some("w1:p9".into()),
                state: EntryState::Uncertain,
                reason: "agent start remained ambiguous after launch cancellation".into(),
            }])
        });
        let mut profile = Some(ProfileUi {
            progress,
            cancel,
            coordinator: Some(coordinator),
        });

        let completed = cancel_and_collect_profile(&mut profile)
            .await
            .expect("TUI failure drains the owned profile coordinator");
        let error = attach_profile_result(
            color_eyre::eyre::eyre!("terminal initialization failed"),
            Some(&completed),
        );
        let diagnostic = format!("{error:#}");

        assert!(profile.as_ref().unwrap().coordinator.is_none());
        assert!(diagnostic.contains("terminal initialization failed"));
        assert!(diagnostic.contains("profile coordinator outcomes"));
        assert!(diagnostic.contains("researcher"));
        assert!(diagnostic.contains("Uncertain"));
        assert!(diagnostic.contains("launch cancellation"));
        assert!(!diagnostic.contains("private-startup-token-marker"));
    }

    #[test]
    fn empty_space_has_shared_tabs_and_actionable_hints() {
        let snapshot = empty_space();
        let mut view = selected_view(&snapshot);
        let text = screen(&render(&snapshot, &view, 100, 24, true));
        assert!(text.contains("mustr  /  agent coordination"));
        assert!(text.contains("Orchard · w1"));
        assert!(text.contains("[t] TODO: off   │   [c] Check-in: ON    │   [p] PR: off"));
        assert!(text.contains("/role NAME in Pi to join."));
        assert!(text.contains("Claude: mustr_join"));
        assert!(text.contains("No addressed messages yet."));
        assert!(!text.contains("No tasks assigned."));
        let (_, roster_row) = position(&text, "Participants 0");
        let (_, task_row) = position(&text, "Tasks 0");
        let (_, message_row) = position(&text, "Messages 0");
        assert_eq!(roster_row, task_row);
        assert_eq!(message_row, task_row);
        assert!(view.navigate(KeyCode::Char('f'), &snapshot));
        let tasks = screen(&render(&snapshot, &view, 100, 24, true));
        assert!(tasks.contains("No tasks assigned."));
        assert!(tasks.contains("A joined agent can assign work to an idle participant."));
        assert!(!tasks.contains("No addressed messages yet."));
        for text in [&text, &tasks] {
            assert!(text.contains("0 tasks · 0 accepted"));
            for corner in ['╭', '╮', '╰', '╯'] {
                assert_eq!(text.matches(corner).count(), 2, "{text}");
            }
        }
    }

    #[test]
    fn unavailable_space_keeps_fixed_identity_and_explains_separate_launches() {
        let snapshot = Snapshot::empty(SpaceId::new("w1"));
        let view = selected_view(&snapshot);
        let text = screen(&render(&snapshot, &view, 80, 24, true));
        for expected in [
            "w1 · unavailable",
            "Bound space w1 is unavailable.",
            "This dashboard never switches spaces.",
            "launch mustr separately in another space.",
            "No agent sessions are read or launched.",
        ] {
            assert!(text.contains(expected), "missing {expected:?}:\n{text}");
        }
        assert!(!text.contains("Participants"));
        assert!(!text.contains("Tasks 0"));
        assert!(!text.contains("Spaces"));
    }

    #[test]
    fn populated_wide_layout_preserves_the_status_and_message_hierarchy() {
        let snapshot = populated();
        let mut view = selected_view(&snapshot);
        let buffer = render(&snapshot, &view, 120, 30, true);
        let messages = screen(&buffer);
        view.navigate(KeyCode::Char('f'), &snapshot);
        let task_buffer = render(&snapshot, &view, 120, 30, true);
        let tasks_text = screen(&task_buffer);
        let text = format!("{messages}\n{tasks_text}");
        for (buffer, text, status, foreground) in [
            (&buffer, &messages, "working", Color::Cyan),
            (&task_buffer, &tasks_text, "active", Color::Cyan),
            (&task_buffer, &tasks_text, "blocked", Color::Red),
        ] {
            let (x, y) = position(text, status);
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
            "TODO open: 2/5   │   PR poll: 0s",
        ] {
            assert!(text.contains(expected), "missing {expected:?}:\n{text}");
        }
        let roster = position(&messages, "Participants 2");
        let tasks = position(&messages, "Tasks 2");
        let messages = position(&messages, "Messages 2");
        assert_eq!(roster.1, tasks.1);
        assert!(roster.0 < messages.0);
        assert_eq!(tasks.1, messages.1);
    }

    #[test]
    fn narrow_layout_stacks_sections_and_keeps_message_bodies_readable() {
        let snapshot = populated();
        let mut view = selected_view(&snapshot);
        let messages_text = screen(&render(&snapshot, &view, 56, 34, true));
        view.navigate(KeyCode::Char('f'), &snapshot);
        let tasks_text = screen(&render(&snapshot, &view, 56, 34, true));
        let roster = position(&messages_text, "Participants 2");
        let tasks = position(&messages_text, "Tasks 2");
        let messages = position(&messages_text, "Messages 2");
        assert_eq!(roster.0, messages.0);
        assert_eq!(tasks.1, messages.1);
        assert!(roster.1 < messages.1);
        let text = format!("{messages_text}\n{tasks_text}");
        for expected in [
            "architect · w1:p1",
            "implementor · w1:p2",
            "Refine dashboard",
            "Review runner logs",
            "Lin · a1b2c3d4 r2",
            "Ada → Lin · assignment · received",
            "Keep the dashboard compact and terminal-native.",
            "Waiting on an explicit review decision.",
            "[p] PR: off",
            "q quit",
        ] {
            assert!(text.contains(expected), "missing {expected:?}:\n{text}");
        }
    }

    #[test]
    fn no_color_resets_every_cell_and_color_uses_only_terminal_ansi_accents() {
        let snapshot = populated();
        for (width, height) in [(10, 4), (35, 12), (56, 34), (120, 30)] {
            for (help, tab) in [
                (false, WorkTab::Messages),
                (false, WorkTab::Tasks),
                (true, WorkTab::Messages),
            ] {
                let mut view = selected_view(&snapshot);
                view.help = help;
                view.work_tab = tab;
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
            let unwrapped = text.split_whitespace().collect::<Vec<_>>().join(" ");
            for expected in [
                "One dashboard per space;",
                "launch mustr in other spaces.",
                "↑ ↓ / k j",
                "f               Messages / Tasks",
                "scroll active tab",
                "t  TODO / continuation on/off",
                "c  manager check-ins on/off",
                "p  PR watcher on/off or discover",
                "q / Ctrl-C",
                "? / Esc",
                "/mustr-todo [path/to/TODO.md]",
                "/mustr-pr",
                "https://github.com/OWNER/REPO/pull/N",
                "p discovers from current checkout",
                "when no",
                "PR is attached.",
                "Check-in saved;",
                "TODO/PR restart off.",
                "not running work.",
                "Done ≠ accepted.",
                "Accepted tasks complete;",
                "cancelled are",
                "not.",
                "Task highlight is browse-only; no actions.",
                "Quit leaves agents/external jobs running.",
            ] {
                assert!(
                    text.contains(expected) || unwrapped.contains(expected),
                    "missing {expected:?}:\n{text}"
                );
            }
            assert!(!text.contains("Refine dashboard"));
            assert!(!text.contains("next space"));
            assert!(!text.contains("previous space"));
            assert!(!text.contains("Tab"));
        }
    }

    #[test]
    fn only_bound_space_data_is_rendered() {
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
        assert!(!view.navigate(KeyCode::Tab, &snapshot));
        assert_eq!(view.space_id, SpaceId::new("w1"));
        assert!(text.contains("2 tasks · 0 accepted"));
        assert!(text.contains("[t] TODO: ON"));
        view.navigate(KeyCode::Char('f'), &snapshot);
        let text = screen(&render(&snapshot, &view, 120, 30, false));
        assert!(!text.contains("FOREIGN_"), "{text}");
        assert!(text.contains("Refine dashboard"));
    }

    #[test]
    fn external_text_is_sanitized_including_identifiers_and_panes() {
        let mut snapshot = populated();
        snapshot.space.as_mut().unwrap().label = "Orc\u{1b}hard\n".into();
        snapshot.members[0].name = "A\u{7}da".into();
        snapshot.members[0].role = "archi\ttect".into();
        snapshot.members[0].pane_id = "w1:\u{1b}p1".into();
        snapshot.tasks[0].id = TaskId::new("a1b2\u{1b}c3d4");
        snapshot.tasks[0].title = "Refine\r dashboard".into();
        snapshot.messages[0].text = "Safe\u{1b}[31m text\nno bell\u{7}".into();
        snapshot.error = Some("dis\nconnected\u{7}".into());
        let mut view = selected_view(&snapshot);
        let buffer = render(&snapshot, &view, 120, 30, true);
        view.navigate(KeyCode::Char('f'), &snapshot);
        let task_buffer = render(&snapshot, &view, 120, 30, true);
        let text = format!("{}\n{}", screen(&buffer), screen(&task_buffer));
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
                .chain(task_buffer.content())
                .all(|cell| cell.symbol().chars().all(|ch| !ch.is_control()))
        );
    }

    #[test]
    fn watcher_errors_and_notices_remain_visible_and_sanitized() {
        let mut snapshot = populated();
        let automation = &mut snapshot.automation;
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
        snapshot.automation.pr = None;
        for index in 0..8 {
            let mut member = snapshot.members[0].clone();
            member.id = AgentId::new(format!("agent-{index}"));
            snapshot.members.push(member);
            let mut task = snapshot.tasks[0].clone();
            task.id = TaskId::new(format!("task-{index}"));
            snapshot.tasks.push(task);
        }
        let mut view = selected_view(&snapshot);
        view.navigate(KeyCode::Char('f'), &snapshot);
        let text = screen(&render(&snapshot, &view, 120, 18, false));
        assert!(text.contains("more participants"), "{text}");
        assert!(text.contains("1–4/10 tasks · 0 accepted"), "{text}");
        assert!(!text.contains("more tasks"));
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
    fn fixed_space_label_is_plain_and_elided_by_terminal_columns() {
        let mut snapshot = empty_space();
        snapshot.space.as_mut().unwrap().label = "Orchard with a very long label and suffix".into();
        let buffer = render(&snapshot, &selected_view(&snapshot), 40, 24, false);
        let text = screen(&buffer);
        let (x, y) = position(&text, "Orchard");
        assert!(
            !buffer[(height(x), height(y))]
                .modifier
                .contains(Modifier::UNDERLINED)
        );
        assert!(text.contains("Space  "), "{text}");
        assert!(
            text.contains(&elide(&snapshot.space.as_ref().unwrap().label, 31)),
            "{text}"
        );
        assert!(!text.contains("Spaces"));
        assert!(!text.contains("1/1"));
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
        for snapshot in [
            Snapshot::empty(SpaceId::new("w1")),
            empty_space(),
            populated(),
        ] {
            for width in [0, 1, 10, 34, 35, 40, 60, 96, 98, 120] {
                for height in [0, 1, 4, 11, 12, 13, 18, 24] {
                    for tab in [WorkTab::Messages, WorkTab::Tasks] {
                        let mut view = selected_view(&snapshot);
                        view.work_tab = tab;
                        let buffer = render(&snapshot, &view, width, height, false);
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
    }

    // Run `cargo test tui::tests::render_previews -- --nocapture` for reproducible visual review.
    #[test]
    fn render_previews() {
        let mut tasks = work_tabs::many_tasks(18);
        tasks.tasks[0].state = TaskState::Accepted;
        tasks.tasks[3].state = TaskState::Review;
        tasks.tasks[5].state = TaskState::Cancelled;
        tasks.tasks[17].state = TaskState::Paused;
        tasks.tasks[17].note = "Participant disconnected. Reconcile external jobs.".into();
        for (name, snapshot, width, height, help, tab, last) in [
            (
                "EMPTY MESSAGES",
                empty_space(),
                100,
                24,
                false,
                WorkTab::Messages,
                false,
            ),
            (
                "EMPTY TASKS",
                empty_space(),
                100,
                24,
                false,
                WorkTab::Tasks,
                false,
            ),
            (
                "POPULATED MESSAGES",
                populated(),
                120,
                30,
                false,
                WorkTab::Messages,
                false,
            ),
            (
                "SCROLLED TASKS",
                tasks.clone(),
                120,
                30,
                false,
                WorkTab::Tasks,
                true,
            ),
            (
                "NARROW MESSAGES",
                populated(),
                56,
                34,
                false,
                WorkTab::Messages,
                false,
            ),
            (
                "NARROW TASKS",
                tasks.clone(),
                40,
                20,
                false,
                WorkTab::Tasks,
                true,
            ),
            ("MINIMUM TASKS", tasks, 35, 12, false, WorkTab::Tasks, true),
            ("HELP", populated(), 120, 30, true, WorkTab::Messages, false),
        ] {
            let mut view = selected_view(&snapshot);
            view.help = help;
            view.work_tab = tab;
            if last {
                for _ in 1..snapshot.tasks.len() {
                    view.navigate(KeyCode::Down, &snapshot);
                }
            }
            let preview = screen(&render(&snapshot, &view, width, height, false));
            assert_eq!(
                preview,
                screen(&render(&snapshot, &view, width, height, false))
            );
            println!("\n--- {name} {width}x{height} ---\n{preview}");
        }
    }

    #[test]
    fn vertical_arrows_and_vim_keys_have_identical_navigation() {
        let snapshot = populated();
        for (arrow, vim) in [(KeyCode::Up, 'k'), (KeyCode::Down, 'j')] {
            for tab in [WorkTab::Messages, WorkTab::Tasks] {
                let mut a = selected_view(&snapshot);
                let mut b = selected_view(&snapshot);
                a.work_tab = tab;
                b.work_tab = tab;
                a.scroll = 3;
                b.scroll = 3;
                assert!(a.navigate(arrow, &snapshot));
                assert!(b.navigate(KeyCode::Char(vim), &snapshot));
                assert_eq!(a.space_id, b.space_id);
                assert_eq!(a.tasks.selected, b.tasks.selected);
                assert_eq!(a.scroll, b.scroll);
            }
        }
        let mut view = selected_view(&snapshot);
        view.navigate(KeyCode::Up, &snapshot);
        assert_eq!(view.scroll, 0);
        assert!(!view.navigate(KeyCode::Char('q'), &snapshot));
    }

    #[test]
    fn former_space_switch_keys_are_inert_in_both_work_tabs() {
        let snapshot = work_tabs::many_tasks(20);
        for tab in [WorkTab::Messages, WorkTab::Tasks] {
            let mut view = selected_view(&snapshot);
            view.work_tab = tab;
            view.scroll = 7;
            for _ in 0..12 {
                view.tasks.advance(&view.space_tasks(&snapshot), true);
            }
            view.tasks.offset = 8;
            view.notice = "Keep this notice".into();
            for key in [
                KeyCode::Tab,
                KeyCode::BackTab,
                KeyCode::Left,
                KeyCode::Right,
                KeyCode::Char('h'),
                KeyCode::Char('l'),
            ] {
                assert!(!view.navigate(key, &snapshot));
                assert_eq!(view.space_id, SpaceId::new("w1"));
                assert_eq!(view.work_tab, tab);
                assert_eq!(view.scroll, 7);
                assert_eq!(view.tasks.selected, Some(TaskId::new("task-012")));
                assert_eq!(view.tasks.offset, 8);
                assert_eq!(view.notice, "Keep this notice");
            }
        }
    }
}
