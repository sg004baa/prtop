use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use chrono::{DateTime, Utc};

use crate::error::AppError;
use crate::types::PrId;

/// Mentioned ロールの PR を「ブラウザで開いた」時点で非表示にするための永続ストア。
/// `dismissed.json` は `{"owner/repo#123": "<dismiss 時刻 RFC3339>"}` の形式で、
/// dismiss 後に他人から再メンションされた PR だけをリストに復帰させる判定に使う。
#[derive(Debug)]
pub struct DismissStore {
    path: PathBuf,
    map: HashMap<PrId, DateTime<Utc>>,
    dirty: bool,
}

impl DismissStore {
    pub fn load() -> Result<Self, AppError> {
        let cache_dir = dirs::cache_dir().ok_or_else(|| {
            AppError::Config(
                "Cache directory not found (dismissed mentions cannot be persisted)".to_string(),
            )
        })?;
        Self::load_from(cache_dir.join("prtop").join("dismissed.json"))
    }

    /// パスを指定してロードする。ファイルが無ければ空(初回起動)。
    /// パース失敗は silent fallback せず fail fast する。
    pub fn load_from(path: PathBuf) -> Result<Self, AppError> {
        let map = match std::fs::read_to_string(&path) {
            Ok(text) => parse_store(&text).map_err(|e| {
                AppError::Config(format!(
                    "Failed to parse {}: {e}. Delete the file to recover (dismissed mentions will reappear).",
                    path.display()
                ))
            })?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => HashMap::new(),
            Err(e) => return Err(AppError::Io(e)),
        };
        Ok(Self {
            path,
            map,
            dirty: false,
        })
    }

    /// 親ディレクトリを作成し、テンポラリファイル経由で atomic に書き込む。
    pub fn save(&mut self) -> Result<(), AppError> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut json = serde_json::Map::new();
        for (id, at) in &self.map {
            json.insert(id.to_string(), serde_json::Value::String(at.to_rfc3339()));
        }
        let text = serde_json::to_string_pretty(&serde_json::Value::Object(json))
            .map_err(|e| AppError::Config(format!("Failed to serialize dismiss store: {e}")))?;
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, text)?;
        std::fs::rename(&tmp, &self.path)?;
        self.dirty = false;
        Ok(())
    }

    pub fn dismiss(&mut self, id: PrId, at: DateTime<Utc>) {
        self.map.insert(id, at);
        self.dirty = true;
    }

    pub fn undismiss(&mut self, id: &PrId) {
        if self.map.remove(id).is_some() {
            self.dirty = true;
        }
    }

    /// テスト用途が主(poller は snapshot() 経由で参照する)。
    #[allow(dead_code)]
    pub fn get(&self, id: &PrId) -> Option<DateTime<Utc>> {
        self.map.get(id).copied()
    }

    /// mentions クエリ結果に存在しなくなった PR のエントリを掃除する。
    pub fn retain_ids(&mut self, keep: &HashSet<PrId>) {
        let before = self.map.len();
        self.map.retain(|id, _| keep.contains(id));
        if self.map.len() != before {
            self.dirty = true;
        }
    }

    pub fn snapshot(&self) -> HashMap<PrId, DateTime<Utc>> {
        self.map.clone()
    }

    pub fn dismissed_ids(&self) -> HashSet<PrId> {
        self.map.keys().cloned().collect()
    }

    pub fn is_dirty(&self) -> bool {
        self.dirty
    }
}

fn parse_store(text: &str) -> Result<HashMap<PrId, DateTime<Utc>>, String> {
    let raw: HashMap<String, DateTime<Utc>> =
        serde_json::from_str(text).map_err(|e| e.to_string())?;
    let mut map = HashMap::with_capacity(raw.len());
    for (key, at) in raw {
        map.insert(parse_pr_id(&key)?, at);
    }
    Ok(map)
}

/// `owner/repo#123` (PrId の Display 形式) をパースする。不正キーはエラー。
fn parse_pr_id(key: &str) -> Result<PrId, String> {
    let (repo_part, number) = key
        .rsplit_once('#')
        .ok_or_else(|| format!("invalid PR key (missing '#'): {key:?}"))?;
    let number: u64 = number
        .parse()
        .map_err(|_| format!("invalid PR number in key: {key:?}"))?;
    let (owner, repo) = repo_part
        .split_once('/')
        .ok_or_else(|| format!("invalid PR key (missing '/'): {key:?}"))?;
    if owner.is_empty() || repo.is_empty() {
        return Err(format!("invalid PR key (empty owner or repo): {key:?}"));
    }
    Ok(PrId {
        owner: owner.to_string(),
        repo: repo.to_string(),
        number,
    })
}

