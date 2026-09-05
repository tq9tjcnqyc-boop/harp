use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use directories::ProjectDirs; 
use serde::{Deserialize, Serialize};

use crate::model::PlayMode;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SavedState {
    pub version: u32, 
    pub source: String, 
    pub track: String, 
    pub position_ms: u64, 
    pub volume: f32, 
    pub mode: PlayMode, 
}

impl Default for SavedState {
    fn default() -> Self {
        Self {
            version: 1,
            source: String::new(), 
            track: String::new(),
            position_ms: 0,
            volume: 0.7, 
            mode: PlayMode::Sequential, 
        }
    }
}

pub fn state_path() -> Result<PathBuf> {
    let dirs = ProjectDirs::from("dev", "Harp", "Harp").context("无法确定系统应用数据目录")?;

    Ok(dirs.data_local_dir().join("state.json"))
}

pub fn load(path: &Path) -> SavedState {
    let Ok(content) = fs::read(path) else {
        return SavedState::default(); 
    };

    match serde_json::from_slice(&content) {
        Ok(state) => state, 
        Err(_) => {
            let backup = path.with_extension("json.bak");
            let _ = fs::rename(path, backup); 
            SavedState::default()
        }
    }
}

pub fn save_atomic(path: &Path, state: &SavedState) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?; 
    }
    let temp = path.with_extension("json.tmp"); 
    let data = serde_json::to_vec_pretty(state)?; 

    {
        let mut file = fs::File::create(&temp)?; 
        file.write_all(&data)?; 
        file.sync_all()?; 
    }

    if path.exists() {
        fs::remove_file(path)?;
    }
    fs::rename(temp, path)?; 
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*; 

    #[test]
    fn state_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");

        let expected = SavedState {
            source: "music".into(), 
            track: "song.flac".into(),
            position_ms: 42_000, 
            ..SavedState::default()
        };
        save_atomic(&path, &expected).unwrap(); 
        let actual = load(&path);
        assert_eq!(actual.position_ms, 42_000); 
        assert_eq!(actual.track, "song.flac");
    }

    #[test]
    fn corrupt_state_is_backed_up() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        fs::write(&path, "{bad").unwrap(); 
        let state = load(&path);
        assert_eq!(state.volume, 0.7); 
        assert!(path.with_extension("json.bak").exists()); 
    }
}
