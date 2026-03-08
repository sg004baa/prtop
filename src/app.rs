use std::collections::HashSet;
use std::time::Instant;

use chrono::{DateTime, Utc};
use indexmap::IndexMap;
use ratatui::widgets::ListState;

use crate::colors::ColorScheme;
use crate::diff::diff_pr_sets;
use crate::notify::Notification;
use crate::poller::PollPayload;
use crate::types::{PrId, PrRole, PullRequest, ReviewDecision};

#[derive(Debug, PartialEq, Eq)]
pub enum Screen {
    PrList,
    Help,
}

#[derive(Debug)]
#[allow(dead_code)]
pub enum LoadingState {
    Initial,
    Loading,
    Loaded,
    Error(String),
}

pub enum Message {
    Quit,
    MoveUp,
    MoveDown,
    OpenSelected,
    ToggleHelp,
    Refresh,
    Deselect,
    PollResult(PollPayload),
    PollError(String),
}

pub struct App {
    pub prs: IndexMap<PrId, PullRequest>,
    pub list_state: ListState,
    pub screen: Screen,
    pub loading: LoadingState,
    pub last_poll: Option<DateTime<Utc>>,
    pub poll_error: Option<String>,
    pub new_pr_ids: HashSet<PrId>,
    pub should_quit: bool,
    pub status_message: Option<String>,
    pub dirty: bool,
    pub last_activity: Option<Instant>,
    pub pending_notifications: Vec<Notification>,
    pub colors: ColorScheme,
}

impl App {
    pub fn new(colors: ColorScheme) -> Self {
        Self {
            prs: IndexMap::new(),
            list_state: ListState::default(),
            screen: Screen::PrList,
            loading: LoadingState::Initial,
            last_poll: None,
            poll_error: None,
            new_pr_ids: HashSet::new(),
            should_quit: false,
            status_message: None,
            dirty: true,
            last_activity: None,
            pending_notifications: Vec::new(),
            colors,
        }
    }

    pub fn update(&mut self, msg: Message) {
        // Help 画面中はバックグラウンドのポール以外の全入力でhelpを閉じる
        if self.screen == Screen::Help {
            match &msg {
                Message::PollResult(_) | Message::PollError(_) => {}
                Message::Quit => {
                    self.should_quit = true;
                    self.dirty = true;
                    return;
                }
                _ => {
                    self.screen = Screen::PrList;
                    self.last_activity = Some(Instant::now());
                    self.dirty = true;
                    return;
                }
            }
        }

        match msg {
            Message::Quit => {
                self.should_quit = true;
                self.dirty = true;
            }
            Message::MoveUp => {
                if self.prs.is_empty() {
                    return;
                }
                let i = match self.list_state.selected() {
                    Some(i) => {
                        if i == 0 {
                            self.prs.len() - 1
                        } else {
                            i - 1
                        }
                    }
                    None => 0,
                };
                self.list_state.select(Some(i));
                if let Some((id, _)) = self.prs.get_index(i) {
                    self.new_pr_ids.remove(id);
                }
                self.last_activity = Some(Instant::now());
                self.dirty = true;
            }
            Message::MoveDown => {
                if self.prs.is_empty() {
                    return;
                }
                let i = match self.list_state.selected() {
                    Some(i) => {
                        if i >= self.prs.len() - 1 {
                            0
                        } else {
                            i + 1
                        }
                    }
                    None => 0,
                };
                self.list_state.select(Some(i));
                if let Some((id, _)) = self.prs.get_index(i) {
                    self.new_pr_ids.remove(id);
                }
                self.last_activity = Some(Instant::now());
                self.dirty = true;
            }
            Message::OpenSelected => {
                if let Some(i) = self.list_state.selected() {
                    if let Some((_, pr)) = self.prs.get_index(i) {
                        let url = pr.url.clone();
                        if open::that(&url).is_err() {
                            self.status_message =
                                Some(format!("Failed to open browser: {url}"));
                            self.dirty = true;
                        }
                    }
                }
                self.last_activity = Some(Instant::now());
            }
            Message::ToggleHelp => {
                self.screen = match self.screen {
                    Screen::PrList => Screen::Help,
                    Screen::Help => Screen::PrList,
                };
                self.last_activity = Some(Instant::now());
                self.dirty = true;
            }
            Message::Refresh => {
                self.status_message = Some("Refreshing...".to_string());
                self.last_activity = Some(Instant::now());
                self.dirty = true;
            }
            Message::Deselect => {
                if self.list_state.selected().is_some() {
                    self.list_state.select(None);
                    self.dirty = true;
                }
            }
            Message::PollResult(payload) => {
                let diff = diff_pr_sets(&self.prs, &payload.prs);
                let already_loaded = matches!(self.loading, LoadingState::Loaded);

                if already_loaded {
                    // PR closed/merged: only notify when we were the author
                    for id in &diff.removed {
                        if let Some(pr) = self.prs.get(id) {
                            if pr.role == PrRole::Author {
                                self.pending_notifications.push(Notification {
                                    title: "PR closed/merged".to_string(),
                                    body: format!("{} ({})", pr.title, id),
                                });
                            }
                        }
                    }

                    // New PR added: only notify when we are NOT the author (review request)
                    for id in &diff.added {
                        if let Some(pr) = payload.prs.get(id) {
                            if pr.role != PrRole::Author {
                                self.pending_notifications.push(Notification {
                                    title: "Review requested".to_string(),
                                    body: format!("{} ({})", pr.title, id),
                                });
                            }
                        }
                    }

                    // Updated PR: notify when review_decision changed to ReviewRequired
                    for id in &diff.updated {
                        let old_decision = self.prs.get(id).and_then(|p| p.review_decision.as_ref());
                        let new_decision = payload.prs.get(id).and_then(|p| p.review_decision.as_ref());
                        let became_review_required =
                            matches!(new_decision, Some(ReviewDecision::ReviewRequired))
                            && !matches!(old_decision, Some(ReviewDecision::ReviewRequired));
                        if became_review_required {
                            if let Some(pr) = payload.prs.get(id) {
                                self.pending_notifications.push(Notification {
                                    title: "Re-review requested".to_string(),
                                    body: format!("{} ({})", pr.title, id),
                                });
                            }
                        }
                    }
                }

                self.new_pr_ids = diff.added.into_iter().collect();
                self.prs = payload.prs;
                self.last_poll = Some(payload.polled_at);
                self.poll_error = None;
                self.status_message = None;

                if matches!(self.loading, LoadingState::Initial | LoadingState::Loading) {
                    self.loading = LoadingState::Loaded;
                }
                self.dirty = true;
            }
            Message::PollError(msg) => {
                self.poll_error = Some(msg.clone());
                if matches!(self.loading, LoadingState::Initial | LoadingState::Loading) {
                    self.loading = LoadingState::Error(msg);
                }
                self.dirty = true;
            }
        }
    }

