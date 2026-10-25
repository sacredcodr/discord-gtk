//! App preferences that are ours rather than Discord's, stored next to the token.

use std::{fs, path::PathBuf, sync::Mutex};

use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct Prefs {
    /// "dark", "light" or "system"
    pub theme: String,
    /// Scale factor for message text, 1.0 is normal.
    pub zoom: f64,
    /// Exact device names; `None` means the system default.
    pub input_device: Option<String>,
    pub output_device: Option<String>,
    /// Microphone level (0..1) that counts as speech.
    pub mic_threshold: f32,
    /// Show a desktop notification for mentions and DMs.
    pub notifications: bool,
    /// Voice volumes, 1.0 is unchanged.
    pub input_volume: f32,
    pub output_volume: f32,
    /// Built-in plugin id -> switched on.
    pub plugins: std::collections::BTreeMap<String, bool>,
}

impl Default for Prefs {
    fn default() -> Self {
        Self {
            theme: "dark".into(),
            zoom: 1.0,
            input_device: None,
            output_device: None,
            mic_threshold: 0.02,
            notifications: true,
            input_volume: 1.0,
            output_volume: 1.0,
            plugins: Default::default(),
        }
    }
}

static CURRENT: Mutex<Option<Prefs>> = Mutex::new(None);

fn path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("discord-gtk").join("settings.json"))
}

/// Current preferences, read from disk on first use.
pub fn get() -> Prefs {
    let mut slot = CURRENT.lock().expect("prefs lock");
    slot.get_or_insert_with(|| {
        path()
            .and_then(|p| fs::read_to_string(p).ok())
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    })
    .clone()
}

/// Changes preferences and writes them out.
pub fn update(change: impl FnOnce(&mut Prefs)) {
    let mut prefs = get();
    change(&mut prefs);
    if let Some(path) = path() {
        if let Some(dir) = path.parent() {
            let _ = fs::create_dir_all(dir);
        }
        if let Ok(text) = serde_json::to_string_pretty(&prefs) {
            let _ = fs::write(path, text);
        }
    }
    *CURRENT.lock().expect("prefs lock") = Some(prefs);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_fields_fall_back_to_defaults() {
        let prefs: Prefs = serde_json::from_str(r#"{"zoom": 1.2}"#).unwrap();
        assert_eq!(prefs.zoom, 1.2);
        assert_eq!(prefs.theme, "dark");
        assert!(prefs.notifications);
    }
}
