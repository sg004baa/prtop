use std::collections::HashSet;
use std::time::Instant;

use chrono::{DateTime, Utc};
use indexmap::IndexMap;
use ratatui::widgets::ListState;

use crate::colors::ColorScheme;
use crate::config::NotifyEvent;
use crate::diff::diff_pr_sets;
use crate::notify::Notification;
use crate::poller::PollPayload;
use crate::types::{CiStatus, PrId, PrRole, PrState, PullRequest, ReviewDecision};

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
pub const ROLE_ORDER: [PrRole; 3] = [PrRole::Author, PrRole::ReviewRequested, PrRole::Mentioned];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VisibleRow {
    Role(PrRole),
    Pr(PrId),
}

pub enum Message {
    Quit,
    MoveUp,
    MoveDown,
    ActivateSelected,
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
    pub visible_rows: Vec<VisibleRow>,
    pub collapsed_roles: HashSet<PrRole>,
    pub ui_state_dirty: bool,
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
    /// ブラウザで開いて既読扱いになった Mentioned PR。main ループが drain して
    /// DismissStore へ永続化する(pending_notifications と同じ drain パターン)。
    pub pending_dismissals: Vec<PrId>,
    pub notify_events: HashSet<NotifyEvent>,
    pub colors: ColorScheme,
    pub username: String,
}

impl App {
    pub fn new(
        username: String,
        colors: ColorScheme,
        notify_events: HashSet<NotifyEvent>,
        collapsed_roles: HashSet<PrRole>,
    ) -> Self {
        let mut app = Self {
            prs: IndexMap::new(),
            list_state: ListState::default(),
            visible_rows: Vec::new(),
            collapsed_roles,
            ui_state_dirty: false,
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
            pending_dismissals: Vec::new(),
            notify_events,
            colors,
            username,
        };
        app.rebuild_visible_rows(None);
        app
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
                if !matches!(self.loading, LoadingState::Loaded) {
                    return;
                }
                let prev_id = self.selected_id();
                let target = match self.list_state.selected() {
                    Some(0) => self.visible_rows.len() - 1,
                    Some(i) => i - 1,
                    None => 0,
                };
                self.move_focus_to(target, prev_id);
                self.last_activity = Some(Instant::now());
                self.dirty = true;
            }
            Message::MoveDown => {
                if !matches!(self.loading, LoadingState::Loaded) {
                    return;
                }
                let prev_id = self.selected_id();
                let target = match self.list_state.selected() {
                    Some(i) if i >= self.visible_rows.len() - 1 => 0,
                    Some(i) => i + 1,
                    None => 0,
                };
                self.move_focus_to(target, prev_id);
                self.last_activity = Some(Instant::now());
                self.dirty = true;
            }
            Message::ActivateSelected => {
                match self.selected_row().cloned() {
                    Some(VisibleRow::Role(role)) => self.toggle_role(role),
                    Some(VisibleRow::Pr(_)) => self.open_selected_pr(),
                    None => {}
                }
                self.last_activity = Some(Instant::now());
            }
            Message::OpenSelected => {
                self.open_selected_pr();
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
                    let prev_id = self.selected_id();
                    self.list_state.select(None);
                    if let Some(prev_id) = prev_id {
                        self.dismiss_if_done(&prev_id);
                        self.rebuild_visible_rows(None);
                    }
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
                    // New PR added: notify based on our role on it (author gets no notification)
                    for id in &diff.added {
                        if let Some(pr) = incoming.get(id) {
                            let notification = match pr.role {
                                PrRole::ReviewRequested => {
                                    Some(("Review requested", NotifyEvent::ReviewRequested))
                                }
                                PrRole::Mentioned => {
                                    Some(("Mentioned in PR", NotifyEvent::Mentioned))
                                }
                                PrRole::Author => None,
                            };
                            if let Some((title, event)) = notification
                                && self.notify_events.contains(&event)
                            {
                                self.pending_notifications.push(Notification {
                                    title: title.to_string(),
                                    body: format!("{} ({})", pr.title, id),
                                });
                            }
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
                                let (title, event) = match new_pr.state {
                                    PrState::Merged => ("PR merged", NotifyEvent::PrMerged),
                                    _ => ("PR closed", NotifyEvent::PrClosed),
                                };
                                if self.notify_events.contains(&event) {
                                    self.pending_notifications.push(Notification {
                                        title: title.to_string(),
                                        body: format!("{} ({})", new_pr.title, id),
                                    });
                                }
                            }

                            let old_decision = old_pr.review_decision.as_ref();
                            let new_decision = new_pr.review_decision.as_ref();
                            let became_review_required =
                                matches!(new_decision, Some(ReviewDecision::ReviewRequired))
                                    && !matches!(
                                        old_decision,
                                        Some(ReviewDecision::ReviewRequired)
                                    );
                            if became_review_required
                                && self.notify_events.contains(&NotifyEvent::ReReviewRequested)
                            {
                                self.pending_notifications.push(Notification {
                                    title: "Re-review requested".to_string(),
                                    body: format!("{} ({})", new_pr.title, id),
                                });
                            }
                        }
                    }

                    // Comment count increase: compare all PRs directly, independent of updated_at
                    // Skip notification if the last commenter is the current user (self-filter)
                    for (id, new_pr) in &incoming {
                        if let Some(old_pr) = self.prs.get(id)
                            && new_pr.total_comments > old_pr.total_comments
                            && new_pr.role == PrRole::Author
                            && new_pr.last_commenter.as_deref() != Some(self.username.as_str())
                        {
                            if self.notify_events.contains(&NotifyEvent::NewComment) {
                                self.pending_notifications.push(Notification {
                                    title: "New comment".to_string(),
                                    body: format!("{} ({})", new_pr.title, id),
                                });
                            }
                            self.new_comment_pr_ids.insert(id.clone());
                        }
                    }

