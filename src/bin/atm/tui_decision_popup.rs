//! Pure, modal decision inspection and answering; attachments/options are never executed.
use super::msg::escaped;
use atm_org::{AwayStep, AwayWait, DecisionId, DecisionStatus};
use atmd::org::decisions::{DecisionEntry, MutationResult, ResolveParams};
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::{
    layout::Margin,
    text::Line,
    widgets::{Block, Borders, Clear, Paragraph},
    Frame,
};

#[derive(Debug, Default)]
pub(super) enum Screen {
    #[default]
    Loading,
    Queue,
    Inspect(Box<DecisionEntry>),
    Recorded(Box<MutationResult>, Option<String>),
    Error(String),
}
#[derive(Debug)]
pub(super) enum Input {
    Key(KeyEvent),
    Queue(Vec<DecisionEntry>),
    Loaded(Box<DecisionEntry>),
    Changed(DecisionId),
    Recorded(Box<MutationResult>, Option<String>),
    OutcomeUnknown(String),
    Failed(String),
}
#[derive(Debug)]
pub(super) enum Effect {
    Get(DecisionId),
    Resolve(ResolveParams),
    Close,
    Quit,
}
#[derive(Debug, Default)]
pub(super) struct DecisionPopup {
    pub(super) screen: Screen,
    queue: Vec<DecisionEntry>,
    selected: usize,
    scroll: usize,
    option: Option<usize>,
    note: String,
    editing_note: bool,
    confirming: bool,
    pending: bool,
    refreshing: bool,
    unknown: bool,
    message: Option<String>,
}
impl DecisionPopup {
    pub(super) fn reduce(&mut self, input: Input) -> Vec<Effect> {
        match input {
            Input::Queue(queue) => {
                self.queue = queue;
                self.selected = 0;
                self.scroll = 0;
                self.screen = Screen::Queue;
            }
            Input::Changed(id) => {
                if matches!(&self.screen, Screen::Inspect(entry) if entry.decision.id == id)
                    && !self.pending
                    && !self.unknown
                {
                    self.refreshing = true;
                    self.confirming = false;
                    return vec![Effect::Get(id)];
                }
            }
            Input::Loaded(_) if matches!(self.screen, Screen::Queue) => {}
            Input::Loaded(entry) => {
                let previous = match &self.screen {
                    Screen::Inspect(old) if old.decision.id == entry.decision.id => self
                        .option
                        .and_then(|index| old.decision.request.options.get(index)),
                    Screen::Recorded(old, _) if old.decision.decision.id == entry.decision.id => {
                        old.decision.decision.resolution.as_ref().map(|r| &r.option)
                    }
                    _ => {
                        self.note.clear();
                        None
                    }
                };
                self.option = previous.and_then(|option| {
                    entry
                        .decision
                        .request
                        .options
                        .iter()
                        .position(|s| s == option)
                });
                self.screen = Screen::Inspect(entry);
                self.scroll = 0;
                self.pending = false;
                self.refreshing = false;
                self.confirming = false;
                self.unknown = false;
                self.message = None;
            }
            Input::Recorded(result, error) => {
                self.screen = Screen::Recorded(result, error);
                self.pending = false;
                self.confirming = false;
                self.scroll = 0;
            }
            Input::OutcomeUnknown(error) => {
                self.pending = false;
                self.confirming = false;
                self.unknown = true;
                self.message = Some(error);
                self.scroll = 0;
            }
            Input::Failed(error) => {
                self.refreshing = false;
                if matches!(self.screen, Screen::Inspect(_)) {
                    self.unknown = true;
                    self.message = Some(error);
                } else {
                    self.screen = Screen::Error(error);
                }
            }
            Input::Key(key) if key.kind == KeyEventKind::Press => {
                if key.modifiers == KeyModifiers::CONTROL && key.code == KeyCode::Char('c') {
                    return vec![Effect::Quit];
                }
                if key.modifiers.intersects(
                    KeyModifiers::CONTROL
                        | KeyModifiers::ALT
                        | KeyModifiers::SUPER
                        | KeyModifiers::HYPER
                        | KeyModifiers::META,
                ) {
                    return vec![];
                }
                if self.pending {
                    return vec![];
                }
                if self.editing_note {
                    match key.code {
                        KeyCode::Esc => self.editing_note = false,
                        KeyCode::Backspace => {
                            self.note.pop();
                        }
                        KeyCode::Char(c)
                            if !c.is_control()
                                && self.note.len().saturating_add(c.len_utf8()) <= 8192 =>
                        {
                            self.note.push(c)
                        }
                        _ => {}
                    }
                    return vec![];
                }
                if self.confirming {
                    match key.code {
                        KeyCode::Char('y') => return self.submit(),
                        KeyCode::Char('n') | KeyCode::Enter | KeyCode::Esc => {
                            self.confirming = false
                        }
                        KeyCode::Down => self.scroll = self.scroll.saturating_add(1),
                        KeyCode::Up => self.scroll = self.scroll.saturating_sub(1),
                        _ => {}
                    }
                    return vec![];
                }
                match key.code {
                    KeyCode::Char('i') => {
                        let id = match &self.screen {
                            Screen::Inspect(entry) => Some(entry.decision.id),
                            Screen::Recorded(result, _) => Some(result.decision.decision.id),
                            _ => None,
                        };
                        if let Some(id) = id {
                            self.unknown = true;
                            return vec![Effect::Get(id)];
                        }
                    }
                    KeyCode::Tab if matches!(self.screen, Screen::Inspect(_)) && !self.unknown => {
                        self.editing_note = true
                    }
                    KeyCode::Char('j') | KeyCode::Char('k')
                        if matches!(self.screen, Screen::Inspect(_)) =>
                    {
                        if let Screen::Inspect(entry) = &self.screen {
                            self.option =
                                entry
                                    .decision
                                    .request
                                    .options
                                    .len()
                                    .checked_sub(1)
                                    .map(|last| match self.option {
                                        Some(index) if key.code == KeyCode::Char('j') => {
                                            index.saturating_add(1).min(last)
                                        }
                                        Some(index) => index.saturating_sub(1),
                                        None => 0,
                                    });
                        }
                    }
                    KeyCode::Enter
                        if matches!(self.screen, Screen::Inspect(_))
                            && !self.unknown
                            && !self.refreshing
                            && self.option.is_some() =>
                    {
                        if let Screen::Inspect(entry) = &self.screen {
                            if waiting(entry) {
                                let overrule = entry.decision.status
                                    == DecisionStatus::TakenProvisionally
                                    && entry.decision.provisional.as_ref().is_some_and(|p| {
                                        Some(&p.option)
                                            != self.option.and_then(|index| {
                                                entry.decision.request.options.get(index)
                                            })
                                    });
                                if entry.decision.away_step(true)
                                    == AwayStep::Wait(AwayWait::Irreversible)
                                    || overrule
                                {
                                    self.confirming = true;
                                    self.scroll = 0;
                                } else {
                                    return self.submit();
                                }
                            }
                        }
                    }
                    KeyCode::Esc => {
                        if matches!(self.screen, Screen::Inspect(_)) && self.queue.len() > 1 {
                            self.screen = Screen::Queue;
                            self.scroll = 0;
                        } else {
                            return vec![Effect::Close];
                        }
                    }
                    KeyCode::Char('j') | KeyCode::Down if matches!(self.screen, Screen::Queue) => {
                        self.selected = self
                            .selected
                            .saturating_add(1)
                            .min(self.queue.len().saturating_sub(1));
                    }
                    KeyCode::Char('k') | KeyCode::Up if matches!(self.screen, Screen::Queue) => {
                        self.selected = self.selected.saturating_sub(1);
                    }
                    KeyCode::Enter if matches!(self.screen, Screen::Queue) => {
                        if let Some(entry) = self.queue.get(self.selected) {
                            let id = entry.decision.id;
                            self.screen = Screen::Loading;
                            return vec![Effect::Get(id)];
                        }
                    }
                    KeyCode::Char('j') | KeyCode::Down => {
                        self.scroll = self.scroll.saturating_add(1)
                    }
                    KeyCode::Char('k') | KeyCode::Up => self.scroll = self.scroll.saturating_sub(1),
                    _ => {}
                }
            }
            Input::Key(_) => {}
        }
        vec![]
    }

