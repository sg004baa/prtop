use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, List, ListItem, Paragraph};

use crate::app::{App, LoadingState, Screen};
use crate::types::{PrRole, PrState};

pub fn view(f: &mut Frame, app: &mut App) {
    match app.screen {
        Screen::PrList => render_pr_list(f, app),
        Screen::Help => render_help(f, app),
    }
}

/// Returns (role, status, number, repo, title) column widths based on actual PR data lengths.
/// Layout: [▸ ][role][  ][status][  ][number][title][  ][repo]
fn col_widths(term_width: u16, app: &App) -> (usize, usize, usize, usize, usize) {
    let effective = (term_width as usize).saturating_sub(2); // 2 for "▸ " / "  "
    let role: usize = 6; // "AUTHOR", "REVIEW", "BOTH  "
    let status: usize = 6; // "OPEN  ", "CLOSED", "MERGED"
    let number: usize = 7; // "#12345 "
    let seps: usize = 6; // "  " after role, after status, after title
    let fixed = role + status + seps + number;
    let remaining = effective.saturating_sub(fixed);

    let max_repo = app
        .prs
        .keys()
        .map(|id| id.owner.len() + 1 + id.repo.len())
        .max()
        .unwrap_or(15);
    let max_title = app
        .prs
        .values()
        .map(|pr| pr.title.chars().count())
        .max()
        .unwrap_or(20);

    let (repo, title) = if max_repo + max_title <= remaining {
        (max_repo, max_title)
    } else {
        let repo = max_repo.min(remaining / 3).max(10);
        let title = remaining.saturating_sub(repo);
        (repo, title)
    };
    (role, status, number, repo, title)
}

fn pr_list_area(f: &Frame, app: &App) -> Rect {
    // fixed rows: app_header(1) + col_header(1) + blank(1) + footer(1) = 4
    let max_list_lines = f.area().height.saturating_sub(4).max(1);
    let list_lines = match &app.loading {
        LoadingState::Loaded => (app.prs.len().max(1) as u16).min(max_list_lines),
        _ => 1,
    };
    let height = (list_lines + 4).min(f.area().height);
    Rect::new(0, 0, f.area().width, height)
}

fn render_pr_list(f: &mut Frame, app: &mut App) {
    let area = pr_list_area(f, app);
    let list_lines = area.height.saturating_sub(4).max(1); // subtract fixed rows

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),          // app header
            Constraint::Length(1),          // column header
            Constraint::Length(list_lines), // pr list
            Constraint::Length(1),          // blank separator
            Constraint::Length(1),          // footer
        ])
        .split(area);

    let widths = col_widths(area.width, app);
    render_header(f, app, chunks[0]);
    render_col_header(f, app, chunks[1], widths);
    render_list(f, app, chunks[2], widths);
    render_footer(f, app, chunks[4]);
}

