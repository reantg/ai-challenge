use std::{
    io,
    time::{Duration, Instant},
};

use crossterm::{
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
        KeyModifiers, MouseEventKind,
    },
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use pulldown_cmark::{CodeBlockKind, Event as MarkdownEvent, Options, Parser, Tag, TagEnd};
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
};
use tokio::sync::mpsc;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::api::{
    self, CompletionOptions, LengthLimit, Message, ResponseFormat, Role, StopCondition,
};

const POLL_INTERVAL: Duration = Duration::from_millis(80);
const MAX_INPUT_CHARS: usize = 4000;

enum RequestState {
    Ready,
    Waiting(Instant),
    Failed(String),
}

enum ReplyEvent {
    Chunk(String),
    Finished(String),
    Failed(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Popup {
    None,
    Menu {
        kind: MenuKind,
        selected: usize,
    },
    Input {
        kind: InputKind,
        value: Vec<char>,
        cursor: usize,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MenuKind {
    Format,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum InputKind {
    FormatJson,
    FormatMarkdown,
    FormatYaml,
    Limit,
    Stop,
}

impl Popup {
    fn input(kind: InputKind, initial: String) -> Self {
        let value = initial.chars().collect::<Vec<_>>();
        let cursor = value.len();
        Self::Input {
            kind,
            value,
            cursor,
        }
    }
}

pub async fn run(api_url: String, api_key: String, api_model: String) -> io::Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    terminal.clear()?;

    let result = run_loop(&mut terminal, App::new(api_url, api_key, api_model)).await;

    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        DisableMouseCapture,
        LeaveAlternateScreen
    )?;
    terminal.show_cursor()?;
    result
}

async fn run_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    mut app: App,
) -> io::Result<()> {
    while !app.quit {
        app.receive_reply();
        terminal.draw(|frame| app.view(frame))?;

        if event::poll(POLL_INTERVAL)? {
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => app.handle_key(key),
                Event::Mouse(mouse) => match mouse.kind {
                    MouseEventKind::ScrollUp if app.popup == Popup::None => {
                        app.scroll_history_up(3)
                    }
                    MouseEventKind::ScrollDown if app.popup == Popup::None => {
                        app.scroll_history_down(3)
                    }
                    _ => {}
                },
                Event::Paste(value) => app.handle_paste(&value),
                _ => {}
            }
        }
        app.frame = app.frame.wrapping_add(1);
    }
    Ok(())
}

struct App {
    messages: Vec<Message>,
    input: Vec<char>,
    cursor: usize,
    request_state: RequestState,
    reply_tx: mpsc::UnboundedSender<ReplyEvent>,
    reply_rx: mpsc::UnboundedReceiver<ReplyEvent>,
    api_url: String,
    api_key: String,
    api_model: String,
    options: CompletionOptions,
    popup: Popup,
    history_top: usize,
    max_history_top: usize,
    history_page_height: usize,
    follow_history_tail: bool,
    frame: usize,
    quit: bool,
}

impl App {
    fn new(api_url: String, api_key: String, api_model: String) -> Self {
        let (reply_tx, reply_rx) = mpsc::unbounded_channel();
        Self {
            messages: Vec::new(),
            input: Vec::new(),
            cursor: 0,
            request_state: RequestState::Ready,
            reply_tx,
            reply_rx,
            api_url,
            api_key,
            api_model,
            options: CompletionOptions::default(),
            popup: Popup::None,
            history_top: 0,
            max_history_top: 0,
            history_page_height: 1,
            follow_history_tail: true,
            frame: 0,
            quit: false,
        }
    }

    fn handle_key(&mut self, key: KeyEvent) {
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            self.quit = true;
            return;
        }
        if self.popup != Popup::None {
            self.handle_popup_key(key);
            return;
        }
        if key.code == KeyCode::Esc {
            self.quit = true;
            return;
        }

        match key.code {
            KeyCode::Up => {
                self.scroll_history_up(1);
                return;
            }
            KeyCode::Down => {
                self.scroll_history_down(1);
                return;
            }
            KeyCode::PageUp => {
                self.scroll_history_up(self.history_page_height);
                return;
            }
            KeyCode::PageDown => {
                self.scroll_history_down(self.history_page_height);
                return;
            }
            KeyCode::End => {
                self.follow_history_tail = true;
                self.history_top = self.max_history_top;
                return;
            }
            _ => {}
        }
        if matches!(self.request_state, RequestState::Waiting(_)) {
            return;
        }

