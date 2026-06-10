use std::io::Write;

pub struct Notification {
    pub title: String,
    pub body: String,
}

pub trait Notifier: Send {
    fn notify(&self, n: &Notification);
}

/// OSC 9 escape sequence to stderr.
/// Supported by: WezTerm, Windows Terminal, iTerm2 (with plugin), etc.
pub struct OscNotifier;

impl Notifier for OscNotifier {
    fn notify(&self, n: &Notification) {
        let _ = write!(std::io::stderr(), "\x1b]9;{}\x07", osc9_message(n));
    }
}

/// PR タイトル等に BEL や ESC などの制御文字が含まれていると、OSC シーケンスが
/// 途中で終端されたり任意のエスケープシーケンスを注入できてしまうため除去する。
fn osc9_message(n: &Notification) -> String {
    format!("{}: {}", n.title, n.body)
        .chars()
        .filter(|c| !c.is_control())
        .collect()
}

pub struct NullNotifier;

impl Notifier for NullNotifier {
    fn notify(&self, _n: &Notification) {}
}

pub fn build_notifier(enabled: bool) -> Box<dyn Notifier> {
    if enabled {
        Box::new(OscNotifier)
    } else {
        Box::new(NullNotifier)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn osc9_message_plain() {
        let n = Notification {
            title: "PR merged".to_string(),
            body: "Fix stuff (org/repo#1)".to_string(),
        };
        assert_eq!(osc9_message(&n), "PR merged: Fix stuff (org/repo#1)");
    }

    #[test]
    fn osc9_message_strips_control_chars() {
        let n = Notification {
            title: "PR merged".to_string(),
            body: "evil\x07\x1b]0;pwned\x07title\nnewline".to_string(),
        };
        let msg = osc9_message(&n);
        assert!(!msg.chars().any(|c| c.is_control()), "got: {msg:?}");
        assert_eq!(msg, "PR merged: evil]0;pwnedtitlenewline");
    }
}
