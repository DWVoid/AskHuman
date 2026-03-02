//! AskHuman – desktop MCP tool for letting AI agents ask the human operator.
//!
//! Runs an MCP Streamable HTTP server (POST /mcp, 2025-03-26 spec) on
//! `ASKHUMAN_PORT` (default 3000) and an iced desktop window on the main thread.

mod app;
mod mcp_server;

use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

fn main() -> iced::Result {
    let port: u16 = std::env::var("ASKHUMAN_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(3000);

    let (question_tx, question_rx) = mpsc::channel::<mcp_server::QuestionRequest>(128);
    let question_rx = Arc::new(Mutex::new(Some(question_rx)));

    // Start the MCP HTTP server on a dedicated tokio runtime in a background thread.
    std::thread::spawn(move || {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("tokio runtime")
            .block_on(mcp_server::run(question_tx, port));
    });

    // Run the iced UI on the main thread (required on most platforms).
    iced::application(App::title, App::update, App::view)
        .subscription(App::subscription)
        .theme(App::theme)
        .window_size(iced::Size::new(660.0, 800.0))
        .run_with(move || App::new(app::Flags { question_rx, port }))
}

// Re-export the app struct so the iced entry-point can find it.
use app::App;