/// コメント本文に `@username` メンションが含まれるかを判定する(ASCII case-insensitive)。
/// GitHub の login は `[A-Za-z0-9-]` なので、直前が英数字/`-`/`@` でなく、
/// 直後が英数字/`-` でない位置だけをメンション境界とみなす。
pub fn contains_mention(body: &str, username: &str) -> bool {
    if username.is_empty() {
        return false;
    }
    let bytes = body.as_bytes();
    let uname = username.as_bytes();
    for (i, &b) in bytes.iter().enumerate() {
        if b != b'@' {
            continue;
        }
        let prev_ok = i == 0 || {
            let p = bytes[i - 1];
            !(p.is_ascii_alphanumeric() || p == b'-' || p == b'@')
        };
        if !prev_ok {
            continue;
        }
        let start = i + 1;
        let end = start + uname.len();
        if end > bytes.len() || !bytes[start..end].eq_ignore_ascii_case(uname) {
            continue;
        }
        let next_ok = end == bytes.len() || {
            let n = bytes[end];
            !(n.is_ascii_alphanumeric() || n == b'-')
        };
        if next_ok {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_store_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "prtop-dismiss-test-{name}-{}-{}.json",
            std::process::id(),
            Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ))
    }

    fn make_id(number: u64) -> PrId {
        PrId {
            owner: "org".to_string(),
            repo: "repo".to_string(),
            number,
        }
    }

    // --- load / save ---

    #[test]
    fn load_missing_file_gives_empty_store() {
        let store = DismissStore::load_from(temp_store_path("missing")).unwrap();
        assert!(store.snapshot().is_empty());
        assert!(!store.is_dirty());
    }

    #[test]
    fn save_and_load_roundtrip() {
        let path = temp_store_path("roundtrip");
        let mut store = DismissStore::load_from(path.clone()).unwrap();
        let at = "2026-07-18T00:00:00Z".parse::<DateTime<Utc>>().unwrap();
        store.dismiss(make_id(1), at);
        assert!(store.is_dirty());
        store.save().unwrap();
        assert!(!store.is_dirty());

        let reloaded = DismissStore::load_from(path.clone()).unwrap();
        assert_eq!(reloaded.get(&make_id(1)), Some(at));
        assert_eq!(reloaded.snapshot().len(), 1);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn load_corrupt_json_fails_with_path_in_message() {
        let path = temp_store_path("corrupt");
        std::fs::write(&path, "not json").unwrap();
        let err = DismissStore::load_from(path.clone()).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains(&path.display().to_string()), "message: {msg}");
        assert!(msg.contains("Delete the file"), "message: {msg}");
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn load_invalid_key_fails() {
        let path = temp_store_path("badkey");
        std::fs::write(&path, r#"{"no-hash-here": "2026-07-18T00:00:00Z"}"#).unwrap();
        assert!(DismissStore::load_from(path.clone()).is_err());
        std::fs::remove_file(path).unwrap();
    }

    // --- parse_pr_id ---

    #[test]
    fn parse_pr_id_display_roundtrip() {
        let id = make_id(42);
        assert_eq!(parse_pr_id(&id.to_string()).unwrap(), id);
    }

    #[test]
    fn parse_pr_id_rejects_malformed_keys() {
        assert!(parse_pr_id("org/repo").is_err());
        assert!(parse_pr_id("orgrepo#1").is_err());
        assert!(parse_pr_id("org/repo#abc").is_err());
        assert!(parse_pr_id("/repo#1").is_err());
        assert!(parse_pr_id("org/#1").is_err());
    }

    // --- mutation API ---

    #[test]
    fn undismiss_removes_entry_and_marks_dirty() {
        let mut store = DismissStore::load_from(temp_store_path("undismiss")).unwrap();
        store.dismiss(make_id(1), Utc::now());
        store.undismiss(&make_id(1));
        assert_eq!(store.get(&make_id(1)), None);
        assert!(store.is_dirty());
    }

    #[test]
    fn undismiss_unknown_id_does_not_mark_dirty() {
        let mut store = DismissStore::load_from(temp_store_path("undismiss-noop")).unwrap();
        store.undismiss(&make_id(1));
        assert!(!store.is_dirty());
    }

    #[test]
    fn retain_ids_drops_entries_not_in_keep_set() {
        let mut store = DismissStore::load_from(temp_store_path("retain")).unwrap();
        store.dismiss(make_id(1), Utc::now());
        store.dismiss(make_id(2), Utc::now());
        let keep = HashSet::from([make_id(1)]);
        store.retain_ids(&keep);
        assert!(store.get(&make_id(1)).is_some());
        assert_eq!(store.get(&make_id(2)), None);
    }

    // --- contains_mention ---

    #[test]
    fn mention_simple_hit() {
        assert!(contains_mention("hey @user please look", "user"));
        assert!(contains_mention("@user", "user"));
        assert!(contains_mention("cc: @user.", "user"));
    }

    #[test]
    fn mention_is_ascii_case_insensitive() {
        assert!(contains_mention("(@User)", "user"));
        assert!(contains_mention("@USER!", "user"));
        assert!(contains_mention("@user", "USER"));
    }

    #[test]
    fn mention_longer_login_does_not_hit() {
        assert!(!contains_mention("ping @username2", "username"));
        assert!(!contains_mention("ping @user-name", "user"));
    }

    #[test]
    fn mention_requires_boundary_before_at() {
        assert!(!contains_mention("foo@user", "user"));
        assert!(!contains_mention("a-@user", "user"));
        assert!(!contains_mention("@@user", "user"));
        assert!(contains_mention("(@user)", "user"));
        assert!(contains_mention("日本語で@userに依頼", "user"));
    }

    #[test]
    fn mention_no_hit_cases() {
        assert!(!contains_mention("", "user"));
        assert!(!contains_mention("user without at", "user"));
        assert!(!contains_mention("@", "user"));
        assert!(!contains_mention("@other", "user"));
        assert!(!contains_mention("@user", ""));
    }
}
