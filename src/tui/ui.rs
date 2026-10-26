use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, HighlightSpacing, List, ListItem, Paragraph, Wrap};

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::app::{App, LoadingState, Screen, VisibleRow};
use crate::types::{CiStatus, PrRole, PrState};

pub fn view(f: &mut Frame, app: &mut App) {
    match app.screen {
        Screen::PrList => render_pr_list(f, app),
        Screen::Help => render_help(f, app),
    }
}

/// Returns (status, ci, review, number, repo, title) column widths based on actual PR data lengths.
/// Child layout: [▸ ][  ][status][  ][ci][  ][number][title][  ][repo]
fn col_widths(term_width: u16, app: &App) -> (usize, usize, usize, usize, usize, usize) {
    let effective = (term_width as usize).saturating_sub(4); // highlight + tree indent
    let status: usize = 6; // "OPEN  ", "CLOSED", "MERGED"
    let ci: usize = 3; // "✓  ", "×  ", "...", "-  "
    let number: usize = 7; // "#12345 "
    let review = app
        .prs
        .keys()
        .map(|id| review_display(app, id, term_width < 100).0.width())
        .max()
        .unwrap_or(6)
        .max(6);
    let seps: usize = 8;
    let fixed = status + ci + review + seps + number;
    let remaining = effective.saturating_sub(fixed);

    let max_repo = app.prs.keys().map(|id| id.repo.width()).max().unwrap_or(15);
    let max_title = app
        .prs
        .values()
        .map(|pr| pr.title.width())
        .max()
        .unwrap_or(20);

    let (repo, title) = if max_repo + max_title <= remaining {
        (max_repo, max_title)
    } else {
        let repo = max_repo.min(remaining / 3).max(10);
        let title = remaining.saturating_sub(repo);
        (repo, title)
    };
    (status, ci, review, number, repo, title)
}

fn pr_list_area(f: &Frame, app: &App) -> Rect {
    // fixed rows: app_header(1) + col_header(1) + blank(1) + footer(1) = 4
    let fixed = 4 + error_banner_height(app, f.area());
    let max_list_lines = f.area().height.saturating_sub(fixed).max(1);
    let list_lines = match &app.loading {
        LoadingState::Loaded => (app.visible_rows.len().max(1) as u16).min(max_list_lines),
        _ => 1,
    };
    let height = (list_lines + fixed).min(f.area().height);
    Rect::new(0, 0, f.area().width, height)
}

fn render_pr_list(f: &mut Frame, app: &mut App) {
    let area = pr_list_area(f, app);
    let banner_height = error_banner_height(app, f.area());
    let list_lines = area.height.saturating_sub(4 + banner_height).max(1);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),             // app header
            Constraint::Length(1),             // column header
            Constraint::Length(list_lines),    // pr list
            Constraint::Length(1),             // blank separator
            Constraint::Length(1),             // footer
            Constraint::Length(banner_height), // multiline API warning below key hints
        ])
        .split(area);

    let widths = col_widths(area.width, app);
    render_header(f, app, chunks[0]);
    render_col_header(f, app, chunks[1], widths);
    render_list(f, app, chunks[2], widths);
    render_footer(f, app, chunks[4]);
    render_api_warning(f, app, chunks[5]);
}

fn error_banner_height(app: &App, area: Rect) -> u16 {
    warning_lines(app, area.width)
        .len()
        .min(area.height.saturating_sub(5) as usize) as u16
}

fn warning_lines(app: &App, width: u16) -> Vec<Line<'static>> {
    let Some(error) = &app.last_api_error else {
        return Vec::new();
    };
    if width == 0 {
        return Vec::new();
    }
    let title = if app.poll_error.is_some() {
        "API warning - ?: details"
    } else {
        "Last API warning - ?: details (may have recovered)"
    };
    let mut lines = Vec::new();
    // Preserve explicit newlines and indentation, wrapping long lines to the
    // terminal width so the reserved height matches the rendered text.
    for (index, source) in std::iter::once(title).chain(error.split('\n')).enumerate() {
        let style = if index == 0 {
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::Yellow)
        };
        let mut line = String::new();
        let mut used = 0;
        for ch in source.trim_end_matches('\r').chars() {
            let size = ch.width().unwrap_or(0);
            if used + size > width as usize && !line.is_empty() {
                lines.push(Line::from(Span::styled(std::mem::take(&mut line), style)));
                used = 0;
            }
            line.push(ch);
            used += size;
        }
        lines.push(Line::from(Span::styled(line, style)));
    }
    lines
}

