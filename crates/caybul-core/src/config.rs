//! Tiny persisted settings, stored as ~/.caybul.json.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Serialize, Deserialize, Default, Clone)]
pub struct Config {
    /// Where received files are saved. None = Downloads/caybul-inbox.
    #[serde(default)]
    pub dest_dir: Option<PathBuf>,
}

fn path() -> Option<PathBuf> {
    #[allow(deprecated)]
    std::env::home_dir().map(|h| h.join(".caybul.json"))
}

pub fn load() -> Config {
    path()
        .and_then(|p| std::fs::read(p).ok())
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

pub fn save(cfg: &Config) {
    if let (Some(p), Ok(json)) = (path(), serde_json::to_vec_pretty(cfg)) {
        let _ = std::fs::write(p, json);
    }
}
