mod app;
mod colors;
mod config;
mod diff;
mod error;
mod github;
mod notify;
mod poller;
mod tui;
mod types;

use std::time::Duration;

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use app::{App, Message};
use config::Config;
use github::client::GitHubClient;
use notify::build_notifier;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let config = Config::load()?;

    let cancel = CancellationToken::new();
    let (msg_tx, mut msg_rx) = mpsc::channel::<Message>(64);
    let (refresh_tx, refresh_rx) = mpsc::channel::<()>(1);

    // Panic hook to restore terminal
    let default_panic = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = tui::terminal::restore();
        default_panic(info);
    }));

    let notifier = build_notifier(config.notify_enabled);

    let mut terminal = tui::terminal::init()?;
    let mut app = App::new(
        config.username.clone(),
        config.color_scheme.clone(),
        config.notify_events.clone(),
    );

    // Spawn event reader
    let event_cancel = cancel.clone();
    let event_tx = msg_tx.clone();
    tokio::spawn(async move {
        tui::event::event_loop(event_tx, event_cancel).await;
    });

    // Spawn poller
    let client = GitHubClient::new(config.github_token.clone());
    let poll_cancel = cancel.clone();
    let poll_tx = msg_tx.clone();
    let error_tx = msg_tx.clone();
    let username = config.username.clone();
    let interval = Duration::from_secs(config.poll_interval_secs);

    tokio::spawn(async move {
        let error_sender = {
            let tx = error_tx;
            let (err_tx, mut err_rx) = mpsc::channel::<String>(64);
            let forward_tx = tx;
            tokio::spawn(async move {
                while let Some(msg) = err_rx.recv().await {
                    let _ = forward_tx.send(Message::PollError(msg)).await;
                }
            });
            err_tx
        };

        let poll_sender = {
            let tx = poll_tx;
            let (payload_tx, mut payload_rx) = mpsc::channel::<poller::PollPayload>(64);
            tokio::spawn(async move {
                while let Some(payload) = payload_rx.recv().await {
                    let _ = tx.send(Message::PollResult(payload)).await;
                }
            });
            payload_tx
        };

        poller::polling_loop(
            client,
            username,
            interval,
            poll_sender,
            error_sender,
            poll_cancel,
            refresh_rx,
        )
        .await;
    });

    // Initial draw
    terminal.draw(|f| tui::ui::view(f, &mut app))?;
    app.dirty = false;

    // Main loop
    loop {
        tokio::select! {
            Some(msg) = msg_rx.recv() => {
                let is_refresh = matches!(msg, Message::Refresh);
                app.update(msg);
                if is_refresh {
                    let _ = refresh_tx.send(()).await;
                }
                for n in app.pending_notifications.drain(..) {
                    notifier.notify(&n);
                }
            }
            _ = tokio::time::sleep(Duration::from_millis(200)) => {
                const INACTIVITY_SECS: u64 = 5;
                let inactive = app.last_activity
                    .map(|t| t.elapsed() > Duration::from_secs(INACTIVITY_SECS))
                    .unwrap_or(false);
                if inactive && app.list_state.selected().is_some() {
                    app.update(Message::Deselect);
                }
            }
            _ = cancel.cancelled() => {
                break;
            }
        }

        if app.should_quit {
            break;
        }

        if app.dirty {
            terminal.draw(|f| tui::ui::view(f, &mut app))?;
            app.dirty = false;
        }
    }

    cancel.cancel();
    tui::terminal::restore()?;

    Ok(())
}