    #[allow(dead_code)]
    pub fn selected_pr(&self) -> Option<&PullRequest> {
        self.list_state
            .selected()
            .and_then(|i| self.prs.get_index(i).map(|(_, pr)| pr))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{PrRole, PrState, ReviewDecision};
    use chrono::Utc;

    fn make_id(number: u64) -> PrId {
        PrId {
            owner: "org".to_string(),
            repo: "repo".to_string(),
            number,
        }
    }

    fn make_pr_custom(
        id: &PrId,
        role: PrRole,
        review_decision: Option<ReviewDecision>,
        updated_secs: i64,
    ) -> PullRequest {
        let base: chrono::DateTime<Utc> = "2024-01-01T00:00:00Z".parse().unwrap();
        PullRequest {
            id: id.clone(),
            title: format!("PR #{}", id.number),
            url: format!("https://github.com/org/repo/pull/{}", id.number),
            author_login: "user".to_string(),
            role,
            state: PrState::Open,
            created_at: base,
            updated_at: base + chrono::Duration::seconds(updated_secs),
            is_draft: false,
            review_decision,
        }
    }

    fn payload_from(prs: IndexMap<PrId, PullRequest>) -> PollPayload {
        PollPayload {
            prs,
            polled_at: Utc::now(),
        }
    }

    fn make_payload(count: usize) -> PollPayload {
        let mut prs = IndexMap::new();
        for i in 0..count {
            let id = make_id(i as u64);
            let pr = make_pr_custom(&id, PrRole::Author, None, 0);
            prs.insert(id, pr);
        }
        payload_from(prs)
    }

    #[test]
    fn quit_sets_flag() {
        let mut app = App::new(ColorScheme::default());
        app.update(Message::Quit);
        assert!(app.should_quit);
    }

    #[test]
    fn poll_result_updates_prs() {
        let mut app = App::new(ColorScheme::default());
        app.update(Message::PollResult(make_payload(3)));
        assert_eq!(app.prs.len(), 3);
        assert!(matches!(app.loading, LoadingState::Loaded));
        assert_eq!(app.list_state.selected(), None);
    }

    #[test]
    fn navigation_wraps() {
        let mut app = App::new(ColorScheme::default());
        app.update(Message::PollResult(make_payload(3)));
        assert_eq!(app.list_state.selected(), None);

        // MoveUp from None → selects 0
        app.update(Message::MoveUp);
        assert_eq!(app.list_state.selected(), Some(0));

        // MoveUp from 0 → wraps to last (2)
        app.update(Message::MoveUp);
        assert_eq!(app.list_state.selected(), Some(2));

        // MoveDown from 2 → wraps to 0
        app.update(Message::MoveDown);
        assert_eq!(app.list_state.selected(), Some(0));
    }

    #[test]
    fn toggle_help() {
        let mut app = App::new(ColorScheme::default());
        assert_eq!(app.screen, Screen::PrList);
        app.update(Message::ToggleHelp);
        assert_eq!(app.screen, Screen::Help);
        app.update(Message::ToggleHelp);
        assert_eq!(app.screen, Screen::PrList);
    }

    #[test]
    fn poll_error_sets_state() {
        let mut app = App::new(ColorScheme::default());
        app.update(Message::PollError("network error".to_string()));
        assert!(app.poll_error.is_some());
        assert!(matches!(app.loading, LoadingState::Error(_)));
    }

    // --- Notification logic ---

    #[test]
    fn no_notifications_on_first_poll() {
        let mut app = App::new(ColorScheme::default());
        let id = make_id(1);
        let mut prs = IndexMap::new();
        prs.insert(id.clone(), make_pr_custom(&id, PrRole::ReviewRequested, None, 0));
        app.update(Message::PollResult(payload_from(prs)));
        assert!(app.pending_notifications.is_empty());
    }

    #[test]
    fn removed_author_pr_triggers_notification() {
        let mut app = App::new(ColorScheme::default());
        let id = make_id(1);
        let mut prs = IndexMap::new();
        prs.insert(id.clone(), make_pr_custom(&id, PrRole::Author, None, 0));
        app.update(Message::PollResult(payload_from(prs)));
        app.update(Message::PollResult(payload_from(IndexMap::new())));
        assert_eq!(app.pending_notifications.len(), 1);
        assert_eq!(app.pending_notifications[0].title, "PR closed/merged");
    }

    #[test]
    fn removed_reviewer_pr_no_notification() {
        let mut app = App::new(ColorScheme::default());
        let id = make_id(1);
        let mut prs = IndexMap::new();
        prs.insert(
            id.clone(),
            make_pr_custom(&id, PrRole::ReviewRequested, None, 0),
        );
        app.update(Message::PollResult(payload_from(prs)));
        app.update(Message::PollResult(payload_from(IndexMap::new())));
        assert!(app.pending_notifications.is_empty());
    }

    #[test]
    fn added_reviewer_pr_triggers_notification() {
        let mut app = App::new(ColorScheme::default());
        app.update(Message::PollResult(payload_from(IndexMap::new())));
        let id = make_id(1);
        let mut prs = IndexMap::new();
        prs.insert(
            id.clone(),
            make_pr_custom(&id, PrRole::ReviewRequested, None, 0),
        );
        app.update(Message::PollResult(payload_from(prs)));
        assert_eq!(app.pending_notifications.len(), 1);
        assert_eq!(app.pending_notifications[0].title, "Review requested");
    }

    #[test]
    fn added_author_pr_no_notification() {
        let mut app = App::new(ColorScheme::default());
        app.update(Message::PollResult(payload_from(IndexMap::new())));
        let id = make_id(1);
        let mut prs = IndexMap::new();
        prs.insert(id.clone(), make_pr_custom(&id, PrRole::Author, None, 0));
        app.update(Message::PollResult(payload_from(prs)));
        assert!(app.pending_notifications.is_empty());
    }

    #[test]
    fn review_required_transition_triggers_notification() {
        let mut app = App::new(ColorScheme::default());
        let id = make_id(1);
        let mut prs = IndexMap::new();
        prs.insert(
            id.clone(),
            make_pr_custom(&id, PrRole::ReviewRequested, Some(ReviewDecision::Approved), 0),
        );
        app.update(Message::PollResult(payload_from(prs)));

        // Second poll: review_decision changes to ReviewRequired (updated_at bumped)
        let mut prs2 = IndexMap::new();
        prs2.insert(
            id.clone(),
            make_pr_custom(
                &id,
                PrRole::ReviewRequested,
                Some(ReviewDecision::ReviewRequired),
                100,
            ),
        );
        app.update(Message::PollResult(payload_from(prs2)));

        assert_eq!(app.pending_notifications.len(), 1);
        assert_eq!(app.pending_notifications[0].title, "Re-review requested");
    }

    #[test]
    fn already_review_required_no_notification() {
        let mut app = App::new(ColorScheme::default());
        let id = make_id(1);
        let mut prs = IndexMap::new();
        prs.insert(
            id.clone(),
            make_pr_custom(
                &id,
                PrRole::ReviewRequested,
                Some(ReviewDecision::ReviewRequired),
                0,
            ),
        );
        app.update(Message::PollResult(payload_from(prs)));

        // Second poll: still ReviewRequired
        let mut prs2 = IndexMap::new();
        prs2.insert(
            id.clone(),
            make_pr_custom(
                &id,
                PrRole::ReviewRequested,
                Some(ReviewDecision::ReviewRequired),
                100,
            ),
        );
        app.update(Message::PollResult(payload_from(prs2)));

        assert!(app.pending_notifications.is_empty());
    }

    #[test]
    fn new_prs_detected() {
        let mut app = App::new(ColorScheme::default());
        app.update(Message::PollResult(make_payload(2)));
        // First poll: all are "new"
        assert_eq!(app.new_pr_ids.len(), 2);

        // Second poll with same data: none new
        app.update(Message::PollResult(make_payload(2)));
        assert_eq!(app.new_pr_ids.len(), 0);
    }
}