        match key.code {
            KeyCode::Enter => self.submit(),
            KeyCode::Backspace if self.cursor > 0 => {
                self.cursor -= 1;
                self.input.remove(self.cursor);
            }
            KeyCode::Delete if self.cursor < self.input.len() => {
                self.input.remove(self.cursor);
            }
            KeyCode::Left => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Right => self.cursor = (self.cursor + 1).min(self.input.len()),
            KeyCode::Home => self.cursor = 0,
            KeyCode::Char(character)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                self.insert_char(character);
            }
            _ => {}
        }
    }

    fn scroll_history_up(&mut self, lines: usize) {
        self.follow_history_tail = false;
        self.history_top = self.history_top.saturating_sub(lines);
    }

    fn scroll_history_down(&mut self, lines: usize) {
        self.history_top = self
            .history_top
            .saturating_add(lines)
            .min(self.max_history_top);
        self.follow_history_tail = self.history_top == self.max_history_top;
    }

    fn insert_char(&mut self, character: char) {
        if self.input.len() < MAX_INPUT_CHARS
            && !matches!(self.request_state, RequestState::Waiting(_))
        {
            self.input.insert(self.cursor, character);
            self.cursor += 1;
        }
    }

    fn insert_text(&mut self, value: &str) {
        for character in value.chars().filter(|character| !character.is_control()) {
            self.insert_char(character);
        }
    }

    fn handle_paste(&mut self, pasted: &str) {
        if let Popup::Input { value, cursor, .. } = &mut self.popup {
            for character in pasted.chars().filter(|character| !character.is_control()) {
                if value.len() >= MAX_INPUT_CHARS {
                    break;
                }
                value.insert(*cursor, character);
                *cursor += 1;
            }
        } else {
            self.insert_text(pasted);
        }
    }

    fn submit(&mut self) {
        let content = self.input.iter().collect::<String>().trim().to_owned();
        if content.is_empty() {
            return;
        }
        self.clear_input();

        if content.starts_with('/') {
            if let Err(reason) = self.apply_command(&content) {
                self.request_state = RequestState::Failed(reason);
            } else {
                self.request_state = RequestState::Ready;
            }
            return;
        }

        self.messages.push(Message::new(Role::User, content));
        self.request_state = RequestState::Waiting(Instant::now());

        let tx = self.reply_tx.clone();
        let api_url = self.api_url.clone();
        let api_key = self.api_key.clone();
        let model = self.api_model.clone();
        let messages = self.messages.clone();
        let options = self.options.clone();
        self.messages.push(Message::new(Role::Assistant, ""));
        tokio::spawn(async move {
            run_request(api_url, api_key, model, messages, options, tx).await;
        });
    }

    fn apply_command(&mut self, content: &str) -> Result<(), String> {
        let (name, argument) = content.split_once(' ').unwrap_or((content, ""));
        let argument = argument.trim();
        match name.to_lowercase().as_str() {
            "/format" if argument.is_empty() => {
                self.popup = Popup::Menu {
                    kind: MenuKind::Format,
                    selected: format_index(&self.options.response_format),
                };
                Ok(())
            }
            "/limit" if argument.is_empty() => {
                let initial = match self.options.length_limit {
                    LengthLimit::Default => "default".into(),
                    LengthLimit::MaxTokens(value) => value.to_string(),
                };
                self.popup = Popup::input(InputKind::Limit, initial);
                Ok(())
            }
            "/stop" if argument.is_empty() => {
                let initial = match &self.options.stop_condition {
                    StopCondition::Natural => "off".into(),
                    StopCondition::Sequence(value) => value.clone(),
                };
                self.popup = Popup::input(InputKind::Stop, initial);
                Ok(())
            }
            "/format" => self.set_format(argument),
            "/limit" => self.set_limit(argument),
            "/stop" => self.set_stop(argument),
            _ => Err("Неизвестная команда. Доступны: /format, /limit, /stop".into()),
        }
    }

    fn set_format(&mut self, argument: &str) -> Result<(), String> {
        let (name, description) = argument.split_once(' ').unwrap_or((argument, ""));
        let description = description.trim();
        self.options.response_format = match (name.to_lowercase().as_str(), description) {
            ("text", _) => ResponseFormat::PlainText,
            ("json", value) if !value.is_empty() => ResponseFormat::JsonObject(value.into()),
            ("markdown" | "md", value) if !value.is_empty() => {
                ResponseFormat::Markdown(value.into())
            }
            ("yaml", value) if !value.is_empty() => ResponseFormat::Yaml(value.into()),
            _ => {
                return Err(
                    "Использование: /format text или /format json|markdown|yaml <схема>".into(),
                );
            }
        };
        Ok(())
    }

    fn handle_popup_key(&mut self, key: KeyEvent) {
        let popup = std::mem::replace(&mut self.popup, Popup::None);
        match popup {
            Popup::None => {}
            Popup::Menu { kind, mut selected } => {
                let count = 4;
                match key.code {
                    KeyCode::Esc => self.request_state = RequestState::Ready,
                    KeyCode::Up => {
                        selected = selected.checked_sub(1).unwrap_or(count - 1);
                        self.popup = Popup::Menu { kind, selected };
                    }
                    KeyCode::Down => {
                        selected = (selected + 1) % count;
                        self.popup = Popup::Menu { kind, selected };
                    }
                    KeyCode::Char(value @ '1'..='4') => {
                        self.apply_menu_choice(kind, value as usize - '1' as usize);
                    }
                    KeyCode::Enter => self.apply_menu_choice(kind, selected),
                    _ => self.popup = Popup::Menu { kind, selected },
                }
            }
            Popup::Input {
                kind,
                mut value,
                mut cursor,
            } => match key.code {
                KeyCode::Esc => self.request_state = RequestState::Ready,
                KeyCode::Enter => {
                    let text = value.iter().collect::<String>().trim().to_owned();
                    if let Err(reason) = self.apply_popup_input(kind, &text) {
                        self.request_state = RequestState::Failed(reason);
                        self.popup = Popup::Input {
                            kind,
                            value,
                            cursor,
                        };
                    } else {
                        self.request_state = RequestState::Ready;
                    }
                }
                KeyCode::Backspace if cursor > 0 => {
                    cursor -= 1;
                    value.remove(cursor);
                    self.popup = Popup::Input {
                        kind,
                        value,
                        cursor,
                    };
                }
                KeyCode::Delete if cursor < value.len() => {
                    value.remove(cursor);
                    self.popup = Popup::Input {
                        kind,
                        value,
                        cursor,
                    };
                }
                KeyCode::Left => {
                    cursor = cursor.saturating_sub(1);
                    self.popup = Popup::Input {
                        kind,
                        value,
                        cursor,
                    };
                }
                KeyCode::Right => {
                    cursor = (cursor + 1).min(value.len());
                    self.popup = Popup::Input {
                        kind,
                        value,
                        cursor,
                    };
                }
                KeyCode::Home => {
                    self.popup = Popup::Input {
                        kind,
                        value,
                        cursor: 0,
                    }
                }
                KeyCode::End => {
                    cursor = value.len();
                    self.popup = Popup::Input {
                        kind,
                        value,
                        cursor,
                    };
                }
                KeyCode::Char(character)
                    if !key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
                        && value.len() < MAX_INPUT_CHARS =>
                {
                    value.insert(cursor, character);
                    cursor += 1;
                    self.popup = Popup::Input {
                        kind,
                        value,
                        cursor,
                    };
                }
                _ => {
                    self.popup = Popup::Input {
                        kind,
                        value,
                        cursor,
                    };
                }
            },
        }
    }

    fn apply_menu_choice(&mut self, kind: MenuKind, selected: usize) {
        match kind {
            MenuKind::Format => match selected {
                0 => {
                    self.options.response_format = ResponseFormat::PlainText;
                    self.request_state = RequestState::Ready;
                }
                1 => self.popup = Popup::input(InputKind::FormatJson, String::new()),
                2 => self.popup = Popup::input(InputKind::FormatMarkdown, String::new()),
                _ => self.popup = Popup::input(InputKind::FormatYaml, String::new()),
            },
        }
    }

    fn apply_popup_input(&mut self, kind: InputKind, value: &str) -> Result<(), String> {
        match kind {
            InputKind::FormatJson | InputKind::FormatMarkdown | InputKind::FormatYaml
                if value.is_empty() =>
            {
                Err("Схема или шаблон не могут быть пустыми".into())
            }
            InputKind::FormatJson => {
                self.options.response_format = ResponseFormat::JsonObject(value.into());
                Ok(())
            }
            InputKind::FormatMarkdown => {
                self.options.response_format = ResponseFormat::Markdown(value.into());
                Ok(())
            }
            InputKind::FormatYaml => {
                self.options.response_format = ResponseFormat::Yaml(value.into());
                Ok(())
            }
            InputKind::Limit => self.set_limit(value),
            InputKind::Stop => self.set_stop(value),
        }
    }

    fn set_limit(&mut self, argument: &str) -> Result<(), String> {
        self.options.length_limit = match argument.to_lowercase().as_str() {
            "default" | "off" => LengthLimit::Default,
            value => match value.parse::<u64>() {
                Ok(value) if value > 0 => LengthLimit::MaxTokens(value),
                _ => {
                    return Err("Использование: /limit <положительное число>|default".into());
                }
            },
        };
        Ok(())
    }

    fn set_stop(&mut self, argument: &str) -> Result<(), String> {
        self.options.stop_condition = if argument.eq_ignore_ascii_case("off") {
            StopCondition::Natural
        } else if argument.is_empty() {
            return Err("Использование: /stop <последовательность>|off".into());
        } else {
            StopCondition::Sequence(argument.into())
        };
        Ok(())
    }

    fn receive_reply(&mut self) {
        if !matches!(self.request_state, RequestState::Waiting(_)) {
            return;
        }
        while let Ok(event) = self.reply_rx.try_recv() {
            match event {
                ReplyEvent::Chunk(content) => {
                    if let Some(Message {
                        role: Role::Assistant,
                        content: answer,
                    }) = self.messages.last_mut()
                    {
                        answer.push_str(&content);
                    }
                }
                ReplyEvent::Finished(content) => {
                    if let Some(Message {
                        role: Role::Assistant,
                        content: answer,
                    }) = self.messages.last_mut()
                    {
                        *answer = content;
                    }
                    self.request_state = RequestState::Ready;
                }
                ReplyEvent::Failed(reason) => {
                    // A streamed answer may be incomplete and must not enter the
                    // multi-turn history after a failed request.
                    if matches!(
                        self.messages.last(),
                        Some(Message {
                            role: Role::Assistant,
                            ..
                        })
                    ) {
                        self.messages.pop();
                    }
                    self.request_state = RequestState::Failed(reason);
                }
            }
            if !matches!(self.request_state, RequestState::Waiting(_)) {
                break;
            }
        }
    }

    fn clear_input(&mut self) {
        self.input.clear();
        self.cursor = 0;
    }

    fn view(&mut self, frame: &mut Frame) {
        let areas = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Min(1),
                Constraint::Length(3),
                Constraint::Length(1),
                Constraint::Length(1),
            ])
            .split(frame.area());

        self.render_feed(frame, areas[0]);
        self.render_input(frame, areas[1]);
        self.render_options(frame, areas[2]);
        self.render_status(frame, areas[3]);
        self.render_popup(frame);
    }

    fn render_feed(&mut self, frame: &mut Frame, area: Rect) {
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(ratatui::widgets::BorderType::Rounded)
            .title(" DeepSeek Chat ")
            .border_style(Style::default().fg(Color::DarkGray));
        let inner = block.inner(area);
        frame.render_widget(block, area);

        let content_width = inner.width.saturating_sub(11).max(1) as usize;
        let mut lines = self.message_lines(content_width);
        if let RequestState::Failed(reason) = &self.request_state {
            lines.push(Line::from(vec![
                Span::styled(
                    "Ошибка › ",
                    Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                ),
                Span::raw(reason.clone()),
            ]));
        }
        if let RequestState::Waiting(started_at) = &self.request_state {
            const SPINNER: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
            lines.push(Line::from(vec![
                Span::raw("           "),
                Span::styled(
                    format!(
                        "{} DeepSeek отвечает ({})",
                        SPINNER[self.frame % SPINNER.len()],
                        format_elapsed(started_at.elapsed())
                    ),
                    Style::default().fg(Color::Magenta),
                ),
            ]));
        }
        self.history_page_height = usize::from(inner.height).max(1);
        self.max_history_top = lines.len().saturating_sub(self.history_page_height);
        if self.follow_history_tail {
            self.history_top = self.max_history_top;
        } else {
            self.history_top = self.history_top.min(self.max_history_top);
        }
        frame.render_widget(
            Paragraph::new(
                lines
                    .into_iter()
                    .skip(self.history_top)
                    .take(self.history_page_height)
                    .collect::<Vec<_>>(),
            )
            .wrap(Wrap { trim: false }),
            inner,
        );
    }

    fn message_lines(&self, content_width: usize) -> Vec<Line<'static>> {
        let mut lines = Vec::new();
        for message in &self.messages {
            if message.role == Role::Assistant && message.content.is_empty() {
                continue;
            }
            let (label, style) = match message.role {
                Role::System => ("Система ›  ", Style::default().fg(Color::DarkGray)),
                Role::User => ("Вы ›       ", Style::default().fg(Color::Cyan)),
                Role::Assistant => (
                    "DeepSeek › ",
                    Style::default()
                        .fg(Color::Magenta)
                        .add_modifier(Modifier::BOLD),
                ),
            };
            let content_lines = match message.role {
                Role::Assistant => markdown_lines(&message.content, content_width),
                Role::System | Role::User => textwrap::wrap(&message.content, content_width)
                    .into_iter()
                    .map(|line| Line::raw(line.into_owned()))
                    .collect(),
            };
            for (index, line) in content_lines.into_iter().enumerate() {
                let prefix = if index == 0 { label } else { "           " };
                let mut spans = vec![Span::styled(prefix.to_owned(), style)];
                spans.extend(line.spans);
                lines.push(Line::from(spans));
            }
            lines.push(Line::default());
        }
        lines
    }

    fn render_input(&self, frame: &mut Frame, area: Rect) {
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(ratatui::widgets::BorderType::Rounded)
            .title(" Сообщение ")
            .border_style(Style::default().fg(Color::Magenta));
        let inner = block.inner(area);
        frame.render_widget(block, area);

        let value = self.input.iter().collect::<String>();
        let shown = if value.is_empty() {
            Line::from(vec![
                Span::raw("› "),
                Span::styled("Введите сообщение…", Style::default().fg(Color::DarkGray)),
            ])
        } else {
            Line::from(format!("› {value}"))
        };
        frame.render_widget(Paragraph::new(shown), inner);

        if !matches!(self.request_state, RequestState::Waiting(_)) && inner.width > 0 {
            let before_cursor = self.input[..self.cursor].iter().collect::<String>();
            let offset = 2 + before_cursor.width();
            let x = inner.x + (offset as u16).min(inner.width.saturating_sub(1));
            frame.set_cursor_position((x, inner.y));
        }
    }

    fn render_options(&self, frame: &mut Frame, area: Rect) {
        let format = match self.options.response_format {
            ResponseFormat::PlainText => "текст",
            ResponseFormat::JsonObject(_) => "JSON (схема задана)",
            ResponseFormat::Markdown(_) => "Markdown (шаблон задан)",
            ResponseFormat::Yaml(_) => "YAML (схема задана)",
        };
        let limit = match self.options.length_limit {
            LengthLimit::Default => "по умолчанию".into(),
            LengthLimit::MaxTokens(value) => format!("{value} токенов"),
        };
        let stop = match &self.options.stop_condition {
            StopCondition::Natural => "выключен",
            StopCondition::Sequence(value) => value,
        };
        frame.render_widget(
            Paragraph::new(format!(
                " Формат: {format}  ·  Лимит: {limit}  ·  Stop: {stop}"
            ))
            .style(Style::default().fg(Color::Cyan)),
            area,
        );
    }

    fn render_status(&self, frame: &mut Frame, area: Rect) {
        let (text, style): (String, Style) = match &self.request_state {
            RequestState::Waiting(_) => (
                " Ответ формируется в ленте · ↑/↓ — прокрутка · Esc — выйти".into(),
                Style::default().fg(Color::DarkGray),
            ),
            RequestState::Ready => (
                " Enter — отправить · /format · /limit · /stop · Esc — выйти".into(),
                Style::default().fg(Color::DarkGray),
            ),
            RequestState::Failed(_) => (
                " Повторите сообщение или исправьте команду".into(),
                Style::default().fg(Color::Red),
            ),
        };
        frame.render_widget(Paragraph::new(text).style(style), area);
    }

    fn render_popup(&self, frame: &mut Frame) {
        match &self.popup {
            Popup::None => {}
            Popup::Menu { kind, selected } => {
                let (title, items): (&str, [&str; 4]) = match kind {
                    MenuKind::Format => (
                        " Формат ответа ",
                        [
                            "1. Text — обычный текст",
                            "2. JSON — ввести схему",
                            "3. Markdown — ввести шаблон",
                            "4. YAML — ввести схему",
                        ],
                    ),
                };
                let area = centered_rect(62, 9, frame.area());
                frame.render_widget(Clear, area);
                let lines = items
                    .iter()
                    .enumerate()
                    .map(|(index, item)| {
                        let marker = if index == *selected { "› " } else { "  " };
                        let style = if index == *selected {
                            Style::default()
                                .fg(Color::Black)
                                .bg(Color::Magenta)
                                .add_modifier(Modifier::BOLD)
                        } else {
                            Style::default()
                        };
                        Line::styled(format!("{marker}{item}"), style)
                    })
                    .chain(std::iter::once(Line::styled(
                        "  ↑/↓ — выбор · Enter — применить · Esc — отменить",
                        Style::default().fg(Color::DarkGray),
                    )))
                    .collect::<Vec<_>>();
                frame.render_widget(
                    Paragraph::new(lines).block(
                        Block::default()
                            .title(title)
                            .borders(Borders::ALL)
                            .border_type(ratatui::widgets::BorderType::Rounded)
                            .border_style(Style::default().fg(Color::Magenta)),
                    ),
                    area,
                );
                frame.set_cursor_position((area.x + 1, area.y + 1 + *selected as u16));
            }
            Popup::Input {
                kind,
                value,
                cursor,
            } => {
                let (title, hint) = match kind {
                    InputKind::FormatJson => (" JSON-схема ", "Введите описание JSON-схемы"),
                    InputKind::FormatMarkdown => {
                        (" Markdown-шаблон ", "Введите структуру Markdown-ответа")
                    }
                    InputKind::FormatYaml => (" YAML-схема ", "Введите описание YAML-схемы"),
                    InputKind::Limit => (" Лимит токенов ", "Число, default или off"),
                    InputKind::Stop => (" Stop sequence ", "Последовательность или off"),
                };
                let area = centered_rect(70, 7, frame.area());
                frame.render_widget(Clear, area);
                let (text, cursor_column) =
                    visible_input(value, *cursor, usize::from(area.width.saturating_sub(4)));
                let error = match &self.request_state {
                    RequestState::Failed(reason) => reason.as_str(),
                    _ => "Enter — применить · Esc — отменить",
                };
                frame.render_widget(
                    Paragraph::new(vec![
                        Line::styled(hint, Style::default().fg(Color::DarkGray)),
                        Line::default(),
                        Line::from(format!("› {text}")),
                        Line::styled(
                            error,
                            if matches!(self.request_state, RequestState::Failed(_)) {
                                Style::default().fg(Color::Red)
                            } else {
                                Style::default().fg(Color::DarkGray)
                            },
                        ),
                    ])
                    .block(
                        Block::default()
                            .title(title)
                            .borders(Borders::ALL)
                            .border_type(ratatui::widgets::BorderType::Rounded)
                            .border_style(Style::default().fg(Color::Magenta)),
                    ),
                    area,
                );
                let x = area.x + 3 + (cursor_column as u16).min(area.width.saturating_sub(4));
                frame.set_cursor_position((x, area.y.saturating_add(3)));
            }
        }
    }
}

