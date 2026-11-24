use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum CategoryIcon {
    #[default]
    Home,
    Folder,
    Music,
    Star,
    Mic,
    Gamepad,
}

impl CategoryIcon {
    pub fn all() -> &'static [CategoryIcon] {
        &[
            CategoryIcon::Home,
            CategoryIcon::Folder,
            CategoryIcon::Music,
            CategoryIcon::Star,
            CategoryIcon::Mic,
            CategoryIcon::Gamepad,
        ]
    }

    pub fn label(&self) -> &'static str {
        match self {
            CategoryIcon::Home => "Home",
            CategoryIcon::Folder => "Folder",
            CategoryIcon::Music => "Music",
            CategoryIcon::Star => "Star",
            CategoryIcon::Mic => "Microphone",
            CategoryIcon::Gamepad => "Gaming",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Hotkey {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub super_key: bool,
    pub key: String,
}

impl Hotkey {
    pub fn new(ctrl: bool, alt: bool, shift: bool, super_key: bool, key: impl Into<String>) -> Self {
        Self {
            ctrl,
            alt,
            shift,
            super_key,
            key: key.into(),
        }
    }

    pub fn display(&self) -> String {
        let mut parts = Vec::new();
        if self.ctrl {
            parts.push("Ctrl");
        }
        if self.alt {
            parts.push("Alt");
        }
        if self.shift {
            parts.push("Shift");
        }
        if self.super_key {
            parts.push("Super");
        }
        parts.push(self.key.as_str());
        parts.join("+")
    }

    pub fn parse_str(s: &str) -> Option<Self> {
        let trimmed = s.trim();
        if trimmed.is_empty() {
            return None;
        }
        let mut ctrl = false;
        let mut alt = false;
        let mut shift = false;
        let mut super_key = false;
        let mut main_key: Option<String> = None;

        for part in trimmed.split('+') {
            let p = part.trim();
            match p.to_ascii_lowercase().as_str() {
                "ctrl" | "control" => ctrl = true,
                "alt" => alt = true,
                "shift" => shift = true,
                "super" | "meta" | "win" => super_key = true,
                "" => {}
                _ => main_key = Some(p.to_string()),
            }
        }

        main_key.map(|key| Self {
            ctrl,
            alt,
            shift,
            super_key,
            key,
        })
    }

    pub fn matches(&self, ctrl: bool, alt: bool, shift: bool, super_key: bool, key: &str) -> bool {
        self.ctrl == ctrl
            && self.alt == alt
            && self.shift == shift
            && self.super_key == super_key
            && self.key.eq_ignore_ascii_case(key)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SoundEntry {
    pub id: u64,
    pub path: PathBuf,
    pub tag: String,
    pub duration_secs: Option<f32>,
    pub hotkey: Option<Hotkey>,
    pub play_count: u64,
    #[serde(default)]
    pub norm_gain: Option<f32>,
}

impl SoundEntry {
    pub fn format_duration(secs: Option<f32>) -> String {
        match secs {
            Some(s) if s.is_finite() && s >= 0.0 => {
                let total = s.round() as u64;
                let mins = total / 60;
                let rem = total % 60;
                format!("{mins}:{rem:02}")
            }
            _ => "0:00".to_string(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Category {
    pub id: u64,
    pub name: String,
    #[serde(default)]
    pub icon: CategoryIcon,
    pub sounds: Vec<SoundEntry>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum RecordingSource {
    Microphone,
    #[default]
    SystemAudio,
    Both,
}

pub fn default_recording_dir() -> PathBuf {
    let base = dirs::data_local_dir().unwrap_or_else(|| {
        dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".local")
            .join("share")
    });
    base.join("rust-soundpad").join("recordings")
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    pub categories: Vec<Category>,
    pub selected_category_id: u64,
    pub speakers_enabled: bool,
    pub mic_enabled: bool,
    pub master_volume: f32,
    pub speakers_volume: f32,
    pub mic_volume: f32,
    pub hotkeys_enabled: bool,
    pub auto_normalize: bool,
    pub repeat_enabled: bool,
    pub auto_advance: bool,
    pub dark_mode: bool,
    pub search_visible: bool,
    pub always_on_top: bool,
    pub allow_overlap: bool,
    pub selected_mic_device: Option<String>,
    pub selected_output_device: Option<String>,
    #[serde(alias = "stop_all_hotkey")]
    pub stop_hotkey: Option<Hotkey>,
    pub pause_hotkey: Option<Hotkey>,
    #[serde(default)]
    pub record_hotkey: Option<Hotkey>,
    #[serde(default)]
    pub recording_source: RecordingSource,
    #[serde(default = "default_recording_dir")]
    pub recording_dir: PathBuf,
    pub next_id: u64,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            categories: vec![Category {
                id: 1,
                name: "My Sounds".to_string(),
                icon: CategoryIcon::Home,
                sounds: Vec::new(),
            }],
            selected_category_id: 1,
            speakers_enabled: true,
            mic_enabled: true,
            master_volume: 1.0,
            speakers_volume: 0.85,
            mic_volume: 0.85,
            hotkeys_enabled: true,
            auto_normalize: true,
            repeat_enabled: false,
            auto_advance: false,
            dark_mode: false,
            search_visible: false,
            always_on_top: false,
            allow_overlap: false,
            selected_mic_device: None,
            selected_output_device: None,
            stop_hotkey: Some(Hotkey::new(true, false, false, false, "F12")),
            pause_hotkey: None,
            record_hotkey: None,
            recording_source: RecordingSource::default(),
            recording_dir: default_recording_dir(),
            next_id: 2,
        }
    }
}

impl AppConfig {
    pub fn alloc_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id = self.next_id.saturating_add(1);
        id
    }

    pub fn ensure_valid(&mut self) {
        if self.categories.is_empty() {
            let id = self.alloc_id();
            self.categories.push(Category {
                id,
                name: "My Sounds".to_string(),
                icon: CategoryIcon::Home,
                sounds: Vec::new(),
            });
            self.selected_category_id = id;
        }
        if !self
            .categories
            .iter()
            .any(|c| c.id == self.selected_category_id)
        {
            self.selected_category_id = self.categories[0].id;
        }
        let max_existing = self
            .categories
            .iter()
            .flat_map(|c| std::iter::once(c.id).chain(c.sounds.iter().map(|s| s.id)))
            .max()
            .unwrap_or(1);
        if self.next_id <= max_existing {
            self.next_id = max_existing + 1;
        }
        self.master_volume = self.master_volume.clamp(0.0, 1.5);
        self.speakers_volume = self.speakers_volume.clamp(0.0, 1.5);
        self.mic_volume = self.mic_volume.clamp(0.0, 1.5);
        if self.recording_dir.as_os_str().is_empty() {
            self.recording_dir = default_recording_dir();
        }
    }

    pub fn selected_category(&self) -> Option<&Category> {
        self.categories
            .iter()
            .find(|c| c.id == self.selected_category_id)
            .or_else(|| self.categories.first())
    }

    pub fn selected_category_mut(&mut self) -> Option<&mut Category> {
        let id = self.selected_category_id;
        if let Some(idx) = self.categories.iter().position(|c| c.id == id) {
            Some(&mut self.categories[idx])
        } else {
            self.categories.first_mut()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hotkey_parse_and_match() {
        let hk = Hotkey::parse_str("Ctrl+Shift+F5").unwrap();
        assert_eq!(hk.display(), "Ctrl+Shift+F5");
        assert!(hk.matches(true, false, true, false, "f5"));
        assert!(!hk.matches(true, false, false, false, "F5"));
        assert!(Hotkey::parse_str("   ").is_none());
    }

    #[test]
    fn format_duration_cases() {
        assert_eq!(SoundEntry::format_duration(None), "0:00");
        assert_eq!(SoundEntry::format_duration(Some(2.1)), "0:02");
        assert_eq!(SoundEntry::format_duration(Some(65.0)), "1:05");
    }
}