    fn submit(&mut self) -> Vec<Effect> {
        if let Screen::Inspect(entry) = &self.screen {
            if waiting(entry) && !self.pending && !self.unknown && !self.refreshing {
                if let Some(option) = self
                    .option
                    .and_then(|index| entry.decision.request.options.get(index))
                {
                    self.pending = true;
                    self.confirming = false;
                    self.scroll = 0;
                    return vec![Effect::Resolve(ResolveParams {
                        id: entry.decision.id,
                        option: option.clone(),
                        note: (!self.note.is_empty()).then(|| self.note.clone()),
                        standing_rule: false,
                        relay: false,
                    })];
                }
            }
        }
        vec![]
    }

    fn text(&self) -> String {
        match &self.screen {
            Screen::Loading => "Loading waiting decisions…\nEsc: close".into(),
            Screen::Error(error) => format!(
                "Decision service unavailable: {}\nEsc: close",
                escaped(error, false)
            ),
            Screen::Queue if self.queue.is_empty() => {
                "No waiting decisions visible to this caller.\nEsc: close".into()
            }
            Screen::Queue => {
                let mut text =
                    "Waiting decisions — j/k: select, Enter: inspect, Esc: close\n".to_owned();
                for (index, entry) in self.queue.iter().enumerate() {
                    text.push_str(&queue_row(entry, index == self.selected));
                    text.push('\n');
                }
                text
            }
            Screen::Recorded(result, error) => {
                let decision = &result.decision.decision;
                let status = if decision.status == DecisionStatus::Overruled {
                    "overruled"
                } else {
                    "resolved"
                };
                let answer = decision
                    .resolution
                    .as_ref()
                    .map(|r| escaped(&r.option, false))
                    .unwrap_or_else(|| "not recorded".into());
                let mut text = format!(
                    "{} {status}: \"{answer}\" (recorded){}\nNo files are reverted.\n",
                    decision.id,
                    if result.changed { "" } else { " (unchanged)" }
                );
                if let Some(error) = error {
                    text.push_str(&format!(
                        "Redo report unavailable: {}\ni: inspect the recorded result\n",
                        escaped(error, false)
                    ));
                } else if let Some(redo) = &result.redo {
                    let count = |n: Option<usize>| {
                        n.map(|n| n.to_string())
                            .unwrap_or_else(|| "not recorded".into())
                    };
                    text.push_str(&format!(
                        "Accepted again: {}/{}; cancelled: {}\nRecorded redo tasks:\n",
                        count(redo.completed_count),
                        count(redo.required_count),
                        count(redo.cancelled_count)
                    ));
                    for id in &redo.task_ids {
                        text.push_str(&format!("  {id}\n"));
                    }
                } else if decision.status == DecisionStatus::Overruled {
                    text.push_str("Redo metadata not available from this daemon.\n");
                }
                text.push_str("↑/↓: scroll; i: inspect; Esc: back");
                text
            }
            Screen::Inspect(entry) => {
                let decision = &entry.decision;
                let request = &decision.request;
                let answer = self
                    .option
                    .and_then(|index| request.options.get(index))
                    .map(|s| format!("\"{}\"", escaped(s, false)))
                    .unwrap_or_else(|| "none — j/k to choose".into());
                let banner = if self.pending {
                    "Recording answer… no second submission; Ctrl-C quits.\n".into()
                } else if self.unknown {
                    format!("Outcome unknown. Inspect before retrying (i); no automatic resubmit.\n{}\n",self.message.as_deref().map(|s| escaped(s,false)).unwrap_or_default())
                } else if self.refreshing {
                    "Decision changed; refreshing before answering…\n".into()
                } else if self.confirming {
                    format!("Confirm {} answer {answer}? [y/N]\nOverruling a provisional choice reopens and unassigns recorded dependent work.\nNo files are reverted.\n",decision.id)
                } else if !waiting(entry) {
                    "This decision is not waiting; answers are disabled.\n".into()
                } else {
                    "j/k: option; Tab: note; Enter: record; ↑/↓: scroll; Esc: back\n".into()
                };
                let mut text = banner + &format!("Selected: {answer}\nNote{}: {}\nOwner answers require a genuinely unbound owner shell.\n",if self.editing_note { " (editing; Esc finishes; Tab/Enter ignored)" } else { " (optional)" },escaped(&self.note,false));
                text.push_str(&format!("{} from {}\n{}\nKind: {:?}; class: {:?}; status: {:?}\nDeadline: {}\nContext:\n{}\nAttachments (references only):\n",
                    decision.id, request.requester, escaped(&request.title, false), request.kind, entry.class, decision.status,
                    request.deadline.map(|at| at.to_rfc3339()).unwrap_or_else(|| "none".into()),
                    request.context.split('\n').map(|line| escaped(line,false)).collect::<Vec<_>>().join("\n")));
                if let Some(resolution) = &decision.resolution {
                    text.push_str(&format!(
                        "Recorded answer: \"{}\"; note: {}\n",
                        escaped(&resolution.option, false),
                        resolution
                            .note
                            .as_deref()
                            .map(|s| escaped(s, false))
                            .unwrap_or_else(|| "none".into())
                    ));
                }
                for file in &request.files {
                    text.push_str(&format!("  {}\n", escaped(file, false)));
                }
                if let Some(seat) = decision
                    .provisional
                    .as_ref()
                    .filter(|_| decision.status == DecisionStatus::TakenProvisionally)
                    .and_then(|choice| choice.relayed_by.as_ref())
                {
                    text.push_str(&format!(
                        "Relayed by {} as your spoken answer; not confirmed by you\n",
                        escaped(&seat.to_string(), false)
                    ));
                }
                text.push_str(&format!(
                    "Recommended: {}\nProvisional: {}\nOptions (exact strings):\n",
                    request
                        .recommended
                        .as_deref()
                        .map(|s| format!("\"{}\"", escaped(s, false)))
                        .unwrap_or_else(|| "none".into()),
                    decision
                        .provisional
                        .as_ref()
                        .map(|p| format!("\"{}\"", escaped(&p.option, false)))
                        .unwrap_or_else(|| "none".into())
                ));
                for (index, option) in request.options.iter().enumerate() {
                    text.push_str(&format!(
                        "{} \"{}\"\n",
                        if Some(index) == self.option { ">" } else { " " },
                        escaped(option, false)
                    ));
                }
                text.push_str("↑/↓: scroll; i: inspect; Esc: back");
                text
            }
        }
    }