#[derive(Default)]
struct MarkdownBuilder {
    lines: Vec<Vec<(String, Style)>>,
    current: Vec<(String, Style)>,
    styles: Vec<Style>,
    lists: Vec<Option<u64>>,
    quote_depth: usize,
    in_code_block: bool,
}

impl MarkdownBuilder {
    fn style(&self) -> Style {
        self.styles.last().copied().unwrap_or_default()
    }

    fn push(&mut self, text: impl Into<String>, style: Style) {
        let text = text.into();
        if !text.is_empty() {
            self.current.push((text, style));
        }
    }

    fn push_text(&mut self, text: &str) {
        let style = self.style();
        for (index, part) in text.split('\n').enumerate() {
            if index > 0 {
                self.finish_line();
            }
            self.push(part, style);
        }
    }

    fn push_code(&mut self, text: &str) {
        let style = Style::default().fg(Color::Green);
        for (index, part) in text.split('\n').enumerate() {
            if index > 0 {
                self.finish_line();
            }
            if !part.is_empty() {
                if self.current.is_empty() {
                    self.push("│ ", Style::default().fg(Color::DarkGray));
                }
                self.push(part, style);
            }
        }
    }

    fn finish_line(&mut self) {
        self.lines.push(std::mem::take(&mut self.current));
    }

