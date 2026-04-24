use std::collections::HashSet;
use std::path::PathBuf;

use clap::Parser;
use serde::Deserialize;

use crate::colors::{ColorScheme, parse_color};
use crate::error::AppError;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotifyEvent {
    ReviewRequested,
    PrClosed,
    PrMerged,
    ReReviewRequested,
    NewComment,
}

impl NotifyEvent {
    pub fn all() -> HashSet<NotifyEvent> {
        HashSet::from([
            NotifyEvent::ReviewRequested,
            NotifyEvent::PrClosed,
            NotifyEvent::PrMerged,
            NotifyEvent::ReReviewRequested,
            NotifyEvent::NewComment,
        ])
    }
}

#[derive(Parser, Debug)]
#[command(name = "prtop", about = "GitHub PR Live Viewer")]
struct Cli {
    #[arg(long, env = "PRTOP_GITHUB_TOKEN")]
    github_token: Option<String>,

    #[arg(long, env = "PRTOP_GITHUB_USERNAME")]
    username: Option<String>,

    #[arg(long)]
    poll_interval_secs: Option<u64>,
}

#[derive(Debug, Deserialize, Default)]
struct NotifyFileConfig {
    enabled: Option<bool>,
    events: Option<Vec<NotifyEvent>>,
}

#[derive(Debug, Deserialize, Default)]
struct ColorsFileConfig {
    app_title: Option<String>,
    col_header: Option<String>,
    role: Option<String>,
    number: Option<String>,
    repo: Option<String>,
    new_pr: Option<String>,
    new_comment: Option<String>,
    draft: Option<String>,
    footer_count: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
struct FileConfig {
    github_token: Option<String>,
    username: Option<String>,
    poll_interval_secs: Option<u64>,
    notify: Option<NotifyFileConfig>,
    colors: Option<ColorsFileConfig>,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub github_token: String,
    pub username: String,
    pub poll_interval_secs: u64,
    pub notify_enabled: bool,
    pub notify_events: HashSet<NotifyEvent>,
    pub color_scheme: ColorScheme,
}

pub(crate) fn resolve_poll_interval(cli: Option<u64>, file: Option<u64>) -> u64 {
    cli.or(file).unwrap_or(60)
}

pub(crate) fn resolve_notify_events(events: Option<Vec<NotifyEvent>>) -> HashSet<NotifyEvent> {
    match events {
        Some(v) => v.into_iter().collect(),
        None => NotifyEvent::all(),
    }
}

fn config_path() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("prtop").join("config.toml"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn poll_interval_file_wins_when_cli_not_specified() {
        assert_eq!(resolve_poll_interval(None, Some(30)), 30);
    }

    #[test]
    fn poll_interval_cli_wins_over_file() {
        assert_eq!(resolve_poll_interval(Some(120), Some(30)), 120);
    }

    #[test]
    fn poll_interval_defaults_to_60() {
        assert_eq!(resolve_poll_interval(None, None), 60);
    }

    // --- NotifyEvent ---

    #[test]
    fn notify_event_all_contains_every_variant() {
        let all = NotifyEvent::all();
        assert_eq!(all.len(), 5);
        assert!(all.contains(&NotifyEvent::ReviewRequested));
        assert!(all.contains(&NotifyEvent::PrClosed));
        assert!(all.contains(&NotifyEvent::PrMerged));
        assert!(all.contains(&NotifyEvent::ReReviewRequested));
        assert!(all.contains(&NotifyEvent::NewComment));
    }

    #[test]
    fn notify_event_deserialize_snake_case() {
        let cases = [
            ("\"review_requested\"", NotifyEvent::ReviewRequested),
            ("\"pr_closed\"", NotifyEvent::PrClosed),
            ("\"pr_merged\"", NotifyEvent::PrMerged),
            ("\"re_review_requested\"", NotifyEvent::ReReviewRequested),
            ("\"new_comment\"", NotifyEvent::NewComment),
        ];
        for (json, expected) in cases {
            let parsed: NotifyEvent = serde_json::from_str(json).unwrap();
            assert_eq!(parsed, expected);
        }
    }

    #[test]
    fn notify_event_deserialize_unknown_fails() {
        let result: Result<NotifyEvent, _> = serde_json::from_str("\"unknown_event\"");
        assert!(result.is_err());
    }

    // --- resolve_notify_events ---

    #[test]
    fn resolve_events_none_returns_all() {
        let events = resolve_notify_events(None);
        assert_eq!(events, NotifyEvent::all());
    }

    #[test]
    fn resolve_events_some_returns_specified() {
        let events = resolve_notify_events(Some(vec![
            NotifyEvent::ReviewRequested,
            NotifyEvent::PrClosed,
        ]));
        assert_eq!(events.len(), 2);
        assert!(events.contains(&NotifyEvent::ReviewRequested));
        assert!(events.contains(&NotifyEvent::PrClosed));
    }

    #[test]
    fn resolve_events_empty_returns_empty() {
        let events = resolve_notify_events(Some(vec![]));
        assert!(events.is_empty());
    }

    // --- TOML deserialization ---

    #[test]
    fn toml_notify_with_events() {
        let toml_str = r#"
            enabled = true
            events = ["review_requested", "pr_merged"]
        "#;
        let parsed: NotifyFileConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(parsed.enabled, Some(true));
        let events = parsed.events.unwrap();
        assert_eq!(events.len(), 2);
        assert!(events.contains(&NotifyEvent::ReviewRequested));
        assert!(events.contains(&NotifyEvent::PrMerged));
    }

    #[test]
    fn toml_notify_without_events() {
        let toml_str = r#"
            enabled = true
        "#;
        let parsed: NotifyFileConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(parsed.enabled, Some(true));
        assert!(parsed.events.is_none());
    }
}

fn load_file_config() -> FileConfig {
    let Some(path) = config_path() else {
        return FileConfig::default();
    };
    let Ok(content) = std::fs::read_to_string(&path) else {
        return FileConfig::default();
    };
    toml::from_str(&content).unwrap_or_default()
}

impl Config {
    pub fn load() -> Result<Self, AppError> {
        let cli = Cli::parse();
        let file = load_file_config();

        let github_token = cli
            .github_token
            .or(file.github_token)
            .ok_or_else(|| {
                AppError::Config(
                    "GitHub token not found. Set via --github-token, PRTOP_GITHUB_TOKEN env, or config file.".to_string(),
                )
            })?;

        let username = cli
            .username
            .or(file.username)
            .ok_or_else(|| {
                AppError::Config(
                    "Username not found. Set via --username, PRTOP_GITHUB_USERNAME env, or config file.".to_string(),
                )
            })?;

        let poll_interval_secs =
            resolve_poll_interval(cli.poll_interval_secs, file.poll_interval_secs);

        let notify_file = file.notify.unwrap_or_default();
        let notify_enabled = notify_file.enabled.unwrap_or(false);
        let notify_events = resolve_notify_events(notify_file.events);

        let fc = file.colors.unwrap_or_default();
        let d = ColorScheme::default();
        let color_scheme = ColorScheme {
            app_title: fc
                .app_title
                .as_deref()
                .and_then(parse_color)
                .unwrap_or(d.app_title),
            col_header: fc
                .col_header
                .as_deref()
                .and_then(parse_color)
                .unwrap_or(d.col_header),
            role: fc.role.as_deref().and_then(parse_color).unwrap_or(d.role),
            number: fc
                .number
                .as_deref()
                .and_then(parse_color)
                .unwrap_or(d.number),
            repo: fc.repo.as_deref().and_then(parse_color).unwrap_or(d.repo),
            new_pr: fc
                .new_pr
                .as_deref()
                .and_then(parse_color)
                .unwrap_or(d.new_pr),
            new_comment: fc
                .new_comment
                .as_deref()
                .and_then(parse_color)
                .unwrap_or(d.new_comment),
            draft: fc.draft.as_deref().and_then(parse_color).unwrap_or(d.draft),
            footer_count: fc
                .footer_count
                .as_deref()
                .and_then(parse_color)
                .unwrap_or(d.footer_count),
        };

        Ok(Config {
            github_token,
            username,
            poll_interval_secs,
            notify_enabled,
            notify_events,
            color_scheme,
        })
    }
}
