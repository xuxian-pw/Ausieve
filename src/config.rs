//! Persistent settings of the tray app (`%APPDATA%\Ausieve\config.json`).

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::procs::normalize_path;

pub const DEFAULT_OUTPUT: &str = "Line 1 (Virtual Audio Cable)";

/// Programs listed (not selected) in a freshly created config. Discord moves to a new
/// `app-<version>` folder on every update, hence the pattern.
pub const PRESET_PROGRAMS: &[&str] = &[r"%LOCALAPPDATA%\Discord\app-*\Discord.exe"];

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Playback device that receives the captured audio.
    pub output: String,
    /// Programs the user added (full exe paths), in the order they were added.
    pub programs: Vec<String>,
    /// The program currently excluded (one of `programs`), or `None` = exclude nothing.
    pub selected: Option<String>,
    /// Jitter-buffer level in ms: lower = less latency, higher = more robust. Hand-edited
    /// only (not in the menu); absent = [`crate::engine::DEFAULT_BUFFER_MS`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub buffer_ms: Option<u32>,
    /// Write a faint +-1 LSB noise instead of exact digital zeros during silence (keeps
    /// receivers with a silence gate, e.g. RustDesk's encoder, from pausing). Hand-edited
    /// only; written to the file only when on.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub silence_dither: bool,
}

impl Default for Config {
    fn default() -> Self {
        Config { output: DEFAULT_OUTPUT.into(), programs: Vec::new(), selected: None, buffer_ms: None, silence_dither: false }
    }
}

impl Config {
    /// The jitter-buffer level to use: the configured one, or the default.
    pub fn buffer_ms(&self) -> u32 {
        self.buffer_ms.unwrap_or(crate::engine::DEFAULT_BUFFER_MS)
    }

    /// `%APPDATA%\Ausieve\config.json`
    pub fn default_path() -> PathBuf {
        let base = std::env::var_os("APPDATA").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."));
        base.join("Ausieve").join("config.json")
    }

    /// Before the rename the settings lived in `%APPDATA%\audio-exclude\config.json`;
    /// copies that file to `path` once if `path` does not exist yet.
    pub fn migrate_legacy(path: &Path) {
        let Some(base) = std::env::var_os("APPDATA").map(PathBuf::from) else { return };
        let old = base.join("audio-exclude").join("config.json");
        if path.exists() || !old.exists() {
            return;
        }
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::copy(&old, path);
    }

    /// The settings of a first run: the defaults plus [`PRESET_PROGRAMS`].
    pub fn first_run() -> Config {
        Config { programs: PRESET_PROGRAMS.iter().map(|s| s.to_string()).collect(), ..Config::default() }
    }

    /// Loads the config; a missing file gives [`Config::first_run`].
    pub fn load(path: &Path) -> Result<Config, String> {
        match std::fs::read_to_string(path) {
            Ok(s) => {
                let mut c: Config = serde_json::from_str(&s).map_err(|e| format!("{}: {e}", path.display()))?;
                c.fix_up();
                Ok(c)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::first_run()),
            Err(e) => Err(format!("{}: {e}", path.display())),
        }
    }