                    // CI status change: notify author when CI transitions from in-progress to finished
                    for (id, new_pr) in &incoming {
                        if let Some(old_pr) = self.prs.get(id)
                            && new_pr.role == PrRole::Author
                            && old_pr
                                .ci_status
                                .as_ref()
                                .is_some_and(CiStatus::is_in_progress)
                            && new_pr.ci_status.as_ref().is_some_and(CiStatus::is_finished)
                            && self.notify_events.contains(&NotifyEvent::CiFinished)
                        {
                            let title = match new_pr.ci_status {
                                Some(CiStatus::Success) => "CI passed",
                                _ => "CI failed",
                            };
                            self.pending_notifications.push(Notification {
                                title: title.to_string(),
                                body: format!("{} ({})", new_pr.title, id),
                            });
                        }
                    }
                }

                if already_loaded {
                    self.new_pr_ids.extend(diff.added.iter().cloned());
                }
                self.new_pr_ids.retain(|id| incoming.contains_key(id));
                // Prune new_comment_pr_ids for PRs no longer in the list
                self.new_comment_pr_ids
                    .retain(|id| incoming.contains_key(id));
                // Selection follows a stable visible-row identity, not an index. This keeps role
                // headers and PRs attached across deterministic re-sorts and poll rebuilds.
                let selected = self.selected_row().cloned();
                self.prs = incoming;
                sort_prs(&mut self.prs);
                self.rebuild_visible_rows(selected);
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
                // フッターは status_message を優先表示するため、"Refreshing..." が
                // 残っているとエラーが見えないままになる。
                self.status_message = None;
                if matches!(self.loading, LoadingState::Initial | LoadingState::Loading) {
                    self.loading = LoadingState::Error(msg);
                }
                self.dirty = true;
            }
        }
    }

    fn selected_id(&self) -> Option<PrId> {
        match self.selected_row() {
            Some(VisibleRow::Pr(id)) => Some(id.clone()),
            _ => None,
        }
    }

    pub fn selected_row(&self) -> Option<&VisibleRow> {
        self.list_state
            .selected()
            .and_then(|i| self.visible_rows.get(i))
    }

    pub fn role_is_collapsed(&self, role: PrRole) -> bool {
        self.collapsed_roles.contains(&role)
    }

    fn rebuild_visible_rows(&mut self, selected: Option<VisibleRow>) {
        self.visible_rows.clear();
        self.visible_rows.reserve(self.prs.len() + ROLE_ORDER.len());
        for role in ROLE_ORDER {
            self.visible_rows.push(VisibleRow::Role(role));
            if !self.collapsed_roles.contains(&role) {
                self.visible_rows.extend(
                    self.prs
                        .iter()
                        .filter(|(_, pr)| pr.role == role)
                        .map(|(id, _)| VisibleRow::Pr(id.clone())),
                );
            }
        }
        self.list_state.select(
            selected.and_then(|selected| self.visible_rows.iter().position(|row| row == &selected)),
        );
    }

    fn toggle_role(&mut self, role: PrRole) {
        if !self.collapsed_roles.remove(&role) {
            self.collapsed_roles.insert(role);
        }
        self.rebuild_visible_rows(Some(VisibleRow::Role(role)));
        self.ui_state_dirty = true;
        self.dirty = true;
    }

    fn open_selected_pr(&mut self) {
        let Some(id) = self.selected_id() else {
            return;
        };
        let Some(pr) = self.prs.get(&id) else {
            return;
        };
        let selected_idx = self.list_state.selected().unwrap_or(0);
        let url = pr.url.clone();
        let is_mentioned = pr.role == PrRole::Mentioned;

        let removed_new = self.new_pr_ids.remove(&id);
        let removed_comment = self.new_comment_pr_ids.remove(&id);
        if removed_new || removed_comment {
            self.dirty = true;
        }
        if open::that(&url).is_err() {
            self.status_message = Some(format!("Failed to open browser: {url}"));
            self.dirty = true;
        }
        if is_mentioned {
            self.pending_dismissals.push(id.clone());
            self.prs.shift_remove(&id);
            self.rebuild_visible_rows(None);
            self.list_state
                .select(Some(selected_idx.min(self.visible_rows.len() - 1)));
            self.dirty = true;
        }
    }

    fn move_focus_to(&mut self, target_idx: usize, prev_id: Option<PrId>) {
        let target = self.visible_rows.get(target_idx).cloned();
        let target_id = match &target {
            Some(VisibleRow::Pr(id)) => Some(id.clone()),
            _ => None,
        };

        if prev_id.as_ref() != target_id.as_ref()
            && let Some(prev_id) = prev_id.as_ref()
        {
            self.dismiss_if_done(prev_id);
        }

        if let Some(target_id) = target_id {
            self.new_pr_ids.remove(&target_id);
            self.new_comment_pr_ids.remove(&target_id);
        }
        self.rebuild_visible_rows(target);
    }

    /// `id` の PR が closed/merged ならリストから削除し、再ポーリングで再登場しないよう dismiss 集合に積む。
    fn dismiss_if_done(&mut self, id: &PrId) {
        let is_done = self
            .prs
            .get(id)
            .is_some_and(|pr| matches!(pr.state, PrState::Closed | PrState::Merged));
        if is_done {
            self.dismissed_ids.insert(id.clone());
            self.prs.shift_remove(id);
        }
    }

    #[allow(dead_code)]
    pub fn selected_pr(&self) -> Option<&PullRequest> {
        self.selected_id().and_then(|id| self.prs.get(&id))
    }
}