fn render_header(f: &mut Frame, app: &App, area: Rect) {
    let last_poll = app
        .last_poll
        .map(|t| {
            t.with_timezone(&chrono::Local)
                .format("%H:%M:%S")
                .to_string()
        })
        .unwrap_or_else(|| "---".to_string());

    let error_indicator = if app.poll_error.is_some() { " [!]" } else { "" };

    let header = Line::from(vec![
        Span::styled(
            " GitHub PR Live",
            Style::default()
                .fg(app.colors.app_title)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(
            format!("Last poll: {last_poll}{error_indicator}"),
            Style::default().fg(Color::DarkGray),
        ),
    ]);
    f.render_widget(Paragraph::new(header), area);
}

fn render_col_header(
    f: &mut Frame,
    app: &App,
    area: Rect,
    widths: (usize, usize, usize, usize, usize),
) {
    let (role_w, status_w, num_w, repo_w, title_w) = widths;
    let style = Style::default()
        .fg(app.colors.col_header)
        .add_modifier(Modifier::BOLD);
    let header = Line::from(vec![
        Span::raw("  "), // indent to match list highlight symbol ("▸ " / "  ")
        Span::styled(format!("{:<width$}", "ROLE", width = role_w), style),
        Span::raw("  "),
        Span::styled(format!("{:<width$}", "STATUS", width = status_w), style),
        Span::raw("  "),
        Span::styled(format!("{:<width$}", "#", width = num_w), style),
        Span::styled(format!("{:<width$}", "TITLE", width = title_w), style),
        Span::raw("  "),
        Span::styled(format!("{:<width$}", "REPO", width = repo_w), style),
    ]);
    f.render_widget(Paragraph::new(header), area);
}

fn render_list(
    f: &mut Frame,
    app: &mut App,
    area: Rect,
    widths: (usize, usize, usize, usize, usize),
) {
    match &app.loading {
        LoadingState::Initial | LoadingState::Loading => {
            let loading = Paragraph::new("  Loading...").style(Style::default().fg(Color::Yellow));
            f.render_widget(loading, area);
            return;
        }
        LoadingState::Error(msg) => {
            let error =
                Paragraph::new(format!("  Error: {msg}")).style(Style::default().fg(Color::Red));
            f.render_widget(error, area);
            return;
        }
        LoadingState::Loaded => {}
    }

    if app.prs.is_empty() {
        let empty = Paragraph::new("  No PRs found").style(Style::default().fg(Color::DarkGray));
        f.render_widget(empty, area);
        return;
    }

    let (role_w, status_w, num_w, repo_w, title_w) = widths;

    let items: Vec<ListItem> = app
        .prs
        .iter()
        .map(|(id, pr)| {
            let is_new = app.new_pr_ids.contains(id);

            let role_tag = match pr.role {
                PrRole::Author => "AUTHOR",
                PrRole::ReviewRequested => "REVIEW",
                PrRole::Both => "BOTH",
            };

            let (status_tag, status_style) = match pr.state {
                PrState::Open => ("OPEN", Style::default().fg(Color::Green)),
                PrState::Closed => ("CLOSED", Style::default().fg(Color::Yellow)),
                PrState::Merged => ("MERGED", Style::default().fg(Color::Magenta)),
            };

            let repo_display = format!("{}/{}", id.owner, id.repo);
            let number_display = format!("#{}", id.number);

            let title_style = if is_new {
                Style::default().fg(app.colors.new_pr)
            } else if pr.is_draft {
                Style::default().fg(app.colors.draft)
            } else {
                Style::default()
            };

            let line = Line::from(vec![
                Span::styled(
                    format!("{:<width$}", role_tag, width = role_w),
                    Style::default().fg(app.colors.role),
                ),
                Span::raw("  "),
                Span::styled(
                    format!("{:<width$}", status_tag, width = status_w),
                    status_style,
                ),
                Span::raw("  "),
                Span::styled(
                    format!("{:<width$}", number_display, width = num_w),
                    Style::default().fg(app.colors.number),
                ),
                Span::styled(
                    format!("{:<width$}", truncate(&pr.title, title_w), width = title_w),
                    title_style,
                ),
                Span::raw("  "),
                Span::styled(
                    format!(
                        "{:<width$}",
                        truncate(&repo_display, repo_w),
                        width = repo_w
                    ),
                    Style::default().fg(app.colors.repo),
                ),
            ]);

            ListItem::new(line)
        })
        .collect();

    let list = List::new(items)
        .block(Block::default())
        .highlight_style(Style::default().add_modifier(Modifier::BOLD))
        .highlight_symbol("▸ ");

    f.render_stateful_widget(list, area, &mut app.list_state);
}

fn render_footer(f: &mut Frame, app: &App, area: Rect) {
    let pr_count = app.prs.len();

    let status = if let Some(ref msg) = app.status_message {
        msg.clone()
    } else if let Some(ref err) = app.poll_error {
        format!("Error: {err}")
    } else {
        String::new()
    };

    let footer = Line::from(vec![
        Span::styled(
            format!(" {pr_count} PRs"),
            Style::default().fg(app.colors.footer_count),
        ),
        Span::raw(" │ "),
        Span::styled("?: help", Style::default().fg(Color::DarkGray)),
        Span::raw("  "),
        Span::styled(status, Style::default().fg(Color::Yellow)),
    ]);
    f.render_widget(Paragraph::new(footer), area);
}

fn render_help(f: &mut Frame, _app: &mut App) {
    let help_text = vec![
        Line::from(""),
        Line::from(Span::styled(
            " GitHub PR Live - Help",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from("  q / Ctrl+C    Quit"),
        Line::from("  j / ↓         Move down"),
        Line::from("  k / ↑         Move up"),
        Line::from("  Enter / o     Open PR in browser"),
        Line::from("  ?             Toggle help"),
        Line::from("  r             Force refresh"),
        Line::from(""),
        Line::from(Span::styled(
            "  Press any key to return",
            Style::default().fg(Color::DarkGray),
        )),
    ];

    let help = Paragraph::new(help_text).block(Block::default().title(" Help "));
    f.render_widget(help, f.area());
}

fn truncate(s: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let truncated: String = s.chars().take(max - 1).collect();
        format!("{truncated}…")
    }
}
