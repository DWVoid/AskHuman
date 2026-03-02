//! Iced desktop application – displays question cards sent by MCP agents.

use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

use futures::SinkExt as _;

use iced::{
    Font,
    widget::{button, column, container, row, scrollable, text, text_editor, Space},
    Alignment, Background, Border, Color, Element, Length, Pixels, Subscription, Task, Theme,
};

use crate::mcp_server::{QuestionRequest, ServerCommand};

// ---------------------------------------------------------------------------
// Bootstrap Icons constants  (embedded via Cargo asset in main.rs)
// ---------------------------------------------------------------------------

/// Bootstrap Icons font, referenced by name after loading the bytes.
const ICONS_FONT: Font = Font::with_name("bootstrap-icons");

/// Bootstrap Icons code-point for "check-lg" (U+F72A).
const ICON_CHECK: char = '\u{F72A}';
/// Bootstrap Icons code-point for "x-lg"  (U+F750).
const ICON_X: char = '\u{F750}';

// ---------------------------------------------------------------------------
// Public flags (passed from main)
// ---------------------------------------------------------------------------

pub struct Flags {
    pub question_rx: Arc<Mutex<Option<mpsc::Receiver<QuestionRequest>>>>,
    pub initial_port: u16,
    pub cmd_tx: mpsc::Sender<ServerCommand>,
}

// ---------------------------------------------------------------------------
// Per-card state
// ---------------------------------------------------------------------------

#[derive(Debug)]
enum CardState {
    /// Waiting for the user to type an answer.
    Active,
    /// Answer sent, showing success animation briefly before removal.
    Success,
    /// Error text shown above the input field; user may retry.
    Error(String),
}

struct QuestionCard {
    id: String,
    question: String,
    context: Option<String>,
    editor: text_editor::Content,
    state: CardState,
    answer_tx: Arc<Mutex<Option<tokio::sync::oneshot::Sender<Option<String>>>>>,
}

impl std::fmt::Debug for QuestionCard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("QuestionCard")
            .field("id", &self.id)
            .field("question", &self.question)
            .field("state", &self.state)
            .finish()
    }
}

// ---------------------------------------------------------------------------
// Server state
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub enum ServerState {
    Stopped,
    Running(u16),
}

// ---------------------------------------------------------------------------
// App state
// ---------------------------------------------------------------------------

pub struct App {
    questions: Vec<QuestionCard>,
    question_rx: Arc<Mutex<Option<mpsc::Receiver<QuestionRequest>>>>,
    /// Desired port (editable in the status bar).
    port_input: String,
    server_state: ServerState,
    cmd_tx: mpsc::Sender<ServerCommand>,
}

// ---------------------------------------------------------------------------
// Messages
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub enum Message {
    /// A new question arrived from the MCP server.
    NewQuestion(QuestionRequest),
    /// User edited the answer field for a card.
    EditorAction(String, text_editor::Action),
    /// User hit Ctrl+Enter or clicked Submit.
    Submit(String),
    /// Result of sending the answer to the MCP server.
    Submitted(String, Result<(), String>),
    /// User clicked the ✕ close/reject button.
    Reject(String),
    /// Remove card after the success-animation delay.
    Remove(String),
    /// User changed the port input text.
    PortChanged(String),
    /// User clicked the Start / Stop server toggle.
    ToggleServer,
}

// ---------------------------------------------------------------------------
// App impl
// ---------------------------------------------------------------------------

impl App {
    pub fn new(flags: Flags) -> (Self, Task<Message>) {
        let port_str = flags.initial_port.to_string();
        (
            App {
                questions: Vec::new(),
                question_rx: flags.question_rx,
                port_input: port_str,
                server_state: ServerState::Running(flags.initial_port),
                cmd_tx: flags.cmd_tx,
            },
            Task::none(),
        )
    }

    pub fn title(&self) -> String {
        match &self.server_state {
            ServerState::Running(p) => format!("AskHuman  —  MCP on :{p}"),
            ServerState::Stopped => "AskHuman  —  server stopped".into(),
        }
    }

