use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::app::Message;

pub async fn event_loop(tx: mpsc::Sender<Message>, cancel: CancellationToken) {
    loop {
        if cancel.is_cancelled() {
            return;
        }

        let event = tokio::task::spawn_blocking(|| {
            event::poll(std::time::Duration::from_millis(100))
                .ok()
                .and_then(|ready| if ready { event::read().ok() } else { None })
        })
        .await;

        match event {
            Ok(Some(Event::Key(key))) => {
                if let Some(msg) = key_to_message(key)
                    && tx.send(msg).await.is_err()
                {
                    return;
                }
            }
            Ok(_) => {}
            Err(_) => return,
        }
    }
}

fn key_to_message(key: KeyEvent) -> Option<Message> {
    match key.code {
        KeyCode::Char('q') => Some(Message::Quit),
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => Some(Message::Quit),
        KeyCode::Up | KeyCode::Char('k') => Some(Message::MoveUp),
        KeyCode::Down | KeyCode::Char('j') => Some(Message::MoveDown),
        KeyCode::Enter | KeyCode::Char('o') => Some(Message::OpenSelected),
        KeyCode::Char('?') => Some(Message::ToggleHelp),
        KeyCode::Char('r') => Some(Message::Refresh),
        _ => None,
    }
}