    fn finish_block(&mut self) {
        if !self.current.is_empty() {
            self.finish_line();
        }
    }

    fn start_item(&mut self) {
        self.finish_block();
        let depth = self.lists.len().saturating_sub(1);
        let marker = match self.lists.last_mut() {
            Some(Some(number)) => {
                let marker = format!("{number}. ");
                *number += 1;
                marker
            }
            _ => "• ".into(),
        };
        self.push("  ".repeat(depth), Style::default());
        self.push(marker, Style::default().fg(Color::Magenta));
    }
}

fn markdown_lines(markdown: &str, width: usize) -> Vec<Line<'static>> {
    let options = Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TABLES
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_GFM;
    let mut builder = MarkdownBuilder::default();

    for event in Parser::new_ext(markdown, options) {
        match event {
            MarkdownEvent::Start(tag) => match tag {
                Tag::Heading { .. } => {
                    builder.finish_block();
                    let style = builder
                        .style()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD);
                    builder.styles.push(style);
                }
                Tag::Strong => {
                    builder
                        .styles
                        .push(builder.style().add_modifier(Modifier::BOLD));
                }
                Tag::Emphasis => {
                    builder
                        .styles
                        .push(builder.style().add_modifier(Modifier::ITALIC));
                }
                Tag::Strikethrough => {
                    builder
                        .styles
                        .push(builder.style().add_modifier(Modifier::CROSSED_OUT));
                }
                Tag::Link { .. } => {
                    builder.styles.push(
                        builder
                            .style()
                            .fg(Color::Cyan)
                            .add_modifier(Modifier::UNDERLINED),
                    );
                }
                Tag::BlockQuote(_) => {
                    builder.finish_block();
                    builder.quote_depth += 1;
                    builder.push(
                        "│ ".repeat(builder.quote_depth),
                        Style::default().fg(Color::DarkGray),
                    );
                }
                Tag::CodeBlock(kind) => {
                    builder.finish_block();
                    let language = match kind {
                        CodeBlockKind::Fenced(language) if !language.is_empty() => {
                            format!("┌─ {language}")
                        }
                        _ => "┌─ code".into(),
                    };
                    builder.push(language, Style::default().fg(Color::DarkGray));
                    builder.finish_line();
                    builder.in_code_block = true;
                }
                Tag::List(start) => builder.lists.push(start),
                Tag::Item => builder.start_item(),
                Tag::TableCell if !builder.current.is_empty() => {
                    builder.push(" │ ", Style::default().fg(Color::DarkGray));
                }
                _ => {}
            },
            MarkdownEvent::End(tag) => match tag {
                TagEnd::Paragraph | TagEnd::Heading(_) | TagEnd::Item | TagEnd::TableRow => {
                    builder.finish_block();
                    if matches!(tag, TagEnd::Heading(_)) {
                        builder.styles.pop();
                    }
                }
                TagEnd::Strong | TagEnd::Emphasis | TagEnd::Strikethrough | TagEnd::Link => {
                    builder.styles.pop();
                }
                TagEnd::BlockQuote(_) => {
                    builder.finish_block();
                    builder.quote_depth = builder.quote_depth.saturating_sub(1);
                }
                TagEnd::CodeBlock => {
                    builder.finish_block();
                    builder.push("└─", Style::default().fg(Color::DarkGray));
                    builder.finish_line();
                    builder.in_code_block = false;
                }
                TagEnd::List(_) => {
                    builder.finish_block();
                    builder.lists.pop();
                }
                _ => {}
            },
            MarkdownEvent::Text(text) => {
                if builder.in_code_block {
                    builder.push_code(&text);
                } else {
                    builder.push_text(&text);
                }
            }
            MarkdownEvent::Code(code) => {
                builder.push(
                    format!(" {code} "),
                    Style::default().fg(Color::Yellow).bg(Color::DarkGray),
                );
            }
            MarkdownEvent::SoftBreak => builder.push(" ", builder.style()),
            MarkdownEvent::HardBreak => builder.finish_line(),
            MarkdownEvent::Rule => {
                builder.finish_block();
                builder.push("────────────────", Style::default().fg(Color::DarkGray));
                builder.finish_line();
            }
            MarkdownEvent::TaskListMarker(checked) => builder.push(
                if checked { "[x] " } else { "[ ] " },
                Style::default().fg(Color::Cyan),
            ),
            MarkdownEvent::InlineMath(math) | MarkdownEvent::DisplayMath(math) => {
                builder.push(math.into_string(), Style::default().fg(Color::Yellow));
            }
            MarkdownEvent::Html(html) | MarkdownEvent::InlineHtml(html) => {
                builder.push_text(&html);
            }
            MarkdownEvent::FootnoteReference(reference) => {
                builder.push(format!("[^{reference}]"), Style::default().fg(Color::Cyan))
            }
        }
    }
    builder.finish_block();
    if builder.lines.is_empty() {
        builder.lines.push(Vec::new());
    }

    builder
        .lines
        .into_iter()
        .flat_map(|line| wrap_markdown_line(line, width.max(1)))
        .map(|spans| {
            Line::from(
                spans
                    .into_iter()
                    .map(|(text, style)| Span::styled(text, style))
                    .collect::<Vec<_>>(),
            )
        })
        .collect()
}