    pub fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::NewQuestion(req) => {
                self.questions.push(QuestionCard {
                    id: req.id.clone(),
                    question: req.question,
                    context: req.context,
                    editor: text_editor::Content::new(),
                    state: CardState::Active,
                    answer_tx: req.answer_tx,
                });
                Task::none()
            }

            Message::EditorAction(id, action) => {
                if let Some(card) = self.questions.iter_mut().find(|c| c.id == id) {
                    if matches!(card.state, CardState::Active | CardState::Error(_)) {
                        card.editor.perform(action);
                        // Clear error on fresh input
                        if let CardState::Error(_) = card.state {
                            card.state = CardState::Active;
                        }
                    }
                }
                Task::none()
            }

            Message::Submit(id) => {
                let Some(card) = self.questions.iter_mut().find(|c| c.id == id) else {
                    return Task::none();
                };
                // Trim the trailing newline that text_editor.text() always appends.
                let trimmed = card.editor.text().trim_end_matches('\n').trim().to_string();
                if trimmed.is_empty() {
                    card.state =
                        CardState::Error("Please enter an answer before submitting.".into());
                    return Task::none();
                }

                let tx_arc = Arc::clone(&card.answer_tx);
                let cid = id.clone();
                Task::perform(
                    async move {
                        let mut lock = tx_arc.lock().unwrap();
                        match lock.take() {
                            Some(tx) => tx
                                .send(Some(trimmed))
                                .map_err(|_| "Channel closed".to_string()),
                            None => Err("Already answered".into()),
                        }
                    },
                    move |r| Message::Submitted(cid.clone(), r),
                )
            }

            Message::Submitted(id, result) => match result {
                Ok(_) => {
                    if let Some(card) = self.questions.iter_mut().find(|c| c.id == id) {
                        card.state = CardState::Success;
                    }
                    let cid = id.clone();
                    Task::perform(
                        async {
                            tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
                        },
                        move |_| Message::Remove(cid.clone()),
                    )
                }
                Err(e) => {
                    if let Some(card) = self.questions.iter_mut().find(|c| c.id == id) {
                        card.state = CardState::Error(e);
                    }
                    Task::none()
                }
            },

            Message::Reject(id) => {
                if let Some(pos) = self.questions.iter().position(|c| c.id == id) {
                    let card = self.questions.remove(pos);
                    let mut lock = card.answer_tx.lock().unwrap();
                    if let Some(tx) = lock.take() {
                        let _ = tx.send(None);
                    }
                }
                Task::none()
            }

            Message::Remove(id) => {
                self.questions.retain(|c| c.id != id);
                Task::none()
            }

            Message::PortChanged(val) => {
                self.port_input = val;
                Task::none()
            }

