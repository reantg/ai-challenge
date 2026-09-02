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
use pulldown_cmark::{CodeBlockKind, Event as MdEvent, Options, Parser, Tag, TagEnd};
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Wrap},
};
use tokio::sync::mpsc;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::api::{self, Message, Role};

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

pub async fn run(api_url: String, api_key: String, api_model: String) -> io::Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(stdout))?;
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
                    MouseEventKind::ScrollUp => app.scroll_history_up(3),
                    MouseEventKind::ScrollDown => app.scroll_history_down(3),
                    _ => {}
                },
                Event::Paste(value) => app.insert_text(&value),
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
        if key.code == KeyCode::Esc {
            self.quit = true;
            return;
        }
        match key.code {
            KeyCode::Up => return self.scroll_history_up(1),
            KeyCode::Down => return self.scroll_history_down(1),
            KeyCode::PageUp => return self.scroll_history_up(self.history_page_height),
            KeyCode::PageDown => return self.scroll_history_down(self.history_page_height),
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
            KeyCode::Char(ch)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                self.insert_char(ch)
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

    fn insert_char(&mut self, ch: char) {
        if self.input.len() < MAX_INPUT_CHARS
            && !matches!(self.request_state, RequestState::Waiting(_))
        {
            self.input.insert(self.cursor, ch);
            self.cursor += 1;
        }
    }

    fn insert_text(&mut self, value: &str) {
        for ch in value.chars().filter(|ch| !ch.is_control()) {
            self.insert_char(ch);
        }
    }

    fn submit(&mut self) {
        let content = self.input.iter().collect::<String>().trim().to_owned();
        if content.is_empty() {
            return;
        }
        self.input.clear();
        self.cursor = 0;
        self.messages.push(Message::new(Role::User, content));
        self.request_state = RequestState::Waiting(Instant::now());
        let tx = self.reply_tx.clone();
        let api_url = self.api_url.clone();
        let api_key = self.api_key.clone();
        let model = self.api_model.clone();
        let messages = self.messages.clone();
        self.messages.push(Message::new(Role::Assistant, ""));
        tokio::spawn(async move {
            let chunk_tx = tx.clone();
            let result =
                api::complete_streaming(&api_url, &api_key, &model, &messages, move |chunk| {
                    let _ = chunk_tx.send(ReplyEvent::Chunk(chunk.to_owned()));
                })
                .await;
            let event = match result {
                Ok(content) => ReplyEvent::Finished(content),
                Err(reason) => ReplyEvent::Failed(reason),
            };
            let _ = tx.send(event);
        });
    }

    fn receive_reply(&mut self) {
        if !matches!(self.request_state, RequestState::Waiting(_)) {
            return;
        }
        while let Ok(event) = self.reply_rx.try_recv() {
            match event {
                ReplyEvent::Chunk(chunk) => {
                    if let Some(Message {
                        role: Role::Assistant,
                        content,
                    }) = self.messages.last_mut()
                    {
                        content.push_str(&chunk);
                    }
                }
                ReplyEvent::Finished(answer) => {
                    if let Some(Message {
                        role: Role::Assistant,
                        content,
                    }) = self.messages.last_mut()
                    {
                        *content = answer;
                    }
                    self.request_state = RequestState::Ready;
                }
                ReplyEvent::Failed(reason) => {
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

    fn view(&mut self, frame: &mut Frame) {
        let areas = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Min(1),
                Constraint::Length(3),
                Constraint::Length(1),
            ])
            .split(frame.area());
        self.render_feed(frame, areas[0]);
        self.render_input(frame, areas[1]);
        self.render_status(frame, areas[2]);
    }

    fn render_feed(&mut self, frame: &mut Frame, area: Rect) {
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(ratatui::widgets::BorderType::Rounded)
            .title(" DeepSeek Chat ")
            .border_style(Style::default().fg(Color::DarkGray));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        let mut lines = self.message_lines(inner.width.saturating_sub(11).max(1) as usize);
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

    fn message_lines(&self, width: usize) -> Vec<Line<'static>> {
        let mut lines = Vec::new();
        for message in &self.messages {
            if message.role == Role::Assistant && message.content.is_empty() {
                continue;
            }
            let (label, style) = match message.role {
                Role::User => ("Вы ›       ", Style::default().fg(Color::Cyan)),
                Role::Assistant => (
                    "DeepSeek › ",
                    Style::default()
                        .fg(Color::Magenta)
                        .add_modifier(Modifier::BOLD),
                ),
            };
            let content_lines = match message.role {
                Role::Assistant => markdown_lines(&message.content, width),
                Role::User => textwrap::wrap(&message.content, width)
                    .into_iter()
                    .map(|line| Line::raw(line.into_owned()))
                    .collect(),
            };
            for (index, line) in content_lines.into_iter().enumerate() {
                let mut spans = vec![Span::styled(
                    if index == 0 { label } else { "           " }.to_owned(),
                    style,
                )];
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
        let line = if value.is_empty() {
            Line::from(vec![
                Span::raw("› "),
                Span::styled("Введите сообщение…", Style::default().fg(Color::DarkGray)),
            ])
        } else {
            Line::from(format!("› {value}"))
        };
        frame.render_widget(Paragraph::new(line), inner);
        if !matches!(self.request_state, RequestState::Waiting(_)) && inner.width > 0 {
            let before_cursor = self.input[..self.cursor].iter().collect::<String>();
            let x =
                inner.x + ((2 + before_cursor.width()) as u16).min(inner.width.saturating_sub(1));
            frame.set_cursor_position((x, inner.y));
        }
    }

    fn render_status(&self, frame: &mut Frame, area: Rect) {
        let (text, color) = match self.request_state {
            RequestState::Waiting(_) => (
                " Ответ формируется в ленте · ↑/↓ — прокрутка · Esc — выйти",
                Color::DarkGray,
            ),
            RequestState::Ready => (
                " Enter — отправить · ↑/↓ — прокрутка · Esc — выйти",
                Color::DarkGray,
            ),
            RequestState::Failed(_) => (" Повторите сообщение", Color::Red),
        };
        frame.render_widget(Paragraph::new(text).style(Style::default().fg(color)), area);
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
    fn finish_line(&mut self) {
        self.lines.push(std::mem::take(&mut self.current));
    }
    fn finish_block(&mut self) {
        if !self.current.is_empty() {
            self.finish_line();
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
        for (index, part) in text.split('\n').enumerate() {
            if index > 0 {
                self.finish_line();
            }
            if !part.is_empty() {
                if self.current.is_empty() {
                    self.push("│ ", Style::default().fg(Color::DarkGray));
                }
                self.push(part, Style::default().fg(Color::Green));
            }
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
            MdEvent::Start(tag) => match tag {
                Tag::Heading { .. } => {
                    builder.finish_block();
                    builder.styles.push(
                        builder
                            .style()
                            .fg(Color::Yellow)
                            .add_modifier(Modifier::BOLD),
                    );
                }
                Tag::Strong => builder
                    .styles
                    .push(builder.style().add_modifier(Modifier::BOLD)),
                Tag::Emphasis => builder
                    .styles
                    .push(builder.style().add_modifier(Modifier::ITALIC)),
                Tag::Strikethrough => builder
                    .styles
                    .push(builder.style().add_modifier(Modifier::CROSSED_OUT)),
                Tag::Link { .. } => builder.styles.push(
                    builder
                        .style()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::UNDERLINED),
                ),
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
                    let title = match kind {
                        CodeBlockKind::Fenced(lang) if !lang.is_empty() => format!("┌─ {lang}"),
                        _ => "┌─ code".into(),
                    };
                    builder.push(title, Style::default().fg(Color::DarkGray));
                    builder.finish_line();
                    builder.in_code_block = true;
                }
                Tag::List(start) => builder.lists.push(start),
                Tag::Item => builder.start_item(),
                Tag::TableCell if !builder.current.is_empty() => {
                    builder.push(" │ ", Style::default().fg(Color::DarkGray))
                }
                _ => {}
            },
            MdEvent::End(tag) => match tag {
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
            MdEvent::Text(text) => {
                if builder.in_code_block {
                    builder.push_code(&text)
                } else {
                    builder.push_text(&text)
                }
            }
            MdEvent::Code(code) => builder.push(
                format!(" {code} "),
                Style::default().fg(Color::Yellow).bg(Color::DarkGray),
            ),
            MdEvent::SoftBreak => builder.push(" ", builder.style()),
            MdEvent::HardBreak => builder.finish_line(),
            MdEvent::Rule => {
                builder.finish_block();
                builder.push("────────────────", Style::default().fg(Color::DarkGray));
                builder.finish_line();
            }
            MdEvent::TaskListMarker(checked) => builder.push(
                if checked { "[x] " } else { "[ ] " },
                Style::default().fg(Color::Cyan),
            ),
            MdEvent::InlineMath(math) | MdEvent::DisplayMath(math) => {
                builder.push(math.into_string(), Style::default().fg(Color::Yellow))
            }
            MdEvent::Html(html) | MdEvent::InlineHtml(html) => builder.push_text(&html),
            MdEvent::FootnoteReference(reference) => {
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
        for ch in text.chars() {
            let char_width = ch.width().unwrap_or(0);
            if column > 0 && column + char_width > width {
                lines.push(Vec::new());
                column = 0;
            }
            if column == 0 && ch.is_whitespace() {
                continue;
            }
            let current = lines.last_mut().expect("at least one wrapped line");
            if let Some((existing, existing_style)) = current.last_mut()
                && *existing_style == style
            {
                existing.push(ch);
            } else {
                current.push((ch.to_string(), style));
            }
            column += char_width;
        }
    }
    lines
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

#[cfg(test)]
mod tests {
    use super::*;

    fn app() -> App {
        App::new("https://example.com".into(), "key".into(), "model".into())
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
    fn assistant_markdown_is_rendered() {
        let lines = markdown_lines(
            "# Заголовок\n\n- **важно** и `код`\n\n```rust\nfn main() {}\n```",
            80,
        );
        let rendered = lines
            .iter()
            .flat_map(|line| &line.spans)
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert!(rendered.contains("Заголовок"));
        assert!(rendered.contains("важно"));
        assert!(rendered.contains("┌─ rust"));
        assert!(rendered.contains("fn main() {}"));
    }

    #[test]
    fn elapsed_time_is_readable() {
        assert_eq!(format_elapsed(Duration::from_secs(9)), "9s");
        assert_eq!(format_elapsed(Duration::from_secs(133)), "2m 13s");
        assert_eq!(format_elapsed(Duration::from_secs(3733)), "1h 02m 13s");
    }

    #[test]
    fn empty_streaming_answer_has_no_assistant_label() {
        let mut app = app();
        app.messages.push(Message::new(Role::User, "Задача"));
        app.messages.push(Message::new(Role::Assistant, ""));
        let rendered = app
            .message_lines(80)
            .iter()
            .flat_map(|line| &line.spans)
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert!(rendered.contains("Вы ›"));
        assert!(!rendered.contains("DeepSeek ›"));
    }
}