    /// Saves atomically (write a temp file, then rename over the old one).
    pub fn save(&self, path: &Path) -> Result<(), String> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        }
        let tmp = path.with_extension("json.tmp");
        let s = serde_json::to_string_pretty(self).map_err(|e| e.to_string())?;
        std::fs::write(&tmp, s).map_err(|e| format!("{}: {e}", tmp.display()))?;
        std::fs::rename(&tmp, path).map_err(|e| format!("{}: {e}", path.display()))
    }

    /// Adds a program unless an equal path is already listed. Returns the stored path.
    pub fn add_program(&mut self, path: &str) -> String {
        if let Some(p) = self.find(path) {
            return p.clone();
        }
        self.programs.push(path.to_string());
        path.to_string()
    }

    /// Removes a program; if it was selected, the selection becomes "exclude nothing".
    pub fn remove_program(&mut self, path: &str) {
        let n = normalize_path(path);
        self.programs.retain(|p| normalize_path(p) != n);
        if self.selected.as_deref().is_some_and(|s| normalize_path(s) == n) {
            self.selected = None;
        }
    }

    /// Selects a listed program (or `None`). Unknown paths are ignored.
    pub fn select(&mut self, path: Option<&str>) {
        self.selected = match path {
            None => None,
            Some(p) => self.find(p).cloned().or(self.selected.take()),
        };
    }

    fn find(&self, path: &str) -> Option<&String> {
        let n = normalize_path(path);
        self.programs.iter().find(|p| normalize_path(p) == n)
    }

    /// Repairs hand-edited files: drops duplicates and a selection that isn't listed.
    fn fix_up(&mut self) {
        let mut seen = std::collections::HashSet::new();
        self.programs.retain(|p| seen.insert(normalize_path(p)));
        if let Some(s) = self.selected.clone() {
            self.selected = self.find(&s).cloned();
        }
        if self.output.trim().is_empty() {
            self.output = DEFAULT_OUTPUT.into();
        }
        self.buffer_ms = self.buffer_ms.map(|b| b.clamp(crate::engine::MIN_BUFFER_MS, crate::engine::MAX_BUFFER_MS));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("ausieve-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d.join("sub").join("config.json")
    }

    #[test]
    fn missing_file_gives_defaults_and_roundtrip_works() {
        let p = tmp("roundtrip");
        let first = Config::load(&p).unwrap();
        assert_eq!(first, Config::first_run());
        assert_eq!(first.programs, PRESET_PROGRAMS, "preset listed");
        assert_eq!(first.selected, None, "preset not selected");
        let mut c = Config::default();
        c.add_program(r"C:\Apps\Discord\Discord.exe");
        c.add_program(r"D:\Games\Voice\voice.exe");
        c.select(Some(r"c:/apps/discord/discord.exe"));
        c.output = "CABLE Input (VB-Audio Virtual Cable)".into();
        c.silence_dither = true;
        c.save(&p).unwrap();
        let back = Config::load(&p).unwrap();
        assert_eq!(back, c);
        let text = std::fs::read_to_string(&p).unwrap();
        assert!(!text.contains("buffer_ms"), "unset buffer is not written: {text}");
        Config::default().save(&p).unwrap();
        let text = std::fs::read_to_string(&p).unwrap();
        assert!(!text.contains("silence_dither"), "dither off is not written: {text}");
        assert_eq!(back.selected.as_deref(), Some(r"C:\Apps\Discord\Discord.exe"));
        let _ = std::fs::remove_dir_all(p.parent().unwrap().parent().unwrap());
    }

    #[test]
    fn add_remove_select() {
        let mut c = Config::default();
        c.add_program(r"C:\A\a.exe");
        c.add_program(r"c:\a\A.EXE"); // same program
        c.add_program(r"C:\B\a.exe"); // same name, different program
        assert_eq!(c.programs.len(), 2);
        c.select(Some(r"C:\B\a.exe"));
        c.select(Some(r"C:\not\listed.exe")); // ignored
        assert_eq!(c.selected.as_deref(), Some(r"C:\B\a.exe"));
        c.remove_program(r"C:\A\a.exe");
        assert_eq!(c.selected.as_deref(), Some(r"C:\B\a.exe"));
        c.remove_program(r"c:/b/a.exe");
        assert_eq!(c.selected, None);
        assert!(c.programs.is_empty());
    }

    #[test]
    fn hand_edited_files_are_repaired_or_rejected() {
        let p = tmp("repair");
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, r#"{"programs":["C:\\x.exe","c:\\X.exe"],"selected":"C:\\gone.exe","output":""}"#).unwrap();
        let c = Config::load(&p).unwrap();
        assert_eq!(c.programs, vec![r"C:\x.exe".to_string()]);
        assert_eq!(c.selected, None);
        assert_eq!(c.output, DEFAULT_OUTPUT);
        assert_eq!(c.buffer_ms(), crate::engine::DEFAULT_BUFFER_MS, "missing field gets the default");
        assert!(!c.silence_dither, "dither is off by default");
        std::fs::write(&p, r#"{"buffer_ms":1}"#).unwrap();
        assert_eq!(Config::load(&p).unwrap().buffer_ms(), crate::engine::MIN_BUFFER_MS, "clamped");
        std::fs::write(&p, r#"{"buffer_ms":15}"#).unwrap();
        assert_eq!(Config::load(&p).unwrap().buffer_ms(), 15, "hand-set value is used");
        std::fs::write(&p, "not json").unwrap();
        assert!(Config::load(&p).is_err());
        let _ = std::fs::remove_dir_all(p.parent().unwrap().parent().unwrap());
    }
}
