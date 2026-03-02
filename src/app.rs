//! Iced desktop application – displays question cards sent by MCP agents.

use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

use futures::SinkExt as _;

use iced::{
    widget::{button, column, container, row, scrollable, text, text_input, Space},
    Alignment, Background, Border, Color, Element, Length, Subscription, Task, Theme,
};

use crate::mcp_server::QuestionRequest;

// ---------------------------------------------------------------------------
// Public flags (passed from main)
// ---------------------------------------------------------------------------

pub struct Flags {
    pub question_rx: Arc<Mutex<Option<mpsc::Receiver<QuestionRequest>>>>,
    pub port: u16,
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

#[derive(Debug)]
struct QuestionCard {
    id: String,
    question: String,
    context: Option<String>,
    answer_input: String,
    state: CardState,
    answer_tx: Arc<Mutex<Option<tokio::sync::oneshot::Sender<Option<String>>>>>,
}

// ---------------------------------------------------------------------------
// App state
// ---------------------------------------------------------------------------

pub struct App {
    questions: Vec<QuestionCard>,
    question_rx: Arc<Mutex<Option<mpsc::Receiver<QuestionRequest>>>>,
    port: u16,
}

// ---------------------------------------------------------------------------
// Messages
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub enum Message {
    /// A new question arrived from the MCP server.
    NewQuestion(QuestionRequest),
    /// User edited the answer field for a card.
    AnswerChanged(String, String),
    /// User clicked Submit (or pressed Enter).
    Submit(String),
    /// Result of sending the answer to the MCP server.
    Submitted(String, Result<(), String>),
    /// User clicked the ✕ close/reject button.
    Reject(String),
    /// Remove card after the success-animation delay.
    Remove(String),
}

// ---------------------------------------------------------------------------
// App impl
// ---------------------------------------------------------------------------

impl App {
    pub fn new(flags: Flags) -> (Self, Task<Message>) {
        (
            App {
                questions: Vec::new(),
                question_rx: flags.question_rx,
                port: flags.port,
            },
            Task::none(),
        )
    }

    pub fn title(&self) -> String {
        format!("AskHuman  —  MCP on :{}", self.port)
    }

    pub fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::NewQuestion(req) => {
                self.questions.push(QuestionCard {
                    id: req.id.clone(),
                    question: req.question,
                    context: req.context,
                    answer_input: String::new(),
                    state: CardState::Active,
                    answer_tx: req.answer_tx,
                });
                Task::none()
            }

            Message::AnswerChanged(id, value) => {
                if let Some(card) = self.questions.iter_mut().find(|c| c.id == id) {
                    if matches!(card.state, CardState::Active | CardState::Error(_)) {
                        card.answer_input = value;
                    }
                }
                Task::none()
            }

            Message::Submit(id) => {
                let Some(card) = self.questions.iter_mut().find(|c| c.id == id) else {
                    return Task::none();
                };
                let trimmed = card.answer_input.trim().to_string();
                if trimmed.is_empty() {
                    card.state =
                        CardState::Error("Please enter an answer before submitting.".into());
                    return Task::none();
                }

                let tx_arc = Arc::clone(&card.answer_tx);
                // Clone the id so the Fn closure can reference it without moving.
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
                    // The closure must be Fn; clone cid each invocation.
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
        }
    }

    pub fn view(&self) -> Element<'_, Message> {
        let content: Element<Message> = if self.questions.is_empty() {
            container(
                column![
                    text("AskHuman").size(28),
                    Space::with_height(12),
                    text("Waiting for questions from AI agents…").size(15),
                    Space::with_height(8),
                    text(format!(
                        "MCP endpoint:  http://127.0.0.1:{}/mcp",
                        self.port
                    ))
                    .size(12)
                    .color(Color::from_rgb(0.5, 0.5, 0.5)),
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
                    .spacing(16)
                    .padding(16)
                    .width(Length::Fill),
            )
            .width(Length::Fill)
            .height(Length::Fill)
            .into()
        };

        let status = container(
            text(format!(
                "MCP endpoint:  http://127.0.0.1:{}/mcp",
                self.port
            ))
            .size(11)
            .color(Color::from_rgb(0.4, 0.4, 0.4)),
        )
        .width(Length::Fill)
        .padding([4, 12])
        .style(|_theme: &Theme| container::Style {
            background: Some(Background::Color(Color::from_rgb(0.92, 0.92, 0.92))),
            ..Default::default()
        });

        column![content, status].into()
    }

    pub fn subscription(&self) -> Subscription<Message> {
        let rx_arc = Arc::clone(&self.question_rx);

        // iced 0.13: use Subscription::run_with_id + iced::stream::channel.
        // The stream::channel closure is FnOnce, so the receiver is taken
        // exactly once.  run_with_id uses a stable hash derived from the
        // id, so iced keeps this subscription alive for the whole session.
        Subscription::run_with_id(
            std::any::TypeId::of::<App>(),
            iced::stream::channel(128, move |mut output| async move {
                // Take the receiver; drop the guard before any await point.
                let rx_opt = {
                    let mut guard = rx_arc.lock().unwrap();
                    guard.take()
                    // guard dropped here
                };
                let mut rx = match rx_opt {
                    Some(r) => r,
                    None => std::future::pending::<mpsc::Receiver<QuestionRequest>>().await,
                };
                loop {
                    match rx.recv().await {
                        Some(q) => {
                            let _ = output.send(Message::NewQuestion(q)).await;
                        }
                        None => std::future::pending::<()>().await,
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
        let b = button(text("✕").size(13)).style(button::text).padding([2, 6]);
        if is_success {
            b
        } else {
            b.on_press(Message::Reject(card.id.clone()))
        }
    };

    let header = row![
        text(&card.question).size(15).width(Length::Fill),
        close_btn,
    ]
    .align_y(Alignment::Start)
    .spacing(8);

    // ── Body ──────────────────────────────────────────────────────────────
    let body: Element<Message> = if is_success {
        column![
            header,
            Space::with_height(8),
            text("✓  Answer submitted successfully!")
                .size(14)
                .color(SUCCESS_GREEN),
        ]
        .spacing(4)
        .into()
    } else {
        let mut col = column![header].spacing(6);

        // Optional context
        if let Some(ctx) = &card.context {
            col = col.push(
                text(ctx)
                    .size(12)
                    .color(Color::from_rgb(0.45, 0.45, 0.45)),
            );
        }

        // Error tip (red, above the input)
        if let CardState::Error(ref msg) = card.state {
            col = col.push(text(msg).size(12).color(Color::from_rgb(0.82, 0.1, 0.1)));
        }

        // Answer text input
        col = col.push(
            text_input("Type your answer here…", &card.answer_input)
                .on_input(|v| Message::AnswerChanged(card.id.clone(), v))
                .on_submit(Message::Submit(card.id.clone()))
                .padding(10)
                .width(Length::Fill),
        );

        // Submit button (right-aligned)
        col = col.push(
            row![
                Space::with_width(Length::Fill),
                button(text("Submit Answer"))
                    .on_press(Message::Submit(card.id.clone()))
                    .style(button::primary)
                    .padding([8, 18]),
            ],
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
        .padding(16)
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
