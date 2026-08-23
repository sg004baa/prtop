use std::collections::HashSet;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::error::AppError;
use crate::types::PrRole;

#[derive(Debug)]
pub struct UiStateStore {
    path: PathBuf,
    collapsed_roles: HashSet<PrRole>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct UiStateFile {
    collapsed_roles: Vec<PersistedRole>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
enum PersistedRole {
    #[serde(rename = "AUTHOR")]
    Author,
    #[serde(rename = "REVIEW")]
    ReviewRequested,
    #[serde(rename = "MENTION")]
    Mentioned,
}

impl From<PrRole> for PersistedRole {
    fn from(role: PrRole) -> Self {
        match role {
            PrRole::Author => Self::Author,
            PrRole::ReviewRequested => Self::ReviewRequested,
            PrRole::Mentioned => Self::Mentioned,
        }
    }
}

impl From<PersistedRole> for PrRole {
    fn from(role: PersistedRole) -> Self {
        match role {
            PersistedRole::Author => Self::Author,
            PersistedRole::ReviewRequested => Self::ReviewRequested,
            PersistedRole::Mentioned => Self::Mentioned,
        }
    }
}

impl UiStateStore {
    pub fn load() -> Result<Self, AppError> {
        let config_dir = dirs::config_dir().ok_or_else(|| {
            AppError::Config(
                "Config directory not found (UI state cannot be persisted)".to_string(),
            )
        })?;
        Self::load_from(config_dir.join("prtop").join("ui-state.json"))
    }

    pub fn load_from(path: PathBuf) -> Result<Self, AppError> {
        let collapsed_roles = match std::fs::read_to_string(&path) {
            Ok(text) => parse_state(&text).map_err(|e| {
                AppError::Config(format!("Failed to parse {}: {e}", path.display()))
            })?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => HashSet::new(),
            Err(e) => return Err(AppError::Io(e)),
        };
        Ok(Self {
            path,
            collapsed_roles,
        })
    }

    pub fn collapsed_roles(&self) -> &HashSet<PrRole> {
        &self.collapsed_roles
    }

    pub fn save(&mut self, collapsed_roles: &HashSet<PrRole>) -> Result<(), AppError> {
        self.collapsed_roles.clone_from(collapsed_roles);
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let collapsed_roles = [PrRole::Author, PrRole::ReviewRequested, PrRole::Mentioned]
            .into_iter()
            .filter(|role| self.collapsed_roles.contains(role))
            .map(PersistedRole::from)
            .collect();
        let text = serde_json::to_string_pretty(&UiStateFile { collapsed_roles })
            .map_err(|e| AppError::Config(format!("Failed to serialize UI state: {e}")))?;
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, text)?;
        std::fs::rename(&tmp, &self.path)?;
        Ok(())
    }
}

fn parse_state(text: &str) -> Result<HashSet<PrRole>, String> {
    let file: UiStateFile = serde_json::from_str(text).map_err(|e| e.to_string())?;
    let mut roles = HashSet::with_capacity(file.collapsed_roles.len());
    for role in file.collapsed_roles {
        if !roles.insert(role.into()) {
            return Err("collapsed_roles contains a duplicate role".to_string());
        }
    }
    Ok(roles)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_state_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "prtop-ui-state-{name}-{}-{}.json",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ))
    }

    #[test]
    fn missing_state_defaults_to_all_expanded() {
        let path = temp_state_path("missing");
        let _ = std::fs::remove_file(&path);
        let store = UiStateStore::load_from(path).unwrap();
        assert!(store.collapsed_roles().is_empty());
    }

    #[test]
    fn collapsed_roles_survive_store_reconstruction() {
        let path = temp_state_path("roundtrip");
        let _ = std::fs::remove_file(&path);
        let mut store = UiStateStore::load_from(path.clone()).unwrap();
        let collapsed = HashSet::from([PrRole::ReviewRequested, PrRole::Mentioned]);
        store.save(&collapsed).unwrap();

        let reloaded = UiStateStore::load_from(path.clone()).unwrap();
        assert_eq!(reloaded.collapsed_roles(), &collapsed);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn corrupt_state_error_includes_path() {
        let path = temp_state_path("corrupt");
        std::fs::write(&path, "not json").unwrap();
        let error = UiStateStore::load_from(path.clone()).unwrap_err();
        assert!(error.to_string().contains(&path.display().to_string()));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn unknown_role_is_invalid() {
        let path = temp_state_path("unknown-role");
        std::fs::write(&path, r#"{"collapsed_roles":["OBSERVER"]}"#).unwrap();
        assert!(UiStateStore::load_from(path.clone()).is_err());
        let _ = std::fs::remove_file(path);
    }
}