fn wrap_markdown_line(spans: Vec<(String, Style)>, width: usize) -> Vec<Vec<(String, Style)>> {
    let mut lines = vec![Vec::<(String, Style)>::new()];
    let mut column = 0;
    for (text, style) in spans {
        for character in text.chars() {
            let char_width = character.width().unwrap_or(0);
            if column > 0 && column + char_width > width {
                lines.push(Vec::new());
                column = 0;
            }
            if column == 0 && character.is_whitespace() {
                continue;
            }
            let current = lines.last_mut().expect("at least one wrapped line");
            if let Some((existing, existing_style)) = current.last_mut()
                && *existing_style == style
            {
                existing.push(character);
            } else {
                current.push((character.to_string(), style));
            }
            column += char_width;
        }
    }
    lines
}

fn format_index(format: &ResponseFormat) -> usize {
    match format {
        ResponseFormat::PlainText => 0,
        ResponseFormat::JsonObject(_) => 1,
        ResponseFormat::Markdown(_) => 2,
        ResponseFormat::Yaml(_) => 3,
    }
}

fn centered_rect(width: u16, height: u16, area: Rect) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    Rect::new(
        area.x + area.width.saturating_sub(width) / 2,
        area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    )
}

fn format_elapsed(elapsed: Duration) -> String {
    let seconds = elapsed.as_secs();
    let hours = seconds / 3600;
    let minutes = (seconds % 3600) / 60;
    let seconds = seconds % 60;
    if hours > 0 {
        format!("{hours}h {minutes:02}m {seconds:02}s")
    } else if minutes > 0 {
        format!("{minutes}m {seconds:02}s")
    } else {
        format!("{seconds}s")
    }
}

