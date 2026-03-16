use std::collections::HashSet;
use std::time::Instant;

use chrono::{DateTime, Utc};
use indexmap::IndexMap;
use ratatui::widgets::ListState;

use crate::colors::ColorScheme;
use crate::diff::diff_pr_sets;
use crate::notify::Notification;
use crate::poller::PollPayload;
use crate::types::{PrId, PrRole, PrState, PullRequest, ReviewDecision};

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
    pub new_comment_pr_ids: HashSet<PrId>,
    pub dismissed_ids: HashSet<PrId>,
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
            new_comment_pr_ids: HashSet::new(),
            dismissed_ids: HashSet::new(),
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
                self.dismiss_if_done(i);
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
                self.dismiss_if_done(i);
                self.last_activity = Some(Instant::now());
                self.dirty = true;
            }
            Message::OpenSelected => {
                if let Some(i) = self.list_state.selected()
                    && let Some((id, pr)) = self.prs.get_index(i)
                {
                    self.new_comment_pr_ids.remove(id);
                    let url = pr.url.clone();
                    if open::that(&url).is_err() {
                        self.status_message = Some(format!("Failed to open browser: {url}"));
                        self.dirty = true;
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
                // Dismissed (closed/merged) PRs should not re-enter the list from the poller.
                let mut incoming = payload.prs;
                for id in &self.dismissed_ids {
                    incoming.shift_remove(id);
                }

                let already_loaded = matches!(self.loading, LoadingState::Loaded);

                if !already_loaded {
                    // Initial load: show only open PRs.
                    incoming.retain(|_, pr| pr.state == PrState::Open);
                } else {
                    // Subsequent polls: accept already-tracked PRs (they may have transitioned
                    // from open to closed/merged) and new open PRs only.
                    // This prevents closed/merged PRs that were never seen as open this session
                    // from appearing in the list.
                    incoming
                        .retain(|id, pr| self.prs.contains_key(id) || pr.state == PrState::Open);
                }

                let diff = diff_pr_sets(&self.prs, &incoming);

                if already_loaded {
                    // New PR added: only notify when we are NOT the author (review request)
                    for id in &diff.added {
                        if let Some(pr) = incoming.get(id)
                            && pr.role != PrRole::Author
                        {
                            self.pending_notifications.push(Notification {
                                title: "Review requested".to_string(),
                                body: format!("{} ({})", pr.title, id),
                            });
                        }
                    }

                    // Updated PR: notify on close/merge (author only) or review_decision change
                    for id in &diff.updated {
                        let old_pr = self.prs.get(id);
                        let new_pr = incoming.get(id);
                        if let (Some(old_pr), Some(new_pr)) = (old_pr, new_pr) {
                            if old_pr.state == PrState::Open
                                && matches!(new_pr.state, PrState::Closed | PrState::Merged)
                                && new_pr.role == PrRole::Author
                            {
                                let title = match new_pr.state {
                                    PrState::Merged => "PR merged",
                                    _ => "PR closed",
                                };
                                self.pending_notifications.push(Notification {
                                    title: title.to_string(),
                                    body: format!("{} ({})", new_pr.title, id),
                                });
                            }

                            let old_decision = old_pr.review_decision.as_ref();
                            let new_decision = new_pr.review_decision.as_ref();
                            let became_review_required =
                                matches!(new_decision, Some(ReviewDecision::ReviewRequired))
                                    && !matches!(
                                        old_decision,
                                        Some(ReviewDecision::ReviewRequired)
                                    );
                            if became_review_required {
                                self.pending_notifications.push(Notification {
                                    title: "Re-review requested".to_string(),
                                    body: format!("{} ({})", new_pr.title, id),
                                });
                            }
                        }
                    }

                    // Comment count increase: compare all PRs directly, independent of updated_at
                    for (id, new_pr) in &incoming {
                        if let Some(old_pr) = self.prs.get(id)
                            && new_pr.total_comments > old_pr.total_comments
                            && matches!(new_pr.role, PrRole::Author | PrRole::Both)
                        {
                            self.pending_notifications.push(Notification {
                                title: "New comment".to_string(),
                                body: format!("{} ({})", new_pr.title, id),
                            });
                            self.new_comment_pr_ids.insert(id.clone());
                        }
                    }
                }

                self.new_pr_ids = diff.added.into_iter().collect();
                // Prune new_comment_pr_ids for PRs no longer in the list
                self.new_comment_pr_ids
                    .retain(|id| incoming.contains_key(id));
                self.prs = incoming;
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

    /// フォーカスされた PR が closed/merged なら即座にリストから削除する。
    fn dismiss_if_done(&mut self, i: usize) {
        let info = self.prs.get_index(i).map(|(id, pr)| {
            (
                id.clone(),
                matches!(pr.state, PrState::Closed | PrState::Merged),
            )
        });
        if let Some((id, is_done)) = info {
            self.new_pr_ids.remove(&id);
            self.new_comment_pr_ids.remove(&id);
            if is_done {
                self.dismissed_ids.insert(id.clone());
                self.prs.shift_remove(&id);
                if self.prs.is_empty() {
                    self.list_state.select(None);
                } else {
                    self.list_state.select(Some(i.min(self.prs.len() - 1)));
                }
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
        make_pr_with_comments(id, role, review_decision, updated_secs, 0)
    }

    fn make_pr_with_comments(
        id: &PrId,
        role: PrRole,
        review_decision: Option<ReviewDecision>,
        updated_secs: i64,
        total_comments: u64,
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
            total_comments,
        }
    }

    fn make_closed_pr(id: &PrId, role: PrRole, updated_secs: i64) -> PullRequest {
        PullRequest {
            state: PrState::Closed,
            ..make_pr_with_comments(id, role, None, updated_secs, 0)
        }
    }

    fn make_merged_pr(id: &PrId, role: PrRole, updated_secs: i64) -> PullRequest {
        PullRequest {
            state: PrState::Merged,
            ..make_pr_with_comments(id, role, None, updated_secs, 0)
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
        prs.insert(
            id.clone(),
            make_pr_custom(&id, PrRole::ReviewRequested, None, 0),
        );
        app.update(Message::PollResult(payload_from(prs)));
        assert!(app.pending_notifications.is_empty());
    }

    #[test]
    fn closed_author_pr_triggers_notification() {
        let mut app = App::new(ColorScheme::default());
        let id = make_id(1);
        let mut prs = IndexMap::new();
        prs.insert(id.clone(), make_pr_custom(&id, PrRole::Author, None, 0));
        app.update(Message::PollResult(payload_from(prs)));

        // Second poll: same PR now Closed (updated_at bumped)
        let mut prs2 = IndexMap::new();
        prs2.insert(id.clone(), make_closed_pr(&id, PrRole::Author, 100));
        app.update(Message::PollResult(payload_from(prs2)));

        assert_eq!(app.pending_notifications.len(), 1);
        assert_eq!(app.pending_notifications[0].title, "PR closed");
    }

    #[test]
    fn merged_author_pr_triggers_notification() {
        let mut app = App::new(ColorScheme::default());
        let id = make_id(1);
        let mut prs = IndexMap::new();
        prs.insert(id.clone(), make_pr_custom(&id, PrRole::Author, None, 0));
        app.update(Message::PollResult(payload_from(prs)));

        let mut prs2 = IndexMap::new();
        prs2.insert(id.clone(), make_merged_pr(&id, PrRole::Author, 100));
        app.update(Message::PollResult(payload_from(prs2)));

        assert_eq!(app.pending_notifications.len(), 1);
        assert_eq!(app.pending_notifications[0].title, "PR merged");
    }

    #[test]
    fn closed_reviewer_pr_no_notification() {
        let mut app = App::new(ColorScheme::default());
        let id = make_id(1);
        let mut prs = IndexMap::new();
        prs.insert(
            id.clone(),
            make_pr_custom(&id, PrRole::ReviewRequested, None, 0),
        );
        app.update(Message::PollResult(payload_from(prs)));

        let mut prs2 = IndexMap::new();
        prs2.insert(
            id.clone(),
            make_closed_pr(&id, PrRole::ReviewRequested, 100),
        );
        app.update(Message::PollResult(payload_from(prs2)));

        assert!(app.pending_notifications.is_empty());
    }

    #[test]
    fn focus_on_closed_pr_removes_it() {
        let mut app = App::new(ColorScheme::default());
        let id_open = make_id(1);
        let id_closed = make_id(2);

        // Initial poll: both open
        let mut prs = IndexMap::new();
        prs.insert(
            id_open.clone(),
            make_pr_custom(&id_open, PrRole::Author, None, 0),
        );
        prs.insert(
            id_closed.clone(),
            make_pr_custom(&id_closed, PrRole::Author, None, 0),
        );
        app.update(Message::PollResult(payload_from(prs)));

        // Second poll: id_closed transitions to Closed during session
        let mut prs2 = IndexMap::new();
        prs2.insert(
            id_open.clone(),
            make_pr_custom(&id_open, PrRole::Author, None, 0),
        );
        prs2.insert(
            id_closed.clone(),
            make_closed_pr(&id_closed, PrRole::Author, 100),
        );
        app.update(Message::PollResult(payload_from(prs2)));

        // Focus index 0 (open) - stays
        app.update(Message::MoveDown);
        assert_eq!(app.prs.len(), 2);

        // Focus index 1 (closed) - gets removed
        app.update(Message::MoveDown);
        assert_eq!(app.prs.len(), 1);
        assert!(app.prs.contains_key(&id_open));
        assert!(!app.prs.contains_key(&id_closed));
    }

    #[test]
    fn focus_on_merged_pr_removes_it() {
        let mut app = App::new(ColorScheme::default());
        let id_merged = make_id(1);
        let mut prs = IndexMap::new();
        prs.insert(
            id_merged.clone(),
            make_merged_pr(&id_merged, PrRole::Author, 0),
        );
        app.update(Message::PollResult(payload_from(prs)));

        app.update(Message::MoveDown);
        assert!(app.prs.is_empty());
        assert_eq!(app.list_state.selected(), None);
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
            make_pr_custom(
                &id,
                PrRole::ReviewRequested,
                Some(ReviewDecision::Approved),
                0,
            ),
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
    fn dismissed_pr_does_not_reappear_after_next_poll() {
        let mut app = App::new(ColorScheme::default());
        let id_open = make_id(1);
        let id_closed = make_id(2);

        // Initial poll: both open
        let mut prs = IndexMap::new();
        prs.insert(
            id_open.clone(),
            make_pr_custom(&id_open, PrRole::Author, None, 0),
        );
        prs.insert(
            id_closed.clone(),
            make_pr_custom(&id_closed, PrRole::Author, None, 0),
        );
        app.update(Message::PollResult(payload_from(prs)));

        // Second poll: id_closed transitions to Closed during session
        let mut prs2 = IndexMap::new();
        prs2.insert(
            id_open.clone(),
            make_pr_custom(&id_open, PrRole::Author, None, 0),
        );
        prs2.insert(
            id_closed.clone(),
            make_closed_pr(&id_closed, PrRole::Author, 100),
        );
        app.update(Message::PollResult(payload_from(prs2)));

        // Focus the closed PR → gets dismissed
        app.update(Message::MoveDown);
        app.update(Message::MoveDown);
        assert!(!app.prs.contains_key(&id_closed));
        assert!(app.dismissed_ids.contains(&id_closed));

        // Next poll still includes the closed PR in payload → should be filtered out
        let mut prs3 = IndexMap::new();
        prs3.insert(
            id_open.clone(),
            make_pr_custom(&id_open, PrRole::Author, None, 0),
        );
        prs3.insert(
            id_closed.clone(),
            make_closed_pr(&id_closed, PrRole::Author, 100),
        );
        app.update(Message::PollResult(payload_from(prs3)));

        assert!(
            !app.prs.contains_key(&id_closed),
            "dismissed PR must not reappear"
        );
        assert_eq!(app.prs.len(), 1);
    }

    #[test]
    fn dismissed_pr_reappear_does_not_trigger_review_notification() {
        let mut app = App::new(ColorScheme::default());
        let id = make_id(1);

        // Initial poll: open reviewer PR
        let mut prs = IndexMap::new();
        prs.insert(
            id.clone(),
            make_pr_custom(&id, PrRole::ReviewRequested, None, 0),
        );
        app.update(Message::PollResult(payload_from(prs)));

        // Second poll: PR transitions to Closed during session
        let mut prs2 = IndexMap::new();
        prs2.insert(
            id.clone(),
            make_closed_pr(&id, PrRole::ReviewRequested, 100),
        );
        app.update(Message::PollResult(payload_from(prs2)));

        // Focus the closed reviewer PR → dismissed
        app.update(Message::MoveDown);
        assert!(app.dismissed_ids.contains(&id));

        // Clear any notifications from the transition poll
        app.pending_notifications.clear();

        // Next poll still includes the dismissed closed PR → must not re-enter or notify
        let mut prs3 = IndexMap::new();
        prs3.insert(
            id.clone(),
            make_closed_pr(&id, PrRole::ReviewRequested, 100),
        );
        app.update(Message::PollResult(payload_from(prs3)));

        assert!(
            app.pending_notifications.is_empty(),
            "dismissed PR re-entry must not produce notifications"
        );
    }

    #[test]
    fn closed_pr_not_tracked_in_session_does_not_appear_on_subsequent_poll() {
        // Regression: closed/merged PRs that were already closed before the session started
        // must not appear in the list, even when they arrive in a subsequent poll payload.
        let mut app = App::new(ColorScheme::default());
        let id_open = make_id(1);
        let id_closing = make_id(2);
        let id_already_closed = make_id(3);

        // Initial poll: only open PRs
        let mut prs = IndexMap::new();
        prs.insert(
            id_open.clone(),
            make_pr_custom(&id_open, PrRole::Author, None, 0),
        );
        prs.insert(
            id_closing.clone(),
            make_pr_custom(&id_closing, PrRole::Author, None, 0),
        );
        app.update(Message::PollResult(payload_from(prs)));
        assert_eq!(app.prs.len(), 2);

        // Second poll: id_closing transitions + an already-closed PR arrives from API
        let mut prs2 = IndexMap::new();
        prs2.insert(
            id_open.clone(),
            make_pr_custom(&id_open, PrRole::Author, None, 0),
        );
        prs2.insert(
            id_closing.clone(),
            make_closed_pr(&id_closing, PrRole::Author, 100),
        );
        prs2.insert(
            id_already_closed.clone(),
            make_closed_pr(&id_already_closed, PrRole::Author, 50),
        );
        app.update(Message::PollResult(payload_from(prs2)));

        assert!(
            !app.prs.contains_key(&id_already_closed),
            "PR closed before session must not appear"
        );
        assert!(
            app.prs.contains_key(&id_closing),
            "PR that transitioned during session must appear"
        );
        assert_eq!(app.prs.len(), 2); // id_open + id_closing
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

    // --- Comment notification logic ---

    #[test]
    fn comment_increase_on_author_pr_triggers_notification() {
        let mut app = App::new(ColorScheme::default());
        let id = make_id(1);
        let mut prs = IndexMap::new();
        prs.insert(
            id.clone(),
            make_pr_with_comments(&id, PrRole::Author, None, 0, 0),
        );
        app.update(Message::PollResult(payload_from(prs)));

        // Second poll: same PR, comment count increased, updated_at bumped
        let mut prs2 = IndexMap::new();
        prs2.insert(
            id.clone(),
            make_pr_with_comments(&id, PrRole::Author, None, 100, 3),
        );
        app.update(Message::PollResult(payload_from(prs2)));

        assert_eq!(app.pending_notifications.len(), 1);
        assert_eq!(app.pending_notifications[0].title, "New comment");
    }

    #[test]
    fn comment_increase_on_reviewer_pr_no_notification() {
        let mut app = App::new(ColorScheme::default());
        let id = make_id(1);
        let mut prs = IndexMap::new();
        prs.insert(
            id.clone(),
            make_pr_with_comments(&id, PrRole::ReviewRequested, None, 0, 0),
        );
        app.update(Message::PollResult(payload_from(prs)));

        // Second poll: comment count increased on a reviewer PR
        let mut prs2 = IndexMap::new();
        prs2.insert(
            id.clone(),
            make_pr_with_comments(&id, PrRole::ReviewRequested, None, 100, 3),
        );
        app.update(Message::PollResult(payload_from(prs2)));

        assert!(app.pending_notifications.is_empty());
    }

    #[test]
    fn comment_unchanged_no_notification() {
        let mut app = App::new(ColorScheme::default());
        let id = make_id(1);
        let mut prs = IndexMap::new();
        prs.insert(
            id.clone(),
            make_pr_with_comments(&id, PrRole::Author, None, 0, 5),
        );
        app.update(Message::PollResult(payload_from(prs)));

        // Second poll: updated_at bumped but comment count unchanged
        let mut prs2 = IndexMap::new();
        prs2.insert(
            id.clone(),
            make_pr_with_comments(&id, PrRole::Author, None, 100, 5),
        );
        app.update(Message::PollResult(payload_from(prs2)));

        assert!(app.pending_notifications.is_empty());
    }

    #[test]
    fn comment_increase_sets_new_comment_pr_ids() {
        let mut app = App::new(ColorScheme::default());
        let id = make_id(1);
        let mut prs = IndexMap::new();
        prs.insert(
            id.clone(),
            make_pr_with_comments(&id, PrRole::Author, None, 0, 0),
        );
        app.update(Message::PollResult(payload_from(prs)));

        let mut prs2 = IndexMap::new();
        prs2.insert(
            id.clone(),
            make_pr_with_comments(&id, PrRole::Author, None, 100, 2),
        );
        app.update(Message::PollResult(payload_from(prs2)));

        assert!(app.new_comment_pr_ids.contains(&id));
    }

    #[test]
    fn navigate_to_pr_clears_new_comment_pr_id() {
        let mut app = App::new(ColorScheme::default());
        let id = make_id(1);
        let mut prs = IndexMap::new();
        prs.insert(
            id.clone(),
            make_pr_with_comments(&id, PrRole::Author, None, 0, 0),
        );
        app.update(Message::PollResult(payload_from(prs)));

        let mut prs2 = IndexMap::new();
        prs2.insert(
            id.clone(),
            make_pr_with_comments(&id, PrRole::Author, None, 100, 2),
        );
        app.update(Message::PollResult(payload_from(prs2)));
        assert!(app.new_comment_pr_ids.contains(&id));

        // Navigate to the PR → should clear from new_comment_pr_ids
        app.update(Message::MoveDown);
        assert!(!app.new_comment_pr_ids.contains(&id));
    }

    #[test]
    fn comment_increase_without_updated_at_triggers_notification() {
        let mut app = App::new(ColorScheme::default());
        let id = make_id(1);
        let mut prs = IndexMap::new();
        // updated_at は固定 (0秒)、コメント数 0
        prs.insert(
            id.clone(),
            make_pr_with_comments(&id, PrRole::Author, None, 0, 0),
        );
        app.update(Message::PollResult(payload_from(prs)));

        // Second poll: updated_at 変化なし (same 0秒)、コメント数だけ増加
        let mut prs2 = IndexMap::new();
        prs2.insert(
            id.clone(),
            make_pr_with_comments(&id, PrRole::Author, None, 0, 3),
        );
        app.update(Message::PollResult(payload_from(prs2)));

        assert_eq!(app.pending_notifications.len(), 1);
        assert_eq!(app.pending_notifications[0].title, "New comment");
    }

    #[test]
    fn comment_increase_on_both_role_pr_triggers_notification() {
        let mut app = App::new(ColorScheme::default());
        let id = make_id(1);
        let mut prs = IndexMap::new();
        prs.insert(
            id.clone(),
            make_pr_with_comments(&id, PrRole::Both, None, 0, 0),
        );
        app.update(Message::PollResult(payload_from(prs)));

        let mut prs2 = IndexMap::new();
        prs2.insert(
            id.clone(),
            make_pr_with_comments(&id, PrRole::Both, None, 100, 2),
        );
        app.update(Message::PollResult(payload_from(prs2)));

        assert_eq!(app.pending_notifications.len(), 1);
        assert_eq!(app.pending_notifications[0].title, "New comment");
    }

    #[test]
    fn open_selected_clears_new_comment_pr_id() {
        let mut app = App::new(ColorScheme::default());
        let id = make_id(1);
        let mut prs = IndexMap::new();
        prs.insert(
            id.clone(),
            make_pr_with_comments(&id, PrRole::Author, None, 0, 0),
        );
        app.update(Message::PollResult(payload_from(prs)));

        let mut prs2 = IndexMap::new();
        prs2.insert(
            id.clone(),
            make_pr_with_comments(&id, PrRole::Author, None, 100, 2),
        );
        app.update(Message::PollResult(payload_from(prs2)));
        app.update(Message::MoveDown); // select the PR
        // re-add flag manually to simulate state
        app.new_comment_pr_ids.insert(id.clone());

        app.update(Message::OpenSelected);
        assert!(!app.new_comment_pr_ids.contains(&id));
    }

    #[test]
    fn new_comment_pr_ids_pruned_when_pr_removed_from_list() {
        let mut app = App::new(ColorScheme::default());
        let id = make_id(1);
        let mut prs = IndexMap::new();
        prs.insert(
            id.clone(),
            make_pr_with_comments(&id, PrRole::Author, None, 0, 0),
        );
        app.update(Message::PollResult(payload_from(prs)));

        let mut prs2 = IndexMap::new();
        prs2.insert(
            id.clone(),
            make_pr_with_comments(&id, PrRole::Author, None, 100, 2),
        );
        app.update(Message::PollResult(payload_from(prs2)));
        assert!(app.new_comment_pr_ids.contains(&id));

        // Third poll: PR is gone from the list
        app.update(Message::PollResult(payload_from(IndexMap::new())));
        assert!(!app.new_comment_pr_ids.contains(&id));
    }
}