    pub(super) fn render(&mut self, frame: &mut Frame) {
        let area = frame.area().inner(Margin::new(1, 1));
        frame.render_widget(Clear, area);
        let block = Block::default()
            .borders(Borders::ALL)
            .title(" Answer waiting decision ");
        let inner = block.inner(area);
        let lines = wrapped(&self.text(), usize::from(inner.width));
        let height = usize::from(inner.height);
        self.scroll = self.scroll.min(lines.len().saturating_sub(height));
        // Keep the selected chooser entry visible even when titles wrap.
        if matches!(self.screen, Screen::Queue) {
            let prefix = self
                .queue
                .iter()
                .take(self.selected)
                .map(|entry| wrapped(&queue_row(entry, false), usize::from(inner.width)).len())
                .sum::<usize>()
                + wrapped(
                    "Waiting decisions — j/k: select, Enter: inspect, Esc: close",
                    usize::from(inner.width),
                )
                .len();
            if prefix < self.scroll {
                self.scroll = prefix;
            }
            if prefix >= self.scroll.saturating_add(height) {
                self.scroll = prefix.saturating_sub(height.saturating_sub(1));
            }
        }
        frame.render_widget(block, area);
        frame.render_widget(
            Paragraph::new(
                lines
                    .into_iter()
                    .skip(self.scroll)
                    .take(height)
                    .map(Line::from)
                    .collect::<Vec<_>>(),
            ),
            inner,
        );
    }
}

