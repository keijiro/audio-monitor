//! User-specific selections. Audio running/mute/confirmation state is never saved.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, OpenOptions},
    io::{ErrorKind, Write},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct State {
    pub input: Option<String>,
    pub output: Option<String>,
    pub buffer: u32,
}

impl Default for State {
    fn default() -> Self {
        Self {
            input: None,
            output: None,
            buffer: 128,
        }
    }
}

pub fn path() -> Result<PathBuf> {
    let home = std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .context("HOME is not set")?;
    let home = PathBuf::from(home);
    anyhow::ensure!(home.is_absolute(), "HOME must be an absolute path");
    Ok(home.join("Library/Application Support/audio-monitor/state.json"))
}

pub fn load(path: &Path) -> Result<State> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(State::default()),
        Err(error) => return Err(error).context("Cannot read saved settings"),
    };
    let state: State = serde_json::from_slice(&bytes).context("Invalid saved settings JSON")?;
    anyhow::ensure!(
        matches!(state.buffer, 64 | 128 | 256 | 512),
        "Invalid saved buffer size: {}",
        state.buffer
    );
    Ok(state)
}

pub fn save(path: &Path, state: &State) -> Result<()> {
    let parent = path.parent().context("State path has no parent")?;
    fs::create_dir_all(parent).context("Cannot create settings directory")?;
    // A unique sibling plus rename prevents a partial write from destroying the
    // previous state. Concurrent instances use last successful save wins.
    let nonce = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let temporary = parent.join(format!(".state-{}-{nonce}.tmp", std::process::id()));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)
        .context("Cannot create temporary settings file")?;
    let result = (|| -> Result<()> {
        let mut bytes = serde_json::to_vec_pretty(state)?;
        bytes.push(b'\n');
        file.write_all(&bytes)?;
        file.sync_all()?;
        fs::rename(&temporary, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result.context("Cannot save settings")
}

/// Names are the available identity in CPAL 0.15. Never guess between duplicates.
pub fn selection<'a>(saved: Option<&str>, names: impl Iterator<Item = &'a str>) -> Option<usize> {
    let mut names = names.enumerate();
    let Some(saved) = saved else {
        return names.next().map(|(index, _)| index);
    };
    let mut matches = names.filter(|(_, name)| *name == saved);
    let (index, _) = matches.next()?;
    matches.next().is_none().then_some(index)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restores_by_name_after_reordering_without_guessing() {
        assert_eq!(
            selection(Some("USB"), ["Built-in", "USB"].into_iter()),
            Some(1)
        );
        assert_eq!(
            selection(Some("USB"), ["USB", "Built-in"].into_iter()),
            Some(0)
        );
        assert_eq!(selection(Some("USB"), ["Built-in"].into_iter()), None);
        assert_eq!(selection(Some("USB"), ["USB", "USB"].into_iter()), None);
        assert_eq!(selection(None, ["Built-in"].into_iter()), Some(0));
        assert_eq!(selection(None, [].into_iter()), None);
    }

    #[test]
    fn persists_replaces_and_rejects_invalid_state() -> Result<()> {
        let nonce = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        let directory =
            std::env::temp_dir().join(format!("audio-monitor-{}-{nonce}", std::process::id()));
        let path = directory.join("nested/state.json");
        let result = (|| -> Result<()> {
            assert_eq!(load(&path)?, State::default());
            let mut state = State {
                input: Some("マイク \"USB\"".into()),
                output: Some("Headphones".into()),
                buffer: 64,
            };
            save(&path, &state)?;
            assert_eq!(load(&path)?, state);
            state.buffer = 512;
            save(&path, &state)?;
            assert_eq!(load(&path)?, state);
            assert_eq!(fs::read_dir(path.parent().unwrap())?.count(), 1);
            fs::write(&path, br#"{"buffer": 0}"#)?;
            assert!(load(&path).is_err());
            fs::write(&path, b"broken JSON")?;
            assert!(load(&path).is_err());
            fs::write(&path, b"{}")?;
            assert_eq!(load(&path)?, State::default());
            Ok(())
        })();
        fs::remove_dir_all(directory)?;
        result
    }
}