fn render_api_warning(f: &mut Frame, app: &App, area: Rect) {
    f.render_widget(Paragraph::new(warning_lines(app, area.width)), area);
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

    let error_indicator = if app.last_api_error.is_some() {
        " [!]"
    } else {
        ""
    };

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
    widths: (usize, usize, usize, usize, usize, usize),
) {
    let (status_w, ci_w, review_w, num_w, repo_w, title_w) = widths;
    let style = Style::default()
        .fg(app.colors.col_header)
        .add_modifier(Modifier::BOLD);
    let header = Line::from(vec![
        Span::raw("    "), // list highlight symbol + PR tree indent
        Span::styled(format!("{:<width$}", "STATUS", width = status_w), style),
        Span::raw("  "),
        Span::styled(format!("{:<width$}", "CI", width = ci_w), style),
        Span::raw("  "),
        Span::styled(format!("{:<width$}", "REVIEW", width = review_w), style),
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
    widths: (usize, usize, usize, usize, usize, usize),
) {
    match &app.loading {
        LoadingState::Initial | LoadingState::Loading => {
            let loading = Paragraph::new("  Loading...").style(Style::default().fg(Color::Yellow));
            f.render_widget(loading, area);
            return;
        }
        LoadingState::Error(_msg) => {
            let error = Paragraph::new("  Unable to load PRs. See error below.")
                .style(Style::default().fg(Color::Red));
            f.render_widget(error, area);
            return;
        }
        LoadingState::Loaded => {}
    }

    let (status_w, ci_w, review_w, num_w, repo_w, title_w) = widths;
    let item_count = app.visible_rows.len();
    if area.is_empty() {
        return;
    }
    if item_count == 0 {
        app.list_state.select(None);
        return;
    }

    let selected = match app.list_state.selected() {
        Some(index) if index >= item_count => {
            app.list_state.select(Some(item_count - 1));
            Some(item_count - 1)
        }
        selected => selected,
    };
    let height = area.height as usize;
    let mut first = app.list_state.offset().min(item_count - 1);
    if let Some(selected) = selected {
        if selected < first {
            first = selected;
        } else if selected >= first.saturating_add(height) {
            first = (selected + 1).saturating_sub(height);
        }
    }

    app.list_state.select(selected);
    *app.list_state.offset_mut() = first;
    let mut local_state = ratatui::widgets::ListState::default();
    local_state.select(selected.map(|index| index - first));

    let items: Vec<ListItem> = app
        .visible_rows
        .iter()
        .skip(first)
        .take(height)
        .map(|row| match row {
            VisibleRow::Role(role) => {
                let marker = if app.role_is_collapsed(*role) {
                    "▶"
                } else {
                    "▼"
                };
                ListItem::new(Line::from(vec![Span::styled(
                    format!("{marker} {}", role_label(*role)),
                    Style::default()
                        .fg(app.colors.role)
                        .add_modifier(Modifier::BOLD),
                )]))
            }
            VisibleRow::Pr(id) => {
                let pr = app
                    .prs
                    .get(id)
                    .expect("visible PR row must reference cached PR");
                let is_new = app.new_pr_ids.contains(id);
                let (status_tag, status_style) = match pr.state {
                    PrState::Open => ("OPEN", Style::default().fg(Color::Green)),
                    PrState::Closed => ("CLOSED", Style::default().fg(Color::Yellow)),
                    PrState::Merged => ("MERGED", Style::default().fg(Color::Magenta)),
                };
                let number_display = format!("#{}", id.number);
                let has_new_comment = app.new_comment_pr_ids.contains(id);
                let title_style = if is_new {
                    Style::default().fg(app.colors.new_pr)
                } else if has_new_comment {
                    Style::default().fg(app.colors.new_comment)
                } else if pr.is_draft {
                    Style::default().fg(app.colors.draft)
                } else {
                    Style::default()
                };
                let (ci_tag, ci_style) = ci_cell(pr.ci_status.as_ref());
                let (review_tag, review_style) = review_display(app, id, area.width < 100);
                ListItem::new(Line::from(vec![
                    Span::raw("  "),
                    Span::styled(
                        format!("{:<width$}", status_tag, width = status_w),
                        status_style,
                    ),
                    Span::raw("  "),
                    Span::styled(format!("{:<width$}", ci_tag, width = ci_w), ci_style),
                    Span::raw("  "),
                    Span::styled(pad(&review_tag, review_w), review_style),
                    Span::raw("  "),
                    Span::styled(
                        format!("{:<width$}", number_display, width = num_w),
                        Style::default().fg(app.colors.number),
                    ),
                    Span::styled(pad(&truncate(&pr.title, title_w), title_w), title_style),
                    Span::raw("  "),
                    Span::styled(
                        pad(&truncate(&id.repo, repo_w), repo_w),
                        Style::default().fg(app.colors.repo),
                    ),
                ]))
            }
        })
        .collect();

    let list = List::new(items)
        .block(Block::default())
        .highlight_style(Style::default().add_modifier(Modifier::BOLD))
        .highlight_symbol("▸ ")
        .highlight_spacing(HighlightSpacing::Always);

    f.render_stateful_widget(list, area, &mut local_state);
}

fn render_footer(f: &mut Frame, app: &App, area: Rect) {
    let pr_count = app.prs.len();

    let status = app.status_message.clone().unwrap_or_default();

    let footer = Line::from(vec![
        Span::styled(
            format!(" {pr_count} PRs"),
            Style::default().fg(app.colors.footer_count),
        ),
        Span::raw(" │ "),
        Span::styled(
            "Enter: toggle/open  o: open  ?: help",
            Style::default().fg(Color::DarkGray),
        ),
        Span::raw("  "),
        Span::styled(status, Style::default().fg(Color::Yellow)),
    ]);
    f.render_widget(Paragraph::new(footer), area);
}

fn render_help(f: &mut Frame, app: &mut App) {
    let mut help_text = vec![
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
        Line::from("  Enter         Toggle role / open PR"),
        Line::from("  o             Open selected PR in browser"),
        Line::from("  ?             Toggle help"),
        Line::from("  r             Force refresh"),
        Line::from(""),
        Line::from(Span::styled(
            "  Press any key to return",
            Style::default().fg(Color::DarkGray),
        )),
    ];

    if let Some(error) = &app.last_api_error {
        help_text.push(Line::from(""));
        help_text.push(Line::styled(
            " Last API error (may have recovered)",
            Style::default().fg(Color::Yellow),
        ));
        help_text.extend(error.split("; ").map(Line::from));
    }
    let help = Paragraph::new(help_text)
        .wrap(Wrap { trim: false })
        .block(Block::default().title(" Help "));
    f.render_widget(help, f.area());
}

fn role_label(role: PrRole) -> &'static str {
    match role {
        PrRole::Author => "AUTHOR",
        PrRole::ReviewRequested => "REVIEW",
        PrRole::Mentioned => "MENTION",
    }
}

fn review_display(app: &App, id: &crate::types::PrId, compact: bool) -> (String, Style) {
    let (mut text, mut style) = review_cell(app.review_summaries.get(id), compact);
    if app.stale_reviews.contains(id) && app.review_summaries.contains_key(id) {
        text.push_str(if compact { " ~" } else { " (stale)" });
        style = Style::default().fg(Color::DarkGray);
    }
    (text, style)
}

fn review_cell(summary: Option<&crate::review::ReviewSummary>, compact: bool) -> (String, Style) {
    let Some(summary) = summary else {
        return ("?".into(), Style::default().fg(Color::DarkGray));
    };
    if summary.total == 0 && summary.pending_teams == 0 {
        return ("—".into(), Style::default().fg(Color::DarkGray));
    }
    let prefix = if compact { "" } else { "approved " };
    let mut text = format!("{prefix}{}/{}", summary.approved, summary.total);
    if summary.changes_requested {
        text.push_str(" !");
    }
    if summary.pending_teams > 0 {
        text.push_str(if compact { " +team" } else { " + team pending" });
    }
    let color = if summary.changes_requested {
        Color::Red
    } else if summary.all_approved() {
        Color::Green
    } else {
        Color::Yellow
    };
    (text, Style::default().fg(color))
}

fn ci_cell(ci: Option<&CiStatus>) -> (&'static str, Style) {
    match ci {
        Some(CiStatus::Pending) => ("...", Style::default().fg(Color::Yellow)),
        Some(CiStatus::Success) => ("✓", Style::default().fg(Color::Green)),
        Some(CiStatus::Failure) => ("✗", Style::default().fg(Color::Red)),
        None => ("-", Style::default().fg(Color::DarkGray)),
    }
}

/// `format!("{:<w$}")` は文字数でパディングするため、全角文字（表示幅2）が
/// 混ざると列がずれる。表示幅ベースでパディング・切り詰めする。
fn pad(s: &str, width: usize) -> String {
    let mut out = s.to_string();
    out.extend(std::iter::repeat_n(' ', width.saturating_sub(s.width())));
    out
}

fn truncate(s: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    if s.width() <= max {
        return s.to_string();
    }
    let mut out = String::new();
    let mut w = 0;
    for ch in s.chars() {
        let cw = ch.width().unwrap_or(0);
        if w + cw > max - 1 {
            break;
        }
        out.push(ch);
        w += cw;
    }
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_ascii_within_limit_unchanged() {
        assert_eq!(truncate("hello", 10), "hello");
        assert_eq!(truncate("hello", 5), "hello");
    }

    #[test]
    fn truncate_ascii_over_limit_adds_ellipsis() {
        assert_eq!(truncate("hello world", 6), "hello…");
    }

    #[test]
    fn truncate_zero_returns_empty() {
        assert_eq!(truncate("hello", 0), "");
    }

    #[test]
    fn truncate_wide_chars_respects_display_width() {
        // "日本語" = width 6, fits exactly
        assert_eq!(truncate("日本語", 6), "日本語");
        // width 14 → cut to ≤ max, ellipsis included
        let cut = truncate("日本語タイトル", 8);
        assert_eq!(cut, "日本語…");
        assert!(cut.width() <= 8);
    }

    #[test]
    fn truncate_never_exceeds_max_width() {
        for max in 1..10 {
            let cut = truncate("あiうeお", max);
            assert!(cut.width() <= max, "max={max} got width {}", cut.width());
        }
    }

    #[test]
    fn pad_uses_display_width() {
        // "日本語" は 3 文字だが表示幅 6 → パディングは 4 で計 10
        assert_eq!(pad("日本語", 10), "日本語    ");
        assert_eq!(pad("abc", 5), "abc  ");
        // already wider: no truncation, no panic
        assert_eq!(pad("abcdef", 3), "abcdef");
    }

    #[test]
    fn review_cells_distinguish_unknown_empty_partial_blocked_and_complete() {
        use crate::review::ReviewSummary;
        assert_eq!(review_cell(None, false).0, "?");
        assert_eq!(review_cell(Some(&ReviewSummary::default()), false).0, "—");
        let mut summary = ReviewSummary {
            approved: 1,
            total: 2,
            ..Default::default()
        };
        assert_eq!(review_cell(Some(&summary), false).0, "approved 1/2");
        assert_eq!(review_cell(Some(&summary), true).0, "1/2");
        summary.changes_requested = true;
        summary.pending_teams = 1;
        let (text, style) = review_cell(Some(&summary), false);
        assert_eq!(text, "approved 1/2 ! + team pending");
        assert_eq!(style.fg, Some(Color::Red));
        summary.changes_requested = false;
        summary.pending_teams = 0;
        summary.approved = 2;
        assert_eq!(review_cell(Some(&summary), false).1.fg, Some(Color::Green));
    }

    #[test]
    fn review_column_renders_on_wide_and_narrow_terminals() {
        use crate::app::{LoadingState, Message};
        use crate::poller::PollPayload;
        use crate::review::ReviewSummary;
        use crate::types::{PrId, PullRequest};
        use chrono::Utc;
        use indexmap::IndexMap;
        use ratatui::{Terminal, backend::TestBackend};
        use std::collections::{HashMap, HashSet};
        for width in [50, 80, 120] {
            let mut app = App::new(
                "me".into(),
                Default::default(),
                HashSet::new(),
                HashSet::new(),
            );
            let id = PrId {
                owner: "org".into(),
                repo: "repo".into(),
                number: 1,
            };
            let pr = PullRequest {
                id: id.clone(),
                title: "Review progress".into(),
                url: String::new(),
                author_login: "me".into(),
                role: PrRole::Author,
                state: PrState::Open,
                created_at: Utc::now(),
                updated_at: Utc::now(),
                is_draft: false,
                review_decision: None,
                total_comments: 0,
                last_commenter: None,
                ci_status: None,
            };
            app.update(Message::PollResult(PollPayload {
                generation: 1,
                prs: IndexMap::from([(id.clone(), pr)]),
                head_shas: HashMap::new(),
                polled_at: Utc::now(),
            }));
            app.loading = LoadingState::Loaded;
            app.review_summaries.insert(
                id.clone(),
                ReviewSummary {
                    approved: 1,
                    total: 2,
                    ..Default::default()
                },
            );
            let mut terminal = Terminal::new(TestBackend::new(width, 10)).unwrap();
            terminal.draw(|frame| view(frame, &mut app)).unwrap();
            let buffer = terminal.backend().buffer();
            let row = |y| {
                (0..width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            };
            assert!(row(1).contains("REVIEW"));
            assert!(row(3).contains(if width < 100 { "1/2" } else { "approved 1/2" }));
            assert!(row(3).contains("#1"));
            app.stale_reviews.insert(id.clone());
            let (label, style) = review_display(&app, &id, width < 100);
            assert!(label.contains(if width < 100 { "~" } else { "(stale)" }));
            assert_eq!(style.fg, Some(Color::DarkGray));
            assert!(label.contains("1/2"));
        }
    }

    #[test]
    fn list_keeps_warning_visible_after_snapshot_and_during_refresh() {
        use crate::app::Message;
        use crate::poller::PollPayload;
        use ratatui::{Terminal, backend::TestBackend};
        let mut app = App::new(
            "me".into(),
            Default::default(),
            Default::default(),
            Default::default(),
        );
        app.update(Message::PollError(
            "GitHub token #1: CI unavailable\nHTTP 403\n  permission denied".into(),
        ));
        for width in [40, 80] {
            let mut terminal = Terminal::new(TestBackend::new(width, 24)).unwrap();
            for recovered in [false, true] {
                if recovered {
                    app.update(Message::PollResult(PollPayload {
                        generation: 1,
                        prs: Default::default(),
                        head_shas: Default::default(),
                        polled_at: chrono::Utc::now(),
                    }));
                    assert!(matches!(app.loading, LoadingState::Loaded));
                    app.status_message = Some("Refreshing...".into());
                }
                terminal.draw(|f| view(f, &mut app)).unwrap();
                let buffer = terminal.backend().buffer();
                let lines: Vec<String> = (0..24)
                    .map(|y| (0..width).map(|x| buffer[(x, y)].symbol()).collect())
                    .collect();
                let text = lines.join("\n");
                assert!(text.contains("API warning"), "{text}");
                assert!(text.contains("?: details"), "{text}");
                assert!(text.contains("CI unavailable"), "{text}");
                let hint_row = lines.iter().position(|l| l.contains("Enter:")).unwrap();
                let warning_row = lines
                    .iter()
                    .position(|l| l.contains("API warning"))
                    .unwrap();
                assert!(warning_row > hint_row, "{text}");
                let http_row = lines.iter().position(|l| l.trim() == "HTTP 403").unwrap();
                assert_eq!(lines[http_row + 1].trim_end(), "  permission denied");
                assert!(http_row > warning_row);
            }
        }
    }

    #[test]
    fn help_preserves_readable_diagnostics_after_successful_poll() {
        use crate::app::Message;
        use crate::poller::PollPayload;
        use indexmap::IndexMap;
        use ratatui::{Terminal, backend::TestBackend};
        use std::collections::{HashMap, HashSet};
        let mut app = App::new(
            "me".into(),
            Default::default(),
            HashSet::new(),
            HashSet::new(),
        );
        app.update(Message::PollError("GitHub token #2: primary limit; HTTP 200; resource=graphql; remaining=0; reset=2026-10-23T15:00:00Z".into()));
        app.update(Message::PollResult(PollPayload {
            generation: 1,
            prs: IndexMap::new(),
            head_shas: HashMap::new(),
            polled_at: chrono::Utc::now(),
        }));
        assert!(app.poll_error.is_none());
        app.screen = Screen::Help;
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|f| view(f, &mut app)).unwrap();
        let buffer = terminal.backend().buffer();
        let text = (0..24)
            .flat_map(|y| (0..80).map(move |x| buffer[(x, y)].symbol()))
            .collect::<String>();
        for expected in [
            "token #2",
            "HTTP 200",
            "resource=graphql",
            "remaining=0",
            "reset=2026-10-23",
        ] {
            assert!(text.contains(expected), "{expected}");
        }
    }
}