            Message::ToggleServer => {
                match &self.server_state {
                    ServerState::Running(_) => {
                        let _ = self.cmd_tx.try_send(ServerCommand::Stop);
                        self.server_state = ServerState::Stopped;
                    }
                    ServerState::Stopped => {
                        if let Ok(port) = self.port_input.trim().parse::<u16>() {
                            let _ = self.cmd_tx.try_send(ServerCommand::Start(port));
                            self.server_state = ServerState::Running(port);
                        }
                        // If the port is invalid, do nothing (the input is red/invalid).
                    }
                }
                Task::none()
            }
        }
    }

    pub fn view(&self) -> Element<'_, Message> {
        let content: Element<Message> = if self.questions.is_empty() {
            container(
                column![
                    text("AskHuman").size(24),
                    Space::with_height(10),
                    text("Waiting for questions from AI agents…").size(13),
                    Space::with_height(6),
                    text("Use Ctrl+Enter to submit answers.")
                        .size(11)
                        .color(Color::from_rgb(0.55, 0.55, 0.55)),
                ]
                .align_x(Alignment::Center),
            )
            .width(Length::Fill)
            .height(Length::Fill)
            .align_x(Alignment::Center)
            .align_y(Alignment::Center)
            .into()
        } else {
            scrollable(
                column(self.questions.iter().map(view_card).collect::<Vec<_>>())
                    .spacing(14)
                    .padding(14)
                    .width(Length::Fill),
            )
            .width(Length::Fill)
            .height(Length::Fill)
            .into()
        };

        let status = self.view_status_bar();
        column![content, status].into()
    }

    fn view_status_bar(&self) -> Element<'_, Message> {
        let is_running = matches!(self.server_state, ServerState::Running(_));
        let port_valid = self.port_input.trim().parse::<u16>().is_ok();

        // Port text field (only editable when stopped)
        let port_field: Element<'_, Message> = if is_running {
            // Non-editable label when running
            text(&self.port_input)
                .size(11)
                .color(Color::from_rgb(0.3, 0.3, 0.3))
                .into()
        } else {
            iced::widget::text_input("port", &self.port_input)
                .on_input(Message::PortChanged)
                .size(11)
                .width(Length::Fixed(60.0))
                .padding([2, 6])
                .style(move |theme: &Theme, status| {
                    let mut s = iced::widget::text_input::default(theme, status);
                    if !port_valid {
                        s.border.color = Color::from_rgb(0.82, 0.1, 0.1);
                    }
                    s
                })
                .into()
        };

        // Endpoint URL label
        let endpoint_label: Element<'_, Message> = if is_running {
            if let ServerState::Running(p) = &self.server_state {
                text(format!("http://127.0.0.1:{p}/mcp"))
                    .size(11)
                    .color(Color::from_rgb(0.3, 0.3, 0.3))
                    .into()
            } else {
                Space::with_width(0).into()
            }
        } else {
            text("(server stopped)")
                .size(11)
                .color(Color::from_rgb(0.55, 0.55, 0.55))
                .into()
        };

        // Toggle button
        let toggle_label = if is_running { "Stop" } else { "Start" };
        let toggle_btn = {
            let b = button(text(toggle_label).size(11))
                .padding([2, 10])
                .style(if is_running {
                    button::danger
                } else {
                    button::primary
                });
            if is_running || port_valid {
                b.on_press(Message::ToggleServer)
            } else {
                b
            }
        };

        let bar_row = row![
            text("MCP  ")
                .size(11)
                .color(Color::from_rgb(0.4, 0.4, 0.4)),
            port_field,
            Space::with_width(8),
            endpoint_label,
            Space::with_width(Length::Fill),
            toggle_btn,
        ]
        .align_y(Alignment::Center)
        .spacing(4);

        container(bar_row)
            .width(Length::Fill)
            .padding([4, 12])
            .style(|_theme: &Theme| container::Style {
                background: Some(Background::Color(Color::from_rgb(0.92, 0.92, 0.92))),
                ..Default::default()
            })
            .into()
    }

    pub fn subscription(&self) -> Subscription<Message> {
        let rx_arc = Arc::clone(&self.question_rx);

        // iced 0.13: Subscription::run_with_id + iced::stream::channel.
        // The stream::channel closure is FnOnce, so the receiver is taken
        // exactly once.  run_with_id uses a stable hash derived from the id,
        // so iced keeps this subscription alive for the whole session.
        Subscription::run_with_id(
            std::any::TypeId::of::<App>(),
            iced::stream::channel(128, move |mut output| async move {
                let rx_opt = {
                    let mut guard = rx_arc.lock().unwrap();
                    guard.take()
                };

                let Some(mut rx) = rx_opt else {
                    std::future::pending::<()>().await;
                    return;
                };

                loop {
                    match rx.recv().await {
                        Some(q) => {
                            let _ = output.send(Message::NewQuestion(q)).await;
                        }
                        None => {
                            std::future::pending::<()>().await;
                            return;
                        }
                    }
                }
            }),
        )
    }

    pub fn theme(&self) -> Theme {
        Theme::Light
    }
}

// ---------------------------------------------------------------------------
// Card view helper
// ---------------------------------------------------------------------------