fn queue_row(entry: &DecisionEntry, selected: bool) -> String {
    format!(
        "{} {} from {}: {}{}",
        if selected { ">" } else { " " },
        entry.decision.id,
        entry.decision.request.requester,
        escaped(&entry.decision.request.title, false),
        if entry.decision.status == DecisionStatus::TakenProvisionally
            && entry
                .decision
                .provisional
                .as_ref()
                .is_some_and(|choice| choice.relayed_by.is_some())
        {
            " [relayed]"
        } else {
            ""
        }
    )
}

fn waiting(entry: &DecisionEntry) -> bool {
    matches!(
        entry.decision.status,
        DecisionStatus::Pending | DecisionStatus::TakenProvisionally
    )
}

// Character-width wrapping retains whitespace, controls already escaped above.
// Store a usize offset and render a window instead of Paragraph's u16 scroll:
// a legal long attachment/context can exceed 65535 rows at sidebar widths.
fn wrapped(text: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![];
    }
    let mut lines = Vec::new();
    for source in text.split('\n') {
        let mut line = String::new();
        let mut used = 0usize;
        for c in source.chars() {
            let size = Line::from(c.to_string()).width();
            if used.saturating_add(size) > width && !line.is_empty() {
                lines.push(std::mem::take(&mut line));
                used = 0;
            }
            line.push(c);
            used = used.saturating_add(size);
        }
        lines.push(line);
    }
    lines
}
