//! AskHuman – desktop MCP tool for letting AI agents ask the human operator.
//!
//! Runs an MCP Streamable HTTP server (POST /mcp, 2025-03-26 spec) on a
//! configurable port (default 3000, controllable from the status bar) and an
//! iced desktop window on the main thread.

mod app;
mod mcp_server;

use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

fn main() -> iced::Result {
    let initial_port: u16 = std::env::var("ASKHUMAN_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(3000);

    let (question_tx, question_rx) = mpsc::channel::<mcp_server::QuestionRequest>(128);
    let question_rx = Arc::new(Mutex::new(Some(question_rx)));

    // Command channel: UI → background server runtime.
    let (cmd_tx, cmd_rx) = mpsc::channel::<mcp_server::ServerCommand>(8);

    // Background thread: owns the tokio runtime + MCP server lifecycle.
    let question_tx_bg = question_tx.clone();
    std::thread::spawn(move || {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("tokio runtime")
            .block_on(mcp_server::server_loop(question_tx_bg, cmd_rx));
    });

    // Send the initial start command (fire-and-forget; background thread will
    // receive it once the tokio runtime is running).
    let _ = cmd_tx.try_send(mcp_server::ServerCommand::Start(initial_port));

    // Build window icon from the embedded 32×32 RGBA asset.
    let icon = iced::window::icon::from_rgba(
        include_bytes!("../assets/icon-32.rgba").to_vec(),
        32,
        32,
    )
    .ok();

    // Run the iced UI on the main thread.
    iced::application(App::title, App::update, App::view)
        .subscription(App::subscription)
        .theme(App::theme)
        .window(iced::window::Settings {
            size: iced::Size::new(660.0, 800.0),
            icon,
            ..Default::default()
        })
        // Embed Bootstrap Icons (MIT) for consistent ✓ / ✕ rendering.
        .font(include_bytes!("../assets/fonts/bootstrap-icons.ttf").as_slice())
        .run_with(move || {
            App::new(app::Flags {
                question_rx,
                initial_port,
                cmd_tx,
            })
        })
}

// Re-export the app struct so the iced entry-point can find it.
use app::App;
