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
        let msg = format!("{}: {}", n.title, n.body);
        let _ = write!(std::io::stderr(), "\x1b]9;{}\x07", msg);
    }
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
