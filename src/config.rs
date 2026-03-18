use clap::Parser;
use serde::Deserialize;
use std::path::PathBuf;

use crate::colors::{ColorScheme, parse_color};
use crate::error::AppError;

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
    pub color_scheme: ColorScheme,
}

pub(crate) fn resolve_poll_interval(cli: Option<u64>, file: Option<u64>) -> u64 {
    cli.or(file).unwrap_or(60)
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

        let notify_enabled = file.notify.unwrap_or_default().enabled.unwrap_or(false);

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
            color_scheme,
        })
    }
}