fn sort_prs(prs: &mut IndexMap<PrId, PullRequest>) {
    prs.sort_by(|id_a, pr_a, id_b, pr_b| {
        role_rank(pr_a.role)
            .cmp(&role_rank(pr_b.role))
            .then_with(|| id_b.repo.cmp(&id_a.repo))
            .then_with(|| id_b.number.cmp(&id_a.number))
            .then_with(|| id_b.owner.cmp(&id_a.owner))
    });
}

fn role_rank(role: PrRole) -> u8 {
    match role {
        PrRole::Author => 0,
        PrRole::ReviewRequested => 1,
        PrRole::Mentioned => 2,
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
    fn make_named_id(owner: &str, repo: &str, number: u64) -> PrId {
        PrId {
            owner: owner.to_string(),
            repo: repo.to_string(),
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
            last_commenter: None,
            ci_status: None,
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
    fn test_app(notify_events: HashSet<NotifyEvent>) -> App {
        App::new(
            "testuser".to_string(),
            ColorScheme::default(),
            notify_events,
            HashSet::new(),
        )
    }

    fn select_row(app: &mut App, row: VisibleRow) {
        let index = app
            .visible_rows
            .iter()
            .position(|candidate| candidate == &row)
            .expect("test row must be visible");
        app.list_state.select(Some(index));
    }

    fn select_pr(app: &mut App, id: &PrId) {
        select_row(app, VisibleRow::Pr(id.clone()));
    }
    fn focus_pr(app: &mut App, id: &PrId) {
        let index = app
            .visible_rows
            .iter()
            .position(|row| row == &VisibleRow::Pr(id.clone()))
            .expect("test PR must be visible");
        let previous = app.selected_id();
        app.move_focus_to(index, previous);
    }

    #[test]
    fn quit_sets_flag() {
        let mut app = test_app(NotifyEvent::all());
        app.update(Message::Quit);
        assert!(app.should_quit);
    }

    #[test]
    fn poll_result_updates_prs() {
        let mut app = test_app(NotifyEvent::all());
        app.update(Message::PollResult(make_payload(3)));
        assert_eq!(app.prs.len(), 3);
        assert!(matches!(app.loading, LoadingState::Loaded));
        assert_eq!(app.list_state.selected(), None);
    }

    #[test]
    fn navigation_wraps() {
        let mut app = test_app(NotifyEvent::all());
        app.update(Message::PollResult(make_payload(3)));
        assert_eq!(app.list_state.selected(), None);

        // MoveUp from None selects the first navigable header.
        app.update(Message::MoveUp);
        assert_eq!(app.selected_row(), Some(&VisibleRow::Role(PrRole::Author)));

        // Moving up from the first row wraps to the final role header.
        app.update(Message::MoveUp);
        assert_eq!(
            app.selected_row(),
            Some(&VisibleRow::Role(PrRole::Mentioned))
        );

        app.update(Message::MoveDown);
        assert_eq!(app.selected_row(), Some(&VisibleRow::Role(PrRole::Author)));
    }
    #[test]
    fn accepted_poll_is_sorted_by_role_then_descending_pr_identity() {
        let mut app = test_app(NotifyEvent::all());
        let author_a = make_named_id("a", "alpha", 9);
        let author_z = make_named_id("a", "zeta", 1);
        let review_a = make_named_id("a", "same", 2);
        let review_z = make_named_id("z", "same", 2);
        let mention = make_named_id("a", "mention", 4);
        let mut prs = IndexMap::new();
        for (id, role) in [
            (mention.clone(), PrRole::Mentioned),
            (review_a.clone(), PrRole::ReviewRequested),
            (author_a.clone(), PrRole::Author),
            (review_z.clone(), PrRole::ReviewRequested),
            (author_z.clone(), PrRole::Author),
        ] {
            prs.insert(id.clone(), make_pr_custom(&id, role, None, 0));
        }

        app.update(Message::PollResult(payload_from(prs)));

        assert_eq!(
            app.prs.keys().cloned().collect::<Vec<_>>(),
            vec![author_z, author_a, review_z, review_a, mention]
        );
    }

    #[test]
    fn role_headers_toggle_children_and_keep_header_selected() {
        let mut app = test_app(NotifyEvent::all());
        let id = make_id(1);
        let mut prs = IndexMap::new();
        prs.insert(id.clone(), make_pr_custom(&id, PrRole::Author, None, 0));
        app.update(Message::PollResult(payload_from(prs)));
        select_row(&mut app, VisibleRow::Role(PrRole::Author));

        app.update(Message::ActivateSelected);

        assert!(app.role_is_collapsed(PrRole::Author));
        assert!(!app.visible_rows.contains(&VisibleRow::Pr(id.clone())));
        assert_eq!(app.selected_row(), Some(&VisibleRow::Role(PrRole::Author)));
        assert!(app.ui_state_dirty);

        app.ui_state_dirty = false;
        app.update(Message::ActivateSelected);
        assert!(!app.role_is_collapsed(PrRole::Author));
        assert!(app.visible_rows.contains(&VisibleRow::Pr(id)));
        assert!(app.ui_state_dirty);
    }

    #[test]
    fn explicit_open_is_inert_on_role_header() {
        let mut app = test_app(NotifyEvent::all());
        app.update(Message::PollResult(make_payload(1)));
        select_row(&mut app, VisibleRow::Role(PrRole::Author));

        app.update(Message::OpenSelected);

        assert!(!app.role_is_collapsed(PrRole::Author));
        assert!(app.pending_dismissals.is_empty());
        assert_eq!(app.prs.len(), 1);
    }

    #[test]
    fn reconstructed_app_honors_persisted_collapsed_roles() {
        let id = make_id(1);
        let mut app = App::new(
            "testuser".to_string(),
            ColorScheme::default(),
            NotifyEvent::all(),
            HashSet::from([PrRole::Author]),
        );
        let mut prs = IndexMap::new();
        prs.insert(id.clone(), make_pr_custom(&id, PrRole::Author, None, 0));
        app.update(Message::PollResult(payload_from(prs)));

        assert_eq!(
            app.visible_rows,
            vec![
                VisibleRow::Role(PrRole::Author),
                VisibleRow::Role(PrRole::ReviewRequested),
                VisibleRow::Role(PrRole::Mentioned),
            ]
        );
    }

    #[test]
    fn toggle_help() {
        let mut app = test_app(NotifyEvent::all());
        assert_eq!(app.screen, Screen::PrList);
        app.update(Message::ToggleHelp);
        assert_eq!(app.screen, Screen::Help);
        app.update(Message::ToggleHelp);
        assert_eq!(app.screen, Screen::PrList);
    }

    #[test]
    fn poll_error_sets_state() {
        let mut app = test_app(NotifyEvent::all());
        app.update(Message::PollError("network error".to_string()));
        assert!(app.poll_error.is_some());
        assert!(matches!(app.loading, LoadingState::Error(_)));
    }

    #[test]
    fn poll_error_clears_stale_status_message() {
        let mut app = test_app(NotifyEvent::all());
        app.update(Message::PollResult(make_payload(1)));
        app.update(Message::Refresh);
        assert!(app.status_message.is_some());

        // The footer prefers status_message over poll_error, so a failed refresh
        // must clear "Refreshing..." or the error stays invisible forever.
        app.update(Message::PollError("network error".to_string()));
        assert_eq!(app.status_message, None);
        assert!(app.poll_error.is_some());
    }

    // --- Notification logic ---

    #[test]
    fn no_notifications_on_first_poll() {
        let mut app = test_app(NotifyEvent::all());
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
        let mut app = test_app(NotifyEvent::all());
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
        let mut app = test_app(NotifyEvent::all());
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
        let mut app = test_app(NotifyEvent::all());
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
    fn focus_on_closed_pr_keeps_it_until_blur() {
        let mut app = test_app(NotifyEvent::all());
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

        // Focus the open PR first, then move onto the adjacent closed PR.
        select_pr(&mut app, &id_open);
        app.update(Message::MoveUp);
        assert_eq!(app.prs.len(), 2);
        assert_eq!(app.selected_pr().unwrap().id, id_closed);

        // Move away from the closed PR - now it gets dismissed.
        app.update(Message::MoveDown);
        assert_eq!(app.prs.len(), 1);
        assert!(app.prs.contains_key(&id_open));
        assert!(!app.prs.contains_key(&id_closed));
        assert_eq!(app.selected_pr().unwrap().id, id_open);
    }

    #[test]
    fn focus_on_merged_pr_keeps_it_until_deselect() {
        let mut app = test_app(NotifyEvent::all());
        let id_merged = make_id(1);

        // Initial poll: PR is open
        let mut prs = IndexMap::new();
        prs.insert(
            id_merged.clone(),
            make_pr_custom(&id_merged, PrRole::Author, None, 0),
        );
        app.update(Message::PollResult(payload_from(prs)));

        // Second poll: PR transitions to merged
        let mut prs2 = IndexMap::new();
        prs2.insert(
            id_merged.clone(),
            make_merged_pr(&id_merged, PrRole::Author, 100),
        );
        app.update(Message::PollResult(payload_from(prs2)));

        // Focus the merged PR - stays in the list so the user can review it.
        select_pr(&mut app, &id_merged);
        assert_eq!(app.prs.len(), 1);
        assert_eq!(app.selected_pr().unwrap().id, id_merged);

        // Deselect drops focus and dismisses the merged PR
        app.update(Message::Deselect);
        assert!(app.prs.is_empty());
        assert_eq!(app.list_state.selected(), None);
    }

    #[test]
    fn move_away_from_merged_pr_removes_it() {
        let mut app = test_app(NotifyEvent::all());
        let id_open = make_id(1);
        let id_merged = make_id(2);

        let mut prs = IndexMap::new();
        prs.insert(
            id_open.clone(),
            make_pr_custom(&id_open, PrRole::Author, None, 0),
        );
        prs.insert(
            id_merged.clone(),
            make_pr_custom(&id_merged, PrRole::Author, None, 0),
        );
        app.update(Message::PollResult(payload_from(prs)));

        let mut prs2 = IndexMap::new();
        prs2.insert(
            id_open.clone(),
            make_pr_custom(&id_open, PrRole::Author, None, 0),
        );
        prs2.insert(
            id_merged.clone(),
            make_merged_pr(&id_merged, PrRole::Author, 100),
        );
        app.update(Message::PollResult(payload_from(prs2)));

        // Step onto the merged PR.
        select_pr(&mut app, &id_merged);
        assert_eq!(app.prs.len(), 2);

        // Step down — merged PR is dismissed, focus lands on the open one.
        app.update(Message::MoveDown);
        assert_eq!(app.prs.len(), 1);
        assert!(!app.prs.contains_key(&id_merged));
        assert_eq!(app.selected_pr().unwrap().id, id_open);
    }

    #[test]
    fn added_reviewer_pr_triggers_notification() {
        let mut app = test_app(NotifyEvent::all());
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
        let mut app = test_app(NotifyEvent::all());
        app.update(Message::PollResult(payload_from(IndexMap::new())));
        let id = make_id(1);
        let mut prs = IndexMap::new();
        prs.insert(id.clone(), make_pr_custom(&id, PrRole::Author, None, 0));
        app.update(Message::PollResult(payload_from(prs)));
        assert!(app.pending_notifications.is_empty());
    }

    #[test]
    fn review_required_transition_triggers_notification() {
        let mut app = test_app(NotifyEvent::all());
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
        let mut app = test_app(NotifyEvent::all());
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
        let mut app = test_app(NotifyEvent::all());
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

        // Focus the closed PR, then move away → gets dismissed on blur.
        select_pr(&mut app, &id_closed);
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
        let mut app = test_app(NotifyEvent::all());
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

        // Focus the closed reviewer PR, then deselect → dismissed.
        select_pr(&mut app, &id);
        app.update(Message::Deselect);
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
        let mut app = test_app(NotifyEvent::all());
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
    fn selection_follows_pr_identity_across_poll_reorder() {
        let mut app = test_app(NotifyEvent::all());
        let id_a = make_id(1);
        let id_b = make_id(2);

        let mut prs = IndexMap::new();
        prs.insert(id_a.clone(), make_pr_custom(&id_a, PrRole::Author, None, 0));
        prs.insert(
            id_b.clone(),
            make_pr_custom(&id_b, PrRole::ReviewRequested, None, 0),
        );
        app.update(Message::PollResult(payload_from(prs)));

        select_pr(&mut app, &id_b);
        assert_eq!(app.selected_pr().unwrap().id, id_b);

        // The next accepted poll moves id_b to another role group.
        let mut prs2 = IndexMap::new();
        prs2.insert(id_b.clone(), make_pr_custom(&id_b, PrRole::Author, None, 0));
        prs2.insert(id_a.clone(), make_pr_custom(&id_a, PrRole::Author, None, 0));
        app.update(Message::PollResult(payload_from(prs2)));

        assert_eq!(
            app.selected_pr().unwrap().id,
            id_b,
            "selection must follow the PR, not the row index"
        );
        assert_eq!(app.selected_row(), Some(&VisibleRow::Pr(id_b)));
    }

    #[test]
    fn selection_cleared_when_selected_pr_disappears() {
        let mut app = test_app(NotifyEvent::all());
        let id_a = make_id(1);
        let id_b = make_id(2);

        let mut prs = IndexMap::new();
        prs.insert(id_a.clone(), make_pr_custom(&id_a, PrRole::Author, None, 0));
        prs.insert(id_b.clone(), make_pr_custom(&id_b, PrRole::Author, None, 0));
        app.update(Message::PollResult(payload_from(prs)));

        select_pr(&mut app, &id_b);

        // Next poll: id_b is gone
        let mut prs2 = IndexMap::new();
        prs2.insert(id_a.clone(), make_pr_custom(&id_a, PrRole::Author, None, 0));
        app.update(Message::PollResult(payload_from(prs2)));

        assert_eq!(
            app.list_state.selected(),
            None,
            "stale index must not point at a different PR"
        );
    }

    #[test]
    fn new_prs_detected() {
        let mut app = test_app(NotifyEvent::all());
        app.update(Message::PollResult(make_payload(2)));
        // Initial load: no markers (all PRs are "known" from the start)
        assert_eq!(app.new_pr_ids.len(), 0);

        // Second poll with a new PR added: only the new one gets a marker
        app.update(Message::PollResult(make_payload(3)));
        assert_eq!(app.new_pr_ids.len(), 1);

        // Third poll with same data: marker persists
        app.update(Message::PollResult(make_payload(3)));
        assert_eq!(app.new_pr_ids.len(), 1);

        // Focusing a known PR preserves the new PR marker.
        focus_pr(&mut app, &make_id(0));
        assert_eq!(app.new_pr_ids.len(), 1);

        focus_pr(&mut app, &make_id(2));
        assert_eq!(app.new_pr_ids.len(), 0);
    }

    // --- Comment notification logic ---

    #[test]
    fn comment_increase_on_author_pr_triggers_notification() {
        let mut app = test_app(NotifyEvent::all());
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
        let mut app = test_app(NotifyEvent::all());
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
        let mut app = test_app(NotifyEvent::all());
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
        let mut app = test_app(NotifyEvent::all());
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
        let mut app = test_app(NotifyEvent::all());
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

        // Navigate to the PR → should clear from new_comment_pr_ids.
        focus_pr(&mut app, &id);
        assert!(!app.new_comment_pr_ids.contains(&id));
    }

    #[test]
    fn comment_increase_without_updated_at_triggers_notification() {
        let mut app = test_app(NotifyEvent::all());
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
    fn comment_increase_on_mentioned_role_pr_no_notification() {
        let mut app = test_app(NotifyEvent::all());
        let id = make_id(1);
        let mut prs = IndexMap::new();
        prs.insert(
            id.clone(),
            make_pr_with_comments(&id, PrRole::Mentioned, None, 0, 0),
        );
        app.update(Message::PollResult(payload_from(prs)));

        let mut prs2 = IndexMap::new();
        prs2.insert(
            id.clone(),
            make_pr_with_comments(&id, PrRole::Mentioned, None, 100, 2),
        );
        app.update(Message::PollResult(payload_from(prs2)));

        assert!(
            app.pending_notifications.is_empty(),
            "comment notification is author-only"
        );
    }

    // --- Mentioned role ---

    #[test]
    fn added_mentioned_pr_triggers_mentioned_notification() {
        let mut app = test_app(NotifyEvent::all());
        app.update(Message::PollResult(payload_from(IndexMap::new())));
        let id = make_id(1);
        let mut prs = IndexMap::new();
        prs.insert(id.clone(), make_pr_custom(&id, PrRole::Mentioned, None, 0));
        app.update(Message::PollResult(payload_from(prs)));
        assert_eq!(app.pending_notifications.len(), 1);
        assert_eq!(app.pending_notifications[0].title, "Mentioned in PR");
    }

    #[test]
    fn disabled_mentioned_suppresses_notification() {
        let mut app = test_app(events_except(NotifyEvent::Mentioned));
        app.update(Message::PollResult(payload_from(IndexMap::new())));
        let id = make_id(1);
        let mut prs = IndexMap::new();
        prs.insert(id.clone(), make_pr_custom(&id, PrRole::Mentioned, None, 0));
        app.update(Message::PollResult(payload_from(prs)));
        assert!(app.pending_notifications.is_empty());
    }

    #[test]
    fn open_selected_mentioned_pr_queues_dismissal_and_removes_from_list() {
        let mut app = test_app(NotifyEvent::all());
        let id_mentioned = make_id(1);
        let id_author = make_id(2);
        let mut prs = IndexMap::new();
        prs.insert(
            id_mentioned.clone(),
            make_pr_custom(&id_mentioned, PrRole::Mentioned, None, 0),
        );
        prs.insert(
            id_author.clone(),
            make_pr_custom(&id_author, PrRole::Author, None, 0),
        );
        app.update(Message::PollResult(payload_from(prs)));
        select_pr(&mut app, &id_mentioned);

        app.update(Message::OpenSelected);

        assert_eq!(app.pending_dismissals, vec![id_mentioned.clone()]);
        assert!(!app.prs.contains_key(&id_mentioned));
        assert_eq!(
            app.selected_row(),
            Some(&VisibleRow::Role(PrRole::Mentioned))
        );
        assert!(app.prs.contains_key(&id_author));
        assert!(app.dirty);
    }

    #[test]
    fn open_selected_last_mentioned_pr_clears_selection() {
        let mut app = test_app(NotifyEvent::all());
        let id = make_id(1);
        let mut prs = IndexMap::new();
        prs.insert(id.clone(), make_pr_custom(&id, PrRole::Mentioned, None, 0));
        app.update(Message::PollResult(payload_from(prs)));
        select_pr(&mut app, &id);

        app.update(Message::OpenSelected);

        assert_eq!(app.pending_dismissals, vec![id]);
        assert!(app.prs.is_empty());
        assert_eq!(
            app.selected_row(),
            Some(&VisibleRow::Role(PrRole::Mentioned))
        );
    }

    #[test]
    fn open_selected_non_mentioned_pr_does_not_queue_dismissal() {
        let mut app = test_app(NotifyEvent::all());
        let id = make_id(1);
        let mut prs = IndexMap::new();
        prs.insert(
            id.clone(),
            make_pr_custom(&id, PrRole::ReviewRequested, None, 0),
        );
        app.update(Message::PollResult(payload_from(prs)));
        select_pr(&mut app, &id);

        app.update(Message::OpenSelected);

        assert!(app.pending_dismissals.is_empty());
        assert!(app.prs.contains_key(&id));
    }

    #[test]
    fn open_selected_clears_new_comment_pr_id() {
        let mut app = test_app(NotifyEvent::all());
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
        select_pr(&mut app, &id);
        // re-add flag manually to simulate state
        app.new_comment_pr_ids.insert(id.clone());
        app.dirty = false;

        app.update(Message::OpenSelected);
        assert!(!app.new_comment_pr_ids.contains(&id));
        assert!(
            app.dirty,
            "clearing the highlight marker must trigger a redraw"
        );
    }

    #[test]
    fn new_comment_pr_ids_pruned_when_pr_removed_from_list() {
        let mut app = test_app(NotifyEvent::all());
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

    // --- Self-comment filter logic ---

    #[test]
    fn self_comment_does_not_trigger_notification() {
        let mut app = test_app(NotifyEvent::all());
        let id = make_id(1);

        // First poll: baseline
        let mut prs = IndexMap::new();
        prs.insert(
            id.clone(),
            make_pr_with_comments(&id, PrRole::Author, None, 0, 0),
        );
        app.update(Message::PollResult(payload_from(prs)));

        // Second poll: comment count increased, but last commenter is self
        let mut prs2 = IndexMap::new();
        let mut pr = make_pr_with_comments(&id, PrRole::Author, None, 100, 3);
        pr.last_commenter = Some("testuser".to_string());
        prs2.insert(id.clone(), pr);
        app.update(Message::PollResult(payload_from(prs2)));

        assert!(
            app.pending_notifications.is_empty(),
            "self comment should not trigger notification"
        );
    }

    #[test]
    fn other_comment_triggers_notification() {
        let mut app = test_app(NotifyEvent::all());
        let id = make_id(1);

        // First poll: baseline
        let mut prs = IndexMap::new();
        prs.insert(
            id.clone(),
            make_pr_with_comments(&id, PrRole::Author, None, 0, 0),
        );
        app.update(Message::PollResult(payload_from(prs)));

        // Second poll: comment count increased, last commenter is someone else
        let mut prs2 = IndexMap::new();
        let mut pr = make_pr_with_comments(&id, PrRole::Author, None, 100, 3);
        pr.last_commenter = Some("otheruser".to_string());
        prs2.insert(id.clone(), pr);
        app.update(Message::PollResult(payload_from(prs2)));

        assert_eq!(app.pending_notifications.len(), 1);
        assert_eq!(app.pending_notifications[0].title, "New comment");
    }

    #[test]
    fn last_commenter_none_triggers_notification() {
        let mut app = test_app(NotifyEvent::all());
        let id = make_id(1);

        // First poll: baseline
        let mut prs = IndexMap::new();
        prs.insert(
            id.clone(),
            make_pr_with_comments(&id, PrRole::Author, None, 0, 0),
        );
        app.update(Message::PollResult(payload_from(prs)));

        // Second poll: comment count increased, last_commenter is None (deleted user)
        let mut prs2 = IndexMap::new();
        prs2.insert(
            id.clone(),
            make_pr_with_comments(&id, PrRole::Author, None, 100, 3),
        );
        app.update(Message::PollResult(payload_from(prs2)));

        assert_eq!(
            app.pending_notifications.len(),
            1,
            "None last_commenter should still notify (safe fallback)"
        );
        assert_eq!(app.pending_notifications[0].title, "New comment");
    }

    // --- Notify event filter ---

    fn events_except(event: NotifyEvent) -> HashSet<NotifyEvent> {
        let mut all = NotifyEvent::all();
        all.remove(&event);
        all
    }

    #[test]
    fn disabled_review_requested_suppresses_notification() {
        let mut app = test_app(events_except(NotifyEvent::ReviewRequested));
        app.update(Message::PollResult(payload_from(IndexMap::new())));
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
    fn disabled_pr_closed_suppresses_notification() {
        let mut app = test_app(events_except(NotifyEvent::PrClosed));
        let id = make_id(1);
        let mut prs = IndexMap::new();
        prs.insert(id.clone(), make_pr_custom(&id, PrRole::Author, None, 0));
        app.update(Message::PollResult(payload_from(prs)));

        let mut prs2 = IndexMap::new();
        prs2.insert(id.clone(), make_closed_pr(&id, PrRole::Author, 100));
        app.update(Message::PollResult(payload_from(prs2)));

        assert!(app.pending_notifications.is_empty());
    }

    #[test]
    fn disabled_pr_merged_suppresses_notification() {
        let mut app = test_app(events_except(NotifyEvent::PrMerged));
        let id = make_id(1);
        let mut prs = IndexMap::new();
        prs.insert(id.clone(), make_pr_custom(&id, PrRole::Author, None, 0));
        app.update(Message::PollResult(payload_from(prs)));

        let mut prs2 = IndexMap::new();
        prs2.insert(id.clone(), make_merged_pr(&id, PrRole::Author, 100));
        app.update(Message::PollResult(payload_from(prs2)));

        assert!(app.pending_notifications.is_empty());
    }

    #[test]
    fn disabled_pr_merged_still_allows_pr_closed() {
        let mut app = test_app(events_except(NotifyEvent::PrMerged));
        let id = make_id(1);
        let mut prs = IndexMap::new();
        prs.insert(id.clone(), make_pr_custom(&id, PrRole::Author, None, 0));
        app.update(Message::PollResult(payload_from(prs)));

        let mut prs2 = IndexMap::new();
        prs2.insert(id.clone(), make_closed_pr(&id, PrRole::Author, 100));
        app.update(Message::PollResult(payload_from(prs2)));

        assert_eq!(app.pending_notifications.len(), 1);
        assert_eq!(app.pending_notifications[0].title, "PR closed");
    }

    #[test]
    fn disabled_re_review_requested_suppresses_notification() {
        let mut app = test_app(events_except(NotifyEvent::ReReviewRequested));
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
    fn disabled_new_comment_suppresses_notification_but_keeps_highlight() {
        let mut app = test_app(events_except(NotifyEvent::NewComment));
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
            make_pr_with_comments(&id, PrRole::Author, None, 100, 3),
        );
        app.update(Message::PollResult(payload_from(prs2)));

        assert!(
            app.pending_notifications.is_empty(),
            "notification should be suppressed"
        );
        assert!(
            app.new_comment_pr_ids.contains(&id),
            "UI highlight should still be set"
        );
    }

    #[test]
    fn empty_events_suppresses_all_notifications() {
        let mut app = test_app(HashSet::new());
        // review requested
        app.update(Message::PollResult(payload_from(IndexMap::new())));
        let id = make_id(1);
        let mut prs = IndexMap::new();
        prs.insert(
            id.clone(),
            make_pr_custom(&id, PrRole::ReviewRequested, None, 0),
        );
        app.update(Message::PollResult(payload_from(prs)));
        assert!(app.pending_notifications.is_empty());
    }

    // --- CI finished notification ---

    fn make_pr_with_ci(
        id: &PrId,
        role: PrRole,
        updated_secs: i64,
        ci_status: Option<CiStatus>,
    ) -> PullRequest {
        PullRequest {
            ci_status,
            ..make_pr_with_comments(id, role, None, updated_secs, 0)
        }
    }

    #[test]
    fn ci_pending_to_success_triggers_notification() {
        let mut app = test_app(NotifyEvent::all());
        let id = make_id(1);
        let mut prs = IndexMap::new();
        prs.insert(
            id.clone(),
            make_pr_with_ci(&id, PrRole::Author, 0, Some(CiStatus::Pending)),
        );
        app.update(Message::PollResult(payload_from(prs)));

        let mut prs2 = IndexMap::new();
        prs2.insert(
            id.clone(),
            make_pr_with_ci(&id, PrRole::Author, 100, Some(CiStatus::Success)),
        );
        app.update(Message::PollResult(payload_from(prs2)));

        assert_eq!(app.pending_notifications.len(), 1);
        assert_eq!(app.pending_notifications[0].title, "CI passed");
    }

    #[test]
    fn ci_pending_to_failure_triggers_notification() {
        let mut app = test_app(NotifyEvent::all());
        let id = make_id(1);
        let mut prs = IndexMap::new();
        prs.insert(
            id.clone(),
            make_pr_with_ci(&id, PrRole::Author, 0, Some(CiStatus::Pending)),
        );
        app.update(Message::PollResult(payload_from(prs)));

        let mut prs2 = IndexMap::new();
        prs2.insert(
            id.clone(),
            make_pr_with_ci(&id, PrRole::Author, 100, Some(CiStatus::Failure)),
        );
        app.update(Message::PollResult(payload_from(prs2)));

        assert_eq!(app.pending_notifications.len(), 1);
        assert_eq!(app.pending_notifications[0].title, "CI failed");
    }

    #[test]
    fn ci_success_to_success_no_notification() {
        let mut app = test_app(NotifyEvent::all());
        let id = make_id(1);
        let mut prs = IndexMap::new();
        prs.insert(
            id.clone(),
            make_pr_with_ci(&id, PrRole::Author, 0, Some(CiStatus::Success)),
        );
        app.update(Message::PollResult(payload_from(prs)));

        let mut prs2 = IndexMap::new();
        prs2.insert(
            id.clone(),
            make_pr_with_ci(&id, PrRole::Author, 100, Some(CiStatus::Success)),
        );
        app.update(Message::PollResult(payload_from(prs2)));

        assert!(app.pending_notifications.is_empty());
    }

    #[test]
    fn ci_none_to_success_no_notification() {
        let mut app = test_app(NotifyEvent::all());
        let id = make_id(1);
        let mut prs = IndexMap::new();
        prs.insert(id.clone(), make_pr_with_ci(&id, PrRole::Author, 0, None));
        app.update(Message::PollResult(payload_from(prs)));

        let mut prs2 = IndexMap::new();
        prs2.insert(
            id.clone(),
            make_pr_with_ci(&id, PrRole::Author, 100, Some(CiStatus::Success)),
        );
        app.update(Message::PollResult(payload_from(prs2)));

        assert!(
            app.pending_notifications.is_empty(),
            "no notification when old ci_status is None"
        );
    }

    #[test]
    fn ci_notification_only_for_author() {
        let mut app = test_app(NotifyEvent::all());
        let id = make_id(1);
        let mut prs = IndexMap::new();
        prs.insert(
            id.clone(),
            make_pr_with_ci(&id, PrRole::ReviewRequested, 0, Some(CiStatus::Pending)),
        );
        app.update(Message::PollResult(payload_from(prs)));

        let mut prs2 = IndexMap::new();
        prs2.insert(
            id.clone(),
            make_pr_with_ci(&id, PrRole::ReviewRequested, 100, Some(CiStatus::Success)),
        );
        app.update(Message::PollResult(payload_from(prs2)));

        assert!(
            app.pending_notifications.is_empty(),
            "reviewer should not get CI notification"
        );
    }

    #[test]
    fn ci_notification_not_for_mentioned_role() {
        let mut app = test_app(NotifyEvent::all());
        let id = make_id(1);
        let mut prs = IndexMap::new();
        prs.insert(
            id.clone(),
            make_pr_with_ci(&id, PrRole::Mentioned, 0, Some(CiStatus::Pending)),
        );
        app.update(Message::PollResult(payload_from(prs)));

        let mut prs2 = IndexMap::new();
        prs2.insert(
            id.clone(),
            make_pr_with_ci(&id, PrRole::Mentioned, 100, Some(CiStatus::Failure)),
        );
        app.update(Message::PollResult(payload_from(prs2)));

        assert!(
            app.pending_notifications.is_empty(),
            "CI notification is author-only"
        );
    }

    #[test]
    fn disabled_ci_finished_suppresses_notification() {
        let mut app = test_app(events_except(NotifyEvent::CiFinished));
        let id = make_id(1);
        let mut prs = IndexMap::new();
        prs.insert(
            id.clone(),
            make_pr_with_ci(&id, PrRole::Author, 0, Some(CiStatus::Pending)),
        );
        app.update(Message::PollResult(payload_from(prs)));

        let mut prs2 = IndexMap::new();
        prs2.insert(
            id.clone(),
            make_pr_with_ci(&id, PrRole::Author, 100, Some(CiStatus::Success)),
        );
        app.update(Message::PollResult(payload_from(prs2)));

        assert!(app.pending_notifications.is_empty());
    }

    #[test]
    fn ci_no_notification_on_first_poll() {
        let mut app = test_app(NotifyEvent::all());
        let id = make_id(1);
        let mut prs = IndexMap::new();
        prs.insert(
            id.clone(),
            make_pr_with_ci(&id, PrRole::Author, 0, Some(CiStatus::Success)),
        );
        app.update(Message::PollResult(payload_from(prs)));

        assert!(
            app.pending_notifications.is_empty(),
            "no CI notification on initial load"
        );
    }
}