fn visible_input(value: &[char], cursor: usize, max_width: usize) -> (String, usize) {
    let mut start = cursor;
    let mut cursor_column = 0;
    while start > 0 {
        let width = value[start - 1].width().unwrap_or(0);
        if cursor_column + width > max_width {
            break;
        }
        cursor_column += width;
        start -= 1;
    }

    let mut shown = String::new();
    let mut shown_width = 0;
    for character in &value[start..] {
        let width = character.width().unwrap_or(0);
        if shown_width + width > max_width {
            break;
        }
        shown.push(*character);
        shown_width += width;
    }
    (shown, cursor_column)
}

async fn run_request(
    api_url: String,
    api_key: String,
    model: String,
    messages: Vec<Message>,
    options: CompletionOptions,
    tx: mpsc::UnboundedSender<ReplyEvent>,
) {
    let result = stream_to_ui(&api_url, &api_key, &model, &messages, &options, &tx).await;
    let event = match result {
        Ok(content) => ReplyEvent::Finished(content),
        Err(reason) => ReplyEvent::Failed(reason),
    };
    let _ = tx.send(event);
}

async fn stream_to_ui(
    api_url: &str,
    api_key: &str,
    model: &str,
    messages: &[Message],
    options: &CompletionOptions,
    tx: &mpsc::UnboundedSender<ReplyEvent>,
) -> Result<String, String> {
    let chunk_tx = tx.clone();
    api::complete_streaming(api_url, api_key, model, messages, options, move |chunk| {
        let _ = chunk_tx.send(ReplyEvent::Chunk(chunk.to_owned()));
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app() -> App {
        App::new("https://example.com".into(), "key".into(), "model".into())
    }

    #[test]
    fn commands_change_only_local_options() {
        let mut app = app();
        app.apply_command("/format json {\"answer\":\"...\"}")
            .unwrap();
        app.apply_command("/limit 512").unwrap();
        app.apply_command("/stop <END>").unwrap();

        assert!(matches!(
            app.options.response_format,
            ResponseFormat::JsonObject(_)
        ));
        assert_eq!(app.options.length_limit, LengthLimit::MaxTokens(512));
        assert_eq!(
            app.options.stop_condition,
            StopCondition::Sequence("<END>".into())
        );
        assert!(app.messages.is_empty());
    }

    #[test]
    fn invalid_commands_are_rejected() {
        let mut app = app();
        assert!(app.apply_command("/format yaml").is_err());
        assert!(app.apply_command("/limit 0").is_err());
        assert!(app.apply_command("/unknown").is_err());
    }

    #[test]
    fn commands_without_arguments_open_popups() {
        let mut app = app();

        app.apply_command("/format").unwrap();
        assert!(matches!(
            app.popup,
            Popup::Menu {
                kind: MenuKind::Format,
                ..
            }
        ));

        app.apply_menu_choice(MenuKind::Format, 1);
        assert!(matches!(
            app.popup,
            Popup::Input {
                kind: InputKind::FormatJson,
                ..
            }
        ));
        app.apply_popup_input(InputKind::FormatJson, r#"{"answer":"..."}"#)
            .unwrap();
        assert!(matches!(
            app.options.response_format,
            ResponseFormat::JsonObject(_)
        ));

        app.apply_command("/limit").unwrap();
        assert!(matches!(
            app.popup,
            Popup::Input {
                kind: InputKind::Limit,
                ..
            }
        ));
        app.apply_command("/stop").unwrap();
        assert!(matches!(
            app.popup,
            Popup::Input {
                kind: InputKind::Stop,
                ..
            }
        ));
    }

    #[test]
    fn history_scroll_stops_and_restores_tail_following() {
        let mut app = app();
        app.history_top = 20;
        app.max_history_top = 20;
        app.history_page_height = 8;

        app.scroll_history_up(8);
        assert_eq!(app.history_top, 12);
        assert!(!app.follow_history_tail);

        app.scroll_history_down(100);
        assert_eq!(app.history_top, 20);
        assert!(app.follow_history_tail);
    }

    #[test]
    fn assistant_markdown_is_rendered_as_styled_lines() {
        let lines = markdown_lines(
            "# Заголовок\n\n- **важно** и `код`\n\n```rust\nfn main() {}\n```",
            80,
        );
        let rendered = lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");

        assert!(rendered.contains("Заголовок"));
        assert!(rendered.contains("• важно и  код "));
        assert!(rendered.contains("┌─ rust"));
        assert!(rendered.contains("│ fn main() {}"));
        assert!(rendered.contains("└─"));
        assert!(lines.iter().any(|line| line.spans.iter().any(|span| {
            span.content.contains("Заголовок") && span.style.add_modifier.contains(Modifier::BOLD)
        })));
        assert!(lines.iter().any(|line| line.spans.iter().any(|span| {
            span.content.contains("код") && span.style.bg == Some(Color::DarkGray)
        })));
    }

    #[test]
    fn elapsed_time_has_compact_readable_format() {
        assert_eq!(format_elapsed(Duration::from_secs(9)), "9s");
        assert_eq!(format_elapsed(Duration::from_secs(133)), "2m 13s");
        assert_eq!(format_elapsed(Duration::from_secs(3733)), "1h 02m 13s");
    }

    #[test]
    fn empty_streaming_answer_does_not_render_assistant_label() {
        let mut app = app();
        app.messages.push(Message::new(Role::User, "Задача"));
        app.messages.push(Message::new(Role::Assistant, ""));

        let lines = app.message_lines(80);
        let rendered = lines
            .iter()
            .flat_map(|line| &line.spans)
            .map(|span| span.content.as_ref())
            .collect::<String>();

        assert!(rendered.contains("Вы ›"));
        assert!(!rendered.contains("DeepSeek ›"));
    }
}