fn view_card(card: &QuestionCard) -> Element<'_, Message> {
    let is_success = matches!(card.state, CardState::Success);

    // ── Header: question text + close button ─────────────────────────────
    let close_btn = {
        let b = button(
            text(ICON_X.to_string())
                .font(ICONS_FONT)
                .size(13),
        )
        .style(button::text)
        .padding([2, 6]);
        if is_success {
            b
        } else {
            b.on_press(Message::Reject(card.id.clone()))
        }
    };

    let header = row![
        text(&card.question).size(13).width(Length::Fill),
        close_btn,
    ]
    .align_y(Alignment::Start)
    .spacing(8);

    // ── Body ──────────────────────────────────────────────────────────────
    let body: Element<Message> = if is_success {
        column![
            header,
            Space::with_height(6),
            row![
                text(ICON_CHECK.to_string())
                    .font(ICONS_FONT)
                    .size(14)
                    .color(SUCCESS_GREEN),
                Space::with_width(6),
                text("Answer submitted successfully!")
                    .size(12)
                    .color(SUCCESS_GREEN),
            ]
            .align_y(Alignment::Center),
        ]
        .spacing(4)
        .into()
    } else {
        let mut col = column![header].spacing(5);

        // Optional context
        if let Some(ctx) = &card.context {
            col = col.push(
                text(ctx)
                    .size(11)
                    .color(Color::from_rgb(0.45, 0.45, 0.45)),
            );
        }

        // Error tip (red, above the input)
        if let CardState::Error(ref msg) = card.state {
            col = col.push(text(msg).size(11).color(Color::from_rgb(0.82, 0.1, 0.1)));
        }

        // Multi-line answer editor – grows with content, max 300 px before
        // internal scrolling kicks in.
        let line_count = card.editor.line_count();
        let editor_height = ((line_count as f32 * EDITOR_LINE_HEIGHT) + EDITOR_PADDING_HEIGHT)
            .clamp(EDITOR_MIN_HEIGHT, EDITOR_MAX_HEIGHT);

        let cid = card.id.clone();
        let cid2 = card.id.clone();
        col = col.push(
            text_editor(&card.editor)
                .placeholder("Type your answer here…")
                .height(Pixels(editor_height))
                .on_action(move |a| Message::EditorAction(cid.clone(), a))
                .key_binding(move |kp| {
                    use iced::keyboard::key::Named;
                    let is_enter = matches!(
                        kp.key,
                        iced::keyboard::Key::Named(Named::Enter)
                    );
                    if is_enter && kp.modifiers.control() {
                        Some(text_editor::Binding::Custom(Message::Submit(cid2.clone())))
                    } else {
                        text_editor::Binding::from_key_press(kp)
                    }
                }),
        );

        // Submit row: hint on left, button on right
        col = col.push(
            row![
                text("Ctrl+Enter to submit")
                    .size(10)
                    .color(Color::from_rgb(0.6, 0.6, 0.6)),
                Space::with_width(Length::Fill),
                button(text("Submit Answer").size(12))
                    .on_press(Message::Submit(card.id.clone()))
                    .style(button::primary)
                    .padding([6, 16]),
            ]
            .align_y(Alignment::Center),
        );

        col.into()
    };

    // ── Card container ────────────────────────────────────────────────────
    let border_color = if is_success {
        SUCCESS_GREEN
    } else if matches!(card.state, CardState::Error(_)) {
        Color::from_rgb(0.85, 0.25, 0.25)
    } else {
        Color::from_rgb(0.82, 0.82, 0.82)
    };

    container(body)
        .width(Length::Fill)
        .padding(14)
        .style(move |_theme: &Theme| container::Style {
            background: Some(Background::Color(Color::WHITE)),
            border: Border {
                color: border_color,
                width: 1.5,
                radius: 8.0.into(),
            },
            ..Default::default()
        })
        .into()
}

const SUCCESS_GREEN: Color = Color {
    r: 0.13,
    g: 0.67,
    b: 0.3,
    a: 1.0,
};

/// Approximate rendered line height in pixels for the answer text editor.
const EDITOR_LINE_HEIGHT: f32 = 19.0;
/// Vertical padding (top + bottom) added to the editor height.
const EDITOR_PADDING_HEIGHT: f32 = 22.0;
/// Minimum editor height in pixels (one line with padding).
const EDITOR_MIN_HEIGHT: f32 = 58.0;
/// Maximum editor height in pixels before internal scrolling takes over.
const EDITOR_MAX_HEIGHT: f32 = 300.0;
