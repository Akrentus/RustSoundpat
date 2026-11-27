use crate::audio::{probe_audio_duration, AudioEngine, PlaybackStatus};
use crate::config::{export_spl, import_spl, save_config};
use crate::editor::{EditorAction, SoundEditor};
use crate::hotkeys::{egui_key_to_name, HotkeyAction, HotkeyManager};
use crate::icons;
use crate::model::{AppConfig, Category, CategoryIcon, Hotkey, RecordingSource, SoundEntry};
use crate::pipewire::{AudioDeviceInfo, PipewireVirtualMic};
use crate::recorder::AudioRecorder;
use egui::{
    pos2, vec2, Align, Color32, Context, CornerRadius, FontId, Frame, Id, Key, Margin, Rect,
    Response, RichText, ScrollArea, Sense, Stroke, Ui, Vec2, ViewportCommand,
};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;

#[derive(Debug, Clone)]
struct ThemePalette {
    toolbar_bg: Color32,
    content_bg: Color32,
    border: Color32,
    accent: Color32,
    accent_hover: Color32,
    text: Color32,
    muted_text: Color32,
    inactive_icon: Color32,
    row_hover: Color32,
    selected_text: Color32,
}

impl ThemePalette {
    fn from_dark(dark: bool) -> Self {
        if dark {
            Self {
                toolbar_bg: Color32::from_rgb(43, 43, 43),
                content_bg: Color32::from_rgb(30, 30, 30),
                border: Color32::from_rgb(64, 64, 64),
                accent: Color32::from_rgb(30, 136, 229),
                accent_hover: Color32::from_rgb(21, 101, 192),
                text: Color32::from_rgb(235, 235, 235),
                muted_text: Color32::from_rgb(165, 165, 165),
                inactive_icon: Color32::from_rgb(130, 130, 130),
                row_hover: Color32::from_rgb(45, 52, 64),
                selected_text: Color32::WHITE,
            }
        } else {
            Self {
                toolbar_bg: Color32::from_rgb(240, 240, 240),   // #F0F0F0
                content_bg: Color32::from_rgb(255, 255, 255),   // #FFFFFF
                border: Color32::from_rgb(208, 208, 208),       // #D0D0D0
                accent: Color32::from_rgb(21, 101, 192),        // #1565C0
                accent_hover: Color32::from_rgb(30, 136, 229),  // #1E88E5
                text: Color32::from_rgb(30, 30, 30),
                muted_text: Color32::from_rgb(100, 100, 100),
                inactive_icon: Color32::from_rgb(145, 145, 145),
                row_hover: Color32::from_rgb(232, 241, 252),
                selected_text: Color32::WHITE,                  // #FFFFFF
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PreferencesTab {
    Audio,
    Hotkeys,
    Devices,
    Recording,
}

impl PreferencesTab {
    fn all() -> &'static [PreferencesTab] {
        &[
            PreferencesTab::Audio,
            PreferencesTab::Hotkeys,
            PreferencesTab::Devices,
            PreferencesTab::Recording,
        ]
    }

    fn label(&self) -> &'static str {
        match self {
            PreferencesTab::Audio => "Audio",
            PreferencesTab::Hotkeys => "Hotkeys",
            PreferencesTab::Devices => "Devices",
            PreferencesTab::Recording => "Recording",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HotkeyCaptureTarget {
    StopAll,
    PauseResume,
    RecordToggle,
}

#[derive(Debug, Clone)]
enum ModalState {
    None,
    Preferences {
        tab: PreferencesTab,
        mics: Vec<AudioDeviceInfo>,
        cpal_outputs: Vec<String>,
        capturing: Option<HotkeyCaptureTarget>,
    },
    SetHotkey {
        sound_id: u64,
        sound_tag: String,
    },
    RenameSound {
        sound_id: u64,
        text: String,
    },
    NewCategory {
        text: String,
        icon: CategoryIcon,
    },
    RenameCategory {
        category_id: u64,
        text: String,
        icon: CategoryIcon,
    },
    HotkeyList,
    About,
}

#[derive(Default)]
struct BgHotkeyState {
    selected_or_first_sound: Option<(u64, PathBuf)>,
    all_sounds: Vec<(u64, PathBuf)>,
    allow_overlap: bool,
    played_counts: Vec<u64>,
}

pub struct SoundpadApp {
    pub config: AppConfig,
    pub audio: AudioEngine,
    pub pipewire: PipewireVirtualMic,
    pub hotkeys: HotkeyManager,
    pub recorder: AudioRecorder,
    selected_sound_ids: HashSet<u64>,
    last_selected_sound_id: Option<u64>,
    search_query: String,
    modal: ModalState,
    editor: Option<SoundEditor>,
    show_mic_vol_popup: bool,
    dismiss_perm_banner: bool,
    status_toast: Option<String>,
    dragging_sound_ids: Option<Vec<u64>>,
    seek_drag_value: Option<f32>,
    scroll_to_sound_id: Option<u64>,
    duration_tx: mpsc::Sender<(u64, PathBuf)>,
    duration_rx: mpsc::Receiver<(u64, f32)>,
    dirty_config: bool,
    last_always_on_top: bool,
    hotkey_bindings_dirty: bool,
    bg_hotkey_state: Arc<Mutex<BgHotkeyState>>,
}

impl SoundpadApp {
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        mut config: AppConfig,
        mut pipewire: PipewireVirtualMic,
    ) -> Self {
        config.ensure_valid();
        let audio = AudioEngine::new(config.selected_output_device.as_deref());
        audio.sync_settings(
            config.speakers_enabled,
            config.mic_enabled,
            config.master_volume,
            config.speakers_volume,
            config.mic_volume,
            config.auto_normalize,
            config.repeat_enabled,
            config.auto_advance,
        );

        if let Err(e) = pipewire.setup(config.selected_mic_device.as_deref()) {
            pipewire.status_message = format!("PipeWire setup error: {e}");
        }

        let bg_hotkey_state = Arc::new(Mutex::new(BgHotkeyState::default()));
        let bg_state_cb = Arc::clone(&bg_hotkey_state);
        let audio_handle = audio.clone_handle();
        let hotkeys = HotkeyManager::start(
            Some(cc.egui_ctx.clone()),
            Some(Arc::new(move |act: &HotkeyAction| -> bool {
                match act {
                    HotkeyAction::StopAll => {
                        audio_handle.stop_all();
                        true
                    }
                    HotkeyAction::PlaySound(id) => {
                        if let Ok(mut st) = bg_state_cb.lock() {
                            if let Some((_, path)) = st.all_sounds.iter().find(|(sid, _)| sid == id).cloned() {
                                audio_handle.play(*id, path, st.allow_overlap);
                                st.played_counts.push(*id);
                                return true;
                            }
                        }
                        false
                    }
                    HotkeyAction::TogglePause => {
                        let status = audio_handle.status();
                        let target = bg_state_cb.lock().ok().and_then(|st| {
                            st.selected_or_first_sound.clone().map(|(id, p)| (id, p, st.allow_overlap))
                        });
                        if let Some((sel_id, sel_path, allow_overlap)) = target {
                            if status.current_sound_id == Some(sel_id) {
                                if status.is_playing {
                                    audio_handle.set_paused(true);
                                } else if status.is_paused {
                                    audio_handle.set_paused(false);
                                } else {
                                    audio_handle.play(sel_id, sel_path, allow_overlap);
                                    if let Ok(mut st) = bg_state_cb.lock() {
                                        st.played_counts.push(sel_id);
                                    }
                                }
                            } else if status.is_playing || status.is_paused {
                                audio_handle.toggle_pause();
                            } else {
                                audio_handle.play(sel_id, sel_path, allow_overlap);
                                if let Ok(mut st) = bg_state_cb.lock() {
                                    st.played_counts.push(sel_id);
                                }
                            }
                            true
                        } else if status.is_playing || status.is_paused {
                            audio_handle.toggle_pause();
                            true
                        } else {
                            false
                        }
                    }
                    _ => false,
                }
            })),
        );

        let (req_tx, req_rx) = mpsc::channel::<(u64, PathBuf)>();
        let (res_tx, res_rx) = mpsc::channel::<(u64, f32)>();
        let ctx_bg = cc.egui_ctx.clone();
        thread::spawn(move || {
            while let Ok((id, path)) = req_rx.recv() {
                if let Some(dur) = probe_audio_duration(&path) {
                    let _ = res_tx.send((id, dur));
                    ctx_bg.request_repaint();
                }
            }
        });

        // Probe any existing sounds that have unknown duration
        for cat in &config.categories {
            for s in &cat.sounds {
                if s.duration_secs.is_none() {
                    let _ = req_tx.send((s.id, s.path.clone()));
                }
            }
        }

        let always_on_top = config.always_on_top;
        if always_on_top {
            cc.egui_ctx
                .send_viewport_cmd(ViewportCommand::WindowLevel(egui::WindowLevel::AlwaysOnTop));
        }

        let mut app = Self {
            config,
            audio,
            pipewire,
            hotkeys,
            recorder: AudioRecorder::new(),
            selected_sound_ids: HashSet::new(),
            last_selected_sound_id: None,
            search_query: String::new(),
            modal: ModalState::None,
            editor: None,
            show_mic_vol_popup: false,
            dismiss_perm_banner: false,
            status_toast: None,
            dragging_sound_ids: None,
            seek_drag_value: None,
            scroll_to_sound_id: None,
            duration_tx: req_tx,
            duration_rx: res_rx,
            dirty_config: false,
            last_always_on_top: always_on_top,
            hotkey_bindings_dirty: true,
            bg_hotkey_state,
        };
        app.sync_hotkey_bindings();
        app
    }

    fn sync_bg_hotkey_state(&self) {
        if let Ok(mut st) = self.bg_hotkey_state.lock() {
            st.allow_overlap = self.config.allow_overlap;
            st.all_sounds.clear();
            for cat in &self.config.categories {
                for s in &cat.sounds {
                    st.all_sounds.push((s.id, s.path.clone()));
                }
            }
            let target_id = self.last_selected_sound_id.or_else(|| {
                self.config
                    .selected_category()
                    .and_then(|c| c.sounds.first())
                    .map(|s| s.id)
            });
            st.selected_or_first_sound = target_id
                .and_then(|id| self.find_sound(id).map(|s| (s.id, s.path.clone())));
        }
    }

    fn sync_hotkey_bindings(&mut self) {
        let capture = matches!(
            self.modal,
            ModalState::SetHotkey { .. }
                | ModalState::Preferences {
                    capturing: Some(_),
                    ..
                }
        );
        let mut sounds = Vec::new();
        for cat in &self.config.categories {
            for s in &cat.sounds {
                if let Some(ref hk) = s.hotkey {
                    sounds.push((s.id, hk.clone()));
                }
            }
        }
        self.hotkeys.update_bindings(
            self.config.hotkeys_enabled,
            self.config.stop_hotkey.clone(),
            self.config.pause_hotkey.clone(),
            self.config.record_hotkey.clone(),
            sounds,
            capture,
        );
        self.hotkey_bindings_dirty = false;
    }

    fn mark_dirty(&mut self) {
        self.dirty_config = true;
        self.hotkey_bindings_dirty = true;
    }

    fn find_sound(&self, sound_id: u64) -> Option<&SoundEntry> {
        for cat in &self.config.categories {
            if let Some(s) = cat.sounds.iter().find(|s| s.id == sound_id) {
                return Some(s);
            }
        }
        None
    }

    fn find_sound_mut(&mut self, sound_id: u64) -> Option<&mut SoundEntry> {
        for cat in &mut self.config.categories {
            if let Some(s) = cat.sounds.iter_mut().find(|s| s.id == sound_id) {
                return Some(s);
            }
        }
        None
    }

    fn open_sound_editor(&mut self, sound_id: u64) {
        let Some((tag, path)) = self
            .find_sound(sound_id)
            .map(|s| (s.tag.clone(), s.path.clone()))
        else {
            return;
        };
        match SoundEditor::open(
            sound_id,
            tag,
            path,
            self.config.selected_output_device.as_deref(),
        ) {
            Ok(ed) => {
                self.editor = Some(ed);
                self.status_toast = None;
            }
            Err(e) => {
                self.status_toast = Some(format!("Failed to open Sound Editor: {e}"));
            }
        }
    }

    fn play_sound_by_id(&mut self, sound_id: u64) {
        let mut found: Option<PathBuf> = None;
        for cat in &mut self.config.categories {
            if let Some(s) = cat.sounds.iter_mut().find(|s| s.id == sound_id) {
                s.play_count = s.play_count.saturating_add(1);
                found = Some(s.path.clone());
                break;
            }
        }
        if let Some(path) = found {
            self.audio.play(sound_id, path, self.config.allow_overlap);
            self.selected_sound_ids.clear();
            self.selected_sound_ids.insert(sound_id);
            self.last_selected_sound_id = Some(sound_id);
            self.dirty_config = true;
        }
    }

    fn toggle_play_selected_or_first(&mut self) {
        let target_id = self.last_selected_sound_id.or_else(|| {
            self.config
                .selected_category()
                .and_then(|c| c.sounds.first())
                .map(|s| s.id)
        });
        let Some(sound_id) = target_id else {
            return;
        };
        let status = self.audio.status();
        if status.current_sound_id == Some(sound_id) {
            if status.is_playing {
                self.audio.set_paused(true);
            } else if status.is_paused {
                self.audio.set_paused(false);
            } else {
                self.play_sound_by_id(sound_id);
            }
        } else {
            self.play_sound_by_id(sound_id);
        }
    }

    fn play_selected_or_first(&mut self) {
        if let Some(id) = self.last_selected_sound_id {
            self.play_sound_by_id(id);
            return;
        }
        if let Some(first_id) = self
            .config
            .selected_category()
            .and_then(|c| c.sounds.first())
            .map(|s| s.id)
        {
            self.play_sound_by_id(first_id);
        }
    }

    fn visible_sound_ids(&self) -> Vec<u64> {
        let query = self.search_query.trim().to_ascii_lowercase();
        self.config
            .selected_category()
            .map(|c| {
                c.sounds
                    .iter()
                    .filter(|s| {
                        if query.is_empty() {
                            true
                        } else {
                            let hk = s
                                .hotkey
                                .as_ref()
                                .map(|h| h.display().to_ascii_lowercase())
                                .unwrap_or_default();
                            s.tag.to_ascii_lowercase().contains(&query) || hk.contains(&query)
                        }
                    })
                    .map(|s| s.id)
                    .collect()
            })
            .unwrap_or_default()
    }

    fn move_selection(&mut self, delta: isize) {
        let ids = self.visible_sound_ids();
        if ids.is_empty() {
            return;
        }
        let target_idx = match self
            .last_selected_sound_id
            .and_then(|cur| ids.iter().position(|&id| id == cur))
        {
            Some(idx) => {
                let next = (idx as isize + delta).clamp(0, (ids.len() - 1) as isize);
                next as usize
            }
            None => {
                if delta < 0 {
                    ids.len() - 1
                } else {
                    0
                }
            }
        };
        let new_id = ids[target_idx];
        self.selected_sound_ids.clear();
        self.selected_sound_ids.insert(new_id);
        self.last_selected_sound_id = Some(new_id);
        self.scroll_to_sound_id = Some(new_id);
    }

    fn play_relative(&mut self, delta: isize) {
        let ids: Vec<u64> = self
            .config
            .selected_category()
            .map(|c| c.sounds.iter().map(|s| s.id).collect())
            .unwrap_or_default();
        if ids.is_empty() {
            return;
        }
        let current = self
            .audio
            .status()
            .current_sound_id
            .or(self.last_selected_sound_id);
        let next_idx = match current.and_then(|cid| ids.iter().position(|&id| id == cid)) {
            Some(idx) => {
                let len = ids.len() as isize;
                ((idx as isize + delta).rem_euclid(len)) as usize
            }
            None => 0,
        };
        self.play_sound_by_id(ids[next_idx]);
    }

    fn add_sound_paths(&mut self, paths: Vec<PathBuf>) {
        let mut added = Vec::new();
        for path in paths {
            if !is_supported_audio(&path) {
                continue;
            }
            let id = self.config.alloc_id();
            let tag = path
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| "Sound".to_string());
            let entry = SoundEntry {
                id,
                path: path.clone(),
                tag,
                duration_secs: None,
                hotkey: None,
                play_count: 0,
                norm_gain: None,
            };
            added.push((id, path, entry));
        }

        if added.is_empty() {
            return;
        }

        if let Some(cat) = self.config.selected_category_mut() {
            for (id, path, entry) in added {
                cat.sounds.push(entry);
                let _ = self.duration_tx.send((id, path));
            }
        }
        self.mark_dirty();
    }

    fn toggle_recording(&mut self) {
        if self.recorder.is_recording() {
            self.stop_recording_if_active();
        } else {
            match self.recorder.start(
                self.config.recording_source,
                self.config.selected_mic_device.as_deref(),
                &self.config.recording_dir,
            ) {
                Ok(()) => {
                    self.status_toast = None;
                }
                Err(e) => {
                    self.status_toast = Some(format!("Recording error: {e}"));
                }
            }
        }
    }

    fn stop_recording_if_active(&mut self) {
        if !self.recorder.is_recording() {
            return;
        }
        match self.recorder.stop() {
            Ok(Some(path)) => self.finish_recorded_file(path),
            Ok(None) => {}
            Err(e) => {
                self.status_toast = Some(format!("Recording error: {e}"));
            }
        }
    }

    fn finish_recorded_file(&mut self, path: PathBuf) {
        self.add_sound_paths(vec![path.clone()]);
        if let Some(last_id) = self
            .config
            .selected_category()
            .and_then(|c| c.sounds.last())
            .map(|s| s.id)
        {
            self.selected_sound_ids.clear();
            self.selected_sound_ids.insert(last_id);
            self.last_selected_sound_id = Some(last_id);
            self.scroll_to_sound_id = Some(last_id);
        }
        match save_config(&self.config) {
            Ok(()) => {
                self.dirty_config = false;
                let name = path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "recording.wav".to_string());
                self.status_toast = Some(format!("Saved recording: {name}"));
            }
            Err(e) => {
                self.status_toast = Some(e);
            }
        }
    }

    fn add_files_dialog(&mut self) {
        if let Some(files) = rfd::FileDialog::new()
            .set_title("Add Sound Files")
            .add_filter("Audio & Video Files", SUPPORTED_MEDIA_EXTENSIONS)
            .pick_files()
        {
            self.add_sound_paths(files);
        }
    }

    fn add_folder_dialog(&mut self) {
        if let Some(folder) = rfd::FileDialog::new().set_title("Add Folder").pick_folder() {
            let mut files = Vec::new();
            collect_audio_files_recursive(&folder, &mut files);
            files.sort();
            self.add_sound_paths(files);
        }
    }

    fn import_spl_dialog(&mut self) {
        if let Some(file) = rfd::FileDialog::new()
            .set_title("Import Soundlist (.spl)")
            .add_filter("Soundpad Soundlist", &["spl", "xml"])
            .pick_file()
        {
            match import_spl(&file, &mut self.config) {
                Ok(count) => {
                    for cat in &self.config.categories {
                        for s in &cat.sounds {
                            if s.duration_secs.is_none() {
                                let _ = self.duration_tx.send((s.id, s.path.clone()));
                            }
                        }
                    }
                    self.status_toast = Some(format!("Imported {count} sound(s)"));
                    self.mark_dirty();
                }
                Err(e) => {
                    self.status_toast = Some(format!("Import error: {e}"));
                }
            }
        }
    }

    fn export_spl_dialog(&mut self) {
        if let Some(file) = rfd::FileDialog::new()
            .set_title("Export Soundlist (.spl)")
            .set_file_name("soundlist.spl")
            .add_filter("Soundpad Soundlist", &["spl"])
            .save_file()
        {
            match export_spl(&file, &self.config) {
                Ok(()) => self.status_toast = Some("Soundlist exported".to_string()),
                Err(e) => self.status_toast = Some(format!("Export error: {e}")),
            }
        }
    }

    fn remove_selected_sounds(&mut self) {
        if self.selected_sound_ids.is_empty() {
            return;
        }
        let to_remove = self.selected_sound_ids.clone();
        if let Some(cat) = self.config.selected_category_mut() {
            for s in &cat.sounds {
                if to_remove.contains(&s.id) {
                    self.audio.clear_cache_for(&s.path);
                }
            }
            cat.sounds.retain(|s| !to_remove.contains(&s.id));
        }
        self.selected_sound_ids.clear();
        self.last_selected_sound_id = None;
        self.mark_dirty();
    }

    fn move_sounds_to_category(&mut self, sound_ids: &[u64], target_cat_id: u64) {
        if target_cat_id == self.config.selected_category_id || sound_ids.is_empty() {
            return;
        }
        let id_set: HashSet<u64> = sound_ids.iter().copied().collect();
        let mut moved = Vec::new();
        for cat in &mut self.config.categories {
            if cat.id == target_cat_id {
                continue;
            }
            let mut remaining = Vec::with_capacity(cat.sounds.len());
            for s in cat.sounds.drain(..) {
                if id_set.contains(&s.id) {
                    moved.push(s);
                } else {
                    remaining.push(s);
                }
            }
            cat.sounds = remaining;
        }
        if !moved.is_empty() {
            if let Some(target) = self
                .config
                .categories
                .iter_mut()
                .find(|c| c.id == target_cat_id)
            {
                target.sounds.extend(moved);
            }
            self.selected_sound_ids.clear();
            self.last_selected_sound_id = None;
            self.mark_dirty();
        }
    }

    fn open_preferences(&mut self) {
        let mics = PipewireVirtualMic::list_physical_mics();
        let cpal_outputs = AudioEngine::list_cpal_output_devices();
        self.modal = ModalState::Preferences {
            tab: PreferencesTab::Audio,
            mics,
            cpal_outputs,
            capturing: None,
        };
        self.hotkey_bindings_dirty = true;
    }

    fn apply_visuals(&self, ctx: &Context, palette: &ThemePalette) {
        let mut visuals = if self.config.dark_mode {
            egui::Visuals::dark()
        } else {
            egui::Visuals::light()
        };
        visuals.panel_fill = palette.content_bg;
        visuals.window_fill = palette.toolbar_bg;
        visuals.extreme_bg_color = palette.content_bg;
        visuals.faint_bg_color = palette.toolbar_bg;
        visuals.widgets.noninteractive.bg_fill = palette.toolbar_bg;
        visuals.widgets.noninteractive.bg_stroke = Stroke::new(1.0_f32, palette.border);
        visuals.widgets.noninteractive.fg_stroke = Stroke::new(1.0_f32, palette.text);
        visuals.widgets.inactive.bg_fill = palette.toolbar_bg;
        visuals.widgets.inactive.fg_stroke = Stroke::new(1.0_f32, palette.text);
        visuals.widgets.hovered.bg_fill = palette.row_hover;
        visuals.widgets.hovered.fg_stroke = Stroke::new(1.0_f32, palette.text);
        visuals.widgets.active.bg_fill = palette.accent;
        visuals.widgets.active.fg_stroke = Stroke::new(1.0_f32, Color32::WHITE);
        visuals.selection.bg_fill = palette.accent;
        visuals.selection.stroke = Stroke::new(1.0_f32, Color32::WHITE);
        visuals.window_corner_radius = CornerRadius::same(2);
        ctx.set_visuals(visuals);
    }

    fn handle_background_events(&mut self, ctx: &Context) {
        // Duration probing updates
        while let Ok((id, dur)) = self.duration_rx.try_recv() {
            if let Some(s) = self.find_sound_mut(id) {
                s.duration_secs = Some(dur);
                self.dirty_config = true;
            }
        }

        // Auto-advance check
        if let Some(finished_id) = self.audio.take_auto_advance() {
            if self.config.auto_advance {
                for cat in &self.config.categories {
                    if let Some(pos) = cat.sounds.iter().position(|s| s.id == finished_id) {
                        if pos + 1 < cat.sounds.len() {
                            let next_id = cat.sounds[pos + 1].id;
                            self.play_sound_by_id(next_id);
                        }
                        break;
                    }
                }
            }
        }

        // Check if active recording ended unexpectedly
        if let Some(res) = self.recorder.poll_finished() {
            match res {
                Ok(path) => self.finish_recorded_file(path),
                Err(e) => self.status_toast = Some(format!("Recording error: {e}")),
            }
        }

        // Sync play counts triggered from background evdev thread while hidden
        let bg_played: Vec<u64> = self
            .bg_hotkey_state
            .lock()
            .map(|mut st| st.played_counts.drain(..).collect())
            .unwrap_or_default();
        for pid in bg_played {
            if let Some(s) = self.find_sound_mut(pid) {
                s.play_count = s.play_count.saturating_add(1);
            }
            self.selected_sound_ids.clear();
            self.selected_sound_ids.insert(pid);
            self.last_selected_sound_id = Some(pid);
            self.dirty_config = true;
        }

        // Hotkey actions from evdev thread
        for act in self.hotkeys.poll_actions() {
            match act {
                HotkeyAction::PlaySound(id) => self.play_sound_by_id(id),
                HotkeyAction::StopAll => self.audio.stop_all(),
                HotkeyAction::TogglePause => self.toggle_play_selected_or_first(),
                HotkeyAction::ToggleRecord => self.toggle_recording(),
                HotkeyAction::Captured(hk) => self.apply_captured_hotkey(hk),
            }
        }

        // Also allow capturing hotkey via egui keyboard events when modal is focused
        if matches!(
            self.modal,
            ModalState::SetHotkey { .. }
                | ModalState::Preferences {
                    capturing: Some(_),
                    ..
                }
        ) {
            let captured = ctx.input(|i| {
                for ev in &i.events {
                    if let egui::Event::Key {
                        key,
                        pressed: true,
                        modifiers,
                        ..
                    } = ev
                    {
                        if let Some(name) = egui_key_to_name(*key) {
                            return Some(Hotkey::new(
                                modifiers.ctrl,
                                modifiers.alt,
                                modifiers.shift,
                                modifiers.mac_cmd || modifiers.command && !modifiers.ctrl,
                                name,
                            ));
                        }
                    }
                }
                None
            });
            if let Some(hk) = captured {
                self.apply_captured_hotkey(hk);
            }
        }

        // Global UI shortcuts when no modal or editor is capturing input
        if matches!(self.modal, ModalState::None)
            && self.editor.is_none()
            && !ctx.wants_keyboard_input()
        {
            let (ctrl_o, ctrl_f, ctrl_e, del_pressed, enter_pressed, up_pressed, down_pressed) =
                ctx.input_mut(|i| {
                    (
                        i.consume_key(egui::Modifiers::CTRL, Key::O),
                        i.consume_key(egui::Modifiers::CTRL, Key::F),
                        i.consume_key(egui::Modifiers::CTRL, Key::E),
                        i.consume_key(egui::Modifiers::NONE, Key::Delete),
                        i.consume_key(egui::Modifiers::NONE, Key::Enter),
                        i.consume_key(egui::Modifiers::NONE, Key::ArrowUp),
                        i.consume_key(egui::Modifiers::NONE, Key::ArrowDown),
                    )
                });
            if ctrl_o {
                self.add_files_dialog();
            }
            if ctrl_f {
                self.config.search_visible = !self.config.search_visible;
                self.mark_dirty();
            }
            if ctrl_e {
                if let Some(sid) = self
                    .last_selected_sound_id
                    .or_else(|| self.selected_sound_ids.iter().next().copied())
                {
                    self.open_sound_editor(sid);
                }
            }
            if del_pressed {
                self.remove_selected_sounds();
            }
            if enter_pressed {
                self.toggle_play_selected_or_first();
            }
            if up_pressed {
                self.move_selection(-1);
            }
            if down_pressed {
                self.move_selection(1);
            }
        }

        // File drag-and-drop from external file manager
        let dropped: Vec<PathBuf> = ctx.input(|i| {
            i.raw
                .dropped_files
                .iter()
                .filter_map(|f| f.path.clone())
                .collect()
        });
        if !dropped.is_empty() {
            let mut expanded = Vec::new();
            for p in dropped {
                if p.is_dir() {
                    collect_audio_files_recursive(&p, &mut expanded);
                } else {
                    expanded.push(p);
                }
            }
            self.add_sound_paths(expanded);
        }
    }

    fn apply_captured_hotkey(&mut self, hk: Hotkey) {
        match &mut self.modal {
            ModalState::SetHotkey { sound_id, .. } => {
                let id = *sound_id;
                if let Some(s) = self.find_sound_mut(id) {
                    s.hotkey = Some(hk);
                }
                self.modal = ModalState::None;
                self.mark_dirty();
            }
            ModalState::Preferences { capturing, .. } if capturing.is_some() => {
                match capturing.take() {
                    Some(HotkeyCaptureTarget::StopAll) => {
                        self.config.stop_hotkey = Some(hk);
                    }
                    Some(HotkeyCaptureTarget::PauseResume) => {
                        self.config.pause_hotkey = Some(hk);
                    }
                    Some(HotkeyCaptureTarget::RecordToggle) => {
                        self.config.record_hotkey = Some(hk);
                    }
                    None => {}
                }
                self.mark_dirty();
            }
            _ => {}
        }
    }
}

impl eframe::App for SoundpadApp {
    fn update(&mut self, ctx: &Context, _frame: &mut eframe::Frame) {
        self.handle_background_events(ctx);

        self.audio.sync_settings(
            self.config.speakers_enabled,
            self.config.mic_enabled,
            self.config.master_volume,
            self.config.speakers_volume,
            self.config.mic_volume,
            self.config.auto_normalize,
            self.config.repeat_enabled,
            self.config.auto_advance,
        );

        if self.hotkey_bindings_dirty {
            self.sync_hotkey_bindings();
        }
        self.sync_bg_hotkey_state();

        if self.last_always_on_top != self.config.always_on_top {
            self.last_always_on_top = self.config.always_on_top;
            let level = if self.config.always_on_top {
                egui::WindowLevel::AlwaysOnTop
            } else {
                egui::WindowLevel::Normal
            };
            ctx.send_viewport_cmd(ViewportCommand::WindowLevel(level));
        }

        let status = self.audio.status();
        if status.is_playing || self.recorder.is_recording() {
            ctx.request_repaint_after(std::time::Duration::from_millis(40));
        }

        let palette = ThemePalette::from_dark(self.config.dark_mode);
        self.apply_visuals(ctx, &palette);

        // A. Top Menu Bar
        egui::TopBottomPanel::top("menu_bar")
            .frame(
                Frame::new()
                    .fill(palette.toolbar_bg)
                    .inner_margin(Margin::symmetric(6, 2))
                    .stroke(Stroke::new(1.0_f32, palette.border)),
            )
            .show(ctx, |ui| {
                self.render_menu_bar(ui, ctx);
            });

        // Optional evdev permission warning banner
        let hk_status = self.hotkeys.status();
        if hk_status.permission_denied && !self.dismiss_perm_banner {
            egui::TopBottomPanel::top("evdev_perm_banner")
                .frame(
                    Frame::new()
                        .fill(Color32::from_rgb(255, 243, 205))
                        .inner_margin(Margin::symmetric(8, 5))
                        .stroke(Stroke::new(1.0_f32, palette.border)),
                )
                .show(ctx, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(
                            RichText::new(
                                "Global hotkeys on Wayland need read access to /dev/input/event*. Run: sudo usermod -aG input $USER (then re-login)",
                            )
                            .color(Color32::from_rgb(90, 62, 0))
                            .size(12.0),
                        );
                        ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                            if ui.small_button("✕").clicked() {
                                self.dismiss_perm_banner = true;
                            }
                            if ui.small_button("Copy command").clicked() {
                                ctx.copy_text("sudo usermod -aG input $USER".to_string());
                                self.status_toast = Some("Copied command to clipboard".to_string());
                            }
                        });
                    });
                });
        }

        // B. Control Toolbar
        egui::TopBottomPanel::top("control_toolbar")
            .exact_height(38.0)
            .frame(
                Frame::new()
                    .fill(palette.toolbar_bg)
                    .inner_margin(Margin::symmetric(8, 4))
                    .stroke(Stroke::new(1.0_f32, palette.border)),
            )
            .show(ctx, |ui| {
                self.render_toolbar(ui, &palette, &status);
            });

        // Optional Search Bar (Ctrl+F / Window -> Search Bar)
        if self.config.search_visible {
            egui::TopBottomPanel::top("search_bar")
                .frame(
                    Frame::new()
                        .fill(palette.toolbar_bg)
                        .inner_margin(Margin::symmetric(8, 4))
                        .stroke(Stroke::new(1.0_f32, palette.border)),
                )
                .show(ctx, |ui| {
                    ui.horizontal(|ui| {
                        ui.label("Search:");
                        let resp = ui.add(
                            egui::TextEdit::singleline(&mut self.search_query)
                                .desired_width(240.0)
                                .hint_text("Filter sounds by tag or hotkey..."),
                        );
                        if resp.lost_focus() && ui.input(|i| i.key_pressed(Key::Escape)) {
                            self.config.search_visible = false;
                        }
                        if ui.small_button("Clear").clicked() {
                            self.search_query.clear();
                        }
                        if ui.small_button("Close").clicked() {
                            self.config.search_visible = false;
                            self.search_query.clear();
                        }
                    });
                });
        }

        // D. Bottom Status Bar
        egui::TopBottomPanel::bottom("status_bar")
            .exact_height(26.0)
            .frame(
                Frame::new()
                    .fill(palette.toolbar_bg)
                    .inner_margin(Margin::symmetric(8, 3))
                    .stroke(Stroke::new(1.0_f32, palette.border)),
            )
            .show(ctx, |ui| {
                self.render_status_bar(ui, &palette, &status);
            });

        // C. Main Workspace (Split View: Left 22% Categories, Right 78% Sounds + Category Footer)
        egui::CentralPanel::default()
            .frame(Frame::new().fill(palette.content_bg).inner_margin(Margin::ZERO))
            .show(ctx, |ui| {
                let total_rect = ui.available_rect_before_wrap();
                let left_w = (total_rect.width() * 0.22).clamp(160.0, 320.0);
                let left_rect = Rect::from_min_size(total_rect.min, vec2(left_w, total_rect.height()));
                let right_rect = Rect::from_min_max(
                    pos2(left_rect.right() + 1.0, total_rect.top()),
                    total_rect.max,
                );

                // Vertical 1px divider between left and right panes
                ui.painter().line_segment(
                    [
                        pos2(left_rect.right() + 0.5, total_rect.top()),
                        pos2(left_rect.right() + 0.5, total_rect.bottom()),
                    ],
                    Stroke::new(1.0_f32, palette.border),
                );

                let mut left_ui = ui.new_child(egui::UiBuilder::new().max_rect(left_rect));
                self.render_categories_panel(&mut left_ui, &palette);

                let mut right_ui = ui.new_child(egui::UiBuilder::new().max_rect(right_rect));
                self.render_right_panel(&mut right_ui, &palette, &status);
            });

        // Floating drag pill when dragging sound row(s) onto a category
        if let Some(ref ids) = self.dragging_sound_ids {
            if ctx.input(|i| i.pointer.any_released()) {
                self.dragging_sound_ids = None;
            } else if let Some(pointer_pos) = ctx.pointer_interact_pos() {
                egui::Area::new(Id::new("drag_sound_pill"))
                    .fixed_pos(pointer_pos + vec2(12.0, 12.0))
                    .interactable(false)
                    .show(ctx, |ui| {
                        Frame::new()
                            .fill(palette.accent)
                            .corner_radius(CornerRadius::same(3))
                            .inner_margin(Margin::symmetric(8, 4))
                            .show(ui, |ui| {
                                ui.label(
                                    RichText::new(format!("Move {} sound(s)", ids.len()))
                                        .color(Color32::WHITE)
                                        .size(12.0),
                                );
                            });
                    });
            }
        }

        // Modals & popups
        self.render_modals(ctx, &palette);
        self.render_mic_volume_popup(ctx, &palette);
        self.render_sound_editor(ctx);

        if self.dirty_config && !ctx.input(|i| i.pointer.any_down()) {
            if save_config(&self.config).is_ok() {
                self.dirty_config = false;
            }
        }
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        let _ = save_config(&self.config);
        self.audio.stop_all();
        self.pipewire.cleanup();
    }
}

impl SoundpadApp {
    fn render_menu_bar(&mut self, ui: &mut Ui, ctx: &Context) {
        egui::menu::bar(ui, |ui| {
            ui.menu_button("File", |ui| {
                if ui.button("Add Sound Files...   Ctrl+O").clicked() {
                    ui.close_menu();
                    self.add_files_dialog();
                }
                if ui.button("Add Folder...").clicked() {
                    ui.close_menu();
                    self.add_folder_dialog();
                }
                ui.separator();
                if ui.button("Import Soundlist (.spl)...").clicked() {
                    ui.close_menu();
                    self.import_spl_dialog();
                }
                if ui.button("Export Soundlist (.spl)...").clicked() {
                    ui.close_menu();
                    self.export_spl_dialog();
                }
                ui.separator();
                if ui.button("Save Configuration").clicked() {
                    ui.close_menu();
                    match save_config(&self.config) {
                        Ok(()) => {
                            self.dirty_config = false;
                            self.status_toast = Some("Configuration saved".to_string());
                        }
                        Err(e) => self.status_toast = Some(e),
                    }
                }
                ui.separator();
                if ui.button("Exit").clicked() {
                    ui.close_menu();
                    ctx.send_viewport_cmd(ViewportCommand::Close);
                }
            });

            ui.menu_button("Edit", |ui| {
                if ui.button("Preferences...").clicked() {
                    ui.close_menu();
                    self.open_preferences();
                }
                ui.separator();
                if ui.button("Add Category...").clicked() {
                    ui.close_menu();
                    self.modal = ModalState::NewCategory {
                        text: String::new(),
                        icon: CategoryIcon::Folder,
                    };
                }
                if ui.button("Rename...").clicked() {
                    ui.close_menu();
                    if let Some(sid) = self.last_selected_sound_id {
                        if let Some(s) = self
                            .config
                            .selected_category()
                            .and_then(|c| c.sounds.iter().find(|s| s.id == sid))
                        {
                            self.modal = ModalState::RenameSound {
                                sound_id: sid,
                                text: s.tag.clone(),
                            };
                        }
                    } else if let Some(cat) = self.config.selected_category() {
                        self.modal = ModalState::RenameCategory {
                            category_id: cat.id,
                            text: cat.name.clone(),
                            icon: cat.icon,
                        };
                    }
                }
                if ui
                    .add_enabled(
                        !self.selected_sound_ids.is_empty(),
                        egui::Button::new("Remove Selected   Del"),
                    )
                    .clicked()
                {
                    ui.close_menu();
                    self.remove_selected_sounds();
                }
            });

            ui.menu_button("Play", |ui| {
                if ui.button("Play").clicked() {
                    ui.close_menu();
                    self.play_selected_or_first();
                }
                if ui.button("Pause").clicked() {
                    ui.close_menu();
                    self.audio.toggle_pause();
                }
                if ui.button("Stop").clicked() {
                    ui.close_menu();
                    self.audio.stop_all();
                }
                ui.separator();
                if ui.button("Previous").clicked() {
                    ui.close_menu();
                    self.play_relative(-1);
                }
                if ui.button("Next").clicked() {
                    ui.close_menu();
                    self.play_relative(1);
                }
                ui.separator();
                if ui
                    .checkbox(&mut self.config.speakers_enabled, "Play on Speakers")
                    .changed()
                {
                    self.dirty_config = true;
                }
                if ui
                    .checkbox(&mut self.config.mic_enabled, "Play on Microphone")
                    .changed()
                {
                    self.dirty_config = true;
                }
            });

            ui.menu_button("Window", |ui| {
                let theme_label = if self.config.dark_mode {
                    "Switch to Light Mode"
                } else {
                    "Switch to Dark Mode"
                };
                if ui.button(theme_label).clicked() {
                    self.config.dark_mode = !self.config.dark_mode;
                    self.dirty_config = true;
                    ui.close_menu();
                }
                if ui
                    .checkbox(&mut self.config.search_visible, "Search Bar   Ctrl+F")
                    .changed()
                {
                    self.dirty_config = true;
                }
                if ui
                    .checkbox(&mut self.config.always_on_top, "Always on Top")
                    .changed()
                {
                    self.dirty_config = true;
                }
            });

            ui.menu_button("Help", |ui| {
                if ui.button("Hotkey List").clicked() {
                    ui.close_menu();
                    self.modal = ModalState::HotkeyList;
                }
                if ui.button("About").clicked() {
                    ui.close_menu();
                    self.modal = ModalState::About;
                }
            });
        });
    }

    fn render_toolbar(&mut self, ui: &mut Ui, palette: &ThemePalette, status: &PlaybackStatus) {
        ui.horizontal_centered(|ui| {
            ui.spacing_mut().item_spacing.x = 4.0;

            // 7 blue icon buttons on the left
            if toolbar_icon_btn(ui, palette, false, "Play", icons::paint_play).clicked() {
                if status.is_paused {
                    self.audio.set_paused(false);
                } else {
                    self.play_selected_or_first();
                }
            }
            if toolbar_icon_btn(
                ui,
                palette,
                self.config.speakers_enabled,
                "Play on Speakers (Toggle)",
                icons::paint_headphones,
            )
            .clicked()
            {
                self.config.speakers_enabled = !self.config.speakers_enabled;
                self.dirty_config = true;
            }
            if toolbar_icon_btn(
                ui,
                palette,
                self.config.mic_enabled,
                "Play on Microphone (Toggle)",
                icons::paint_mic,
            )
            .clicked()
            {
                self.config.mic_enabled = !self.config.mic_enabled;
                self.dirty_config = true;
            }
            if toolbar_icon_btn(ui, palette, status.is_paused, "Pause", icons::paint_pause)
                .clicked()
            {
                self.audio.toggle_pause();
            }
            if toolbar_icon_btn(ui, palette, false, "Stop", icons::paint_stop).clicked() {
                self.audio.stop_all();
                self.stop_recording_if_active();
            }
            if toolbar_icon_btn(ui, palette, false, "Previous", icons::paint_prev).clicked() {
                self.play_relative(-1);
            }
            if toolbar_icon_btn(ui, palette, false, "Next", icons::paint_next).clicked() {
                self.play_relative(1);
            }

            // Round Record button
            let is_recording = self.recorder.is_recording();
            let (rec_rect, rec_resp) = ui.allocate_exact_size(vec2(26.0, 26.0), Sense::click());
            let rec_red = Color32::from_rgb(211, 47, 47);
            if is_recording {
                ui.painter()
                    .circle_filled(rec_rect.center(), rec_rect.width() * 0.46, rec_red);
            } else if rec_resp.hovered() {
                ui.painter().circle_filled(
                    rec_rect.center(),
                    rec_rect.width() * 0.46,
                    palette.row_hover,
                );
            }
            let rec_icon_color = if is_recording { Color32::WHITE } else { rec_red };
            icons::paint_record(ui.painter(), rec_rect, rec_icon_color);
            if rec_resp
                .on_hover_text(if is_recording {
                    "Stop Recording"
                } else {
                    "Record Audio"
                })
                .clicked()
            {
                self.toggle_recording();
            }

            ui.add_space(6.0);

            // Center: timer + seek bar
            let shown_pos = self.seek_drag_value.unwrap_or(status.position_secs);
            if is_recording {
                let total = self.recorder.elapsed_secs().max(0.0).floor() as u64;
                let rec_str = format!("REC {}:{:02}", total / 60, total % 60);
                ui.label(
                    RichText::new(rec_str)
                        .monospace()
                        .strong()
                        .size(13.0)
                        .color(rec_red),
                );
            } else {
                let time_str = SoundEntry::format_duration(Some(shown_pos));
                ui.label(
                    RichText::new(time_str)
                        .monospace()
                        .size(13.0)
                        .color(palette.text),
                );
            }

            ui.add_space(4.0);

            let right_area_w = 185.0;
            let seek_w = (ui.available_width() - right_area_w).max(80.0);
            let max_dur = status.duration_secs.max(0.01);
            let mut slider_val = shown_pos.clamp(0.0, max_dur);

            let seek_resp = paint_blue_slider(
                ui,
                palette,
                &mut slider_val,
                0.0..=max_dur,
                seek_w,
                status.current_sound_id.is_some(),
            );
            if seek_resp.dragged() || seek_resp.clicked() {
                self.seek_drag_value = Some(slider_val);
            }
            if seek_resp.drag_stopped() || (seek_resp.clicked() && !seek_resp.dragged()) {
                self.audio.seek(slider_val);
                self.seek_drag_value = None;
            }

            ui.add_space(6.0);

            // Vertical separator |
            let sep_rect = ui.available_rect_before_wrap();
            ui.painter().line_segment(
                [
                    pos2(sep_rect.left(), sep_rect.top() + 3.0),
                    pos2(sep_rect.left(), sep_rect.bottom() - 3.0),
                ],
                Stroke::new(1.0_f32, palette.border),
            );
            ui.add_space(6.0);

            // Right: blue headphones icon + speaker volume slider (right-click -> mic volume)
            let (hp_rect, hp_resp) = ui.allocate_exact_size(vec2(24.0, 24.0), Sense::click());
            icons::paint_headphones(ui.painter(), hp_rect, palette.accent);
            if hp_resp.clicked() {
                self.config.speakers_enabled = !self.config.speakers_enabled;
                self.dirty_config = true;
            }
            if hp_resp.secondary_clicked() {
                self.show_mic_vol_popup = !self.show_mic_vol_popup;
            }
            hp_resp.on_hover_text(format!(
                "Speakers volume: {}% (Right-click for Microphone volume)",
                (self.config.speakers_volume * 100.0).round() as u32
            ));

            let vol_w = (ui.available_width() - 4.0).clamp(60.0, 140.0);
            let mut spk_vol = self.config.speakers_volume;
            let vol_resp = paint_blue_slider(ui, palette, &mut spk_vol, 0.0..=1.0, vol_w, true);
            if vol_resp.changed() {
                self.config.speakers_volume = spk_vol;
                self.dirty_config = true;
            }
            if vol_resp.secondary_clicked() {
                self.show_mic_vol_popup = !self.show_mic_vol_popup;
            }
            vol_resp.on_hover_text(format!(
                "Speakers: {}% | Right-click for Mic volume",
                (self.config.speakers_volume * 100.0).round() as u32
            ));
        });
    }

    fn render_categories_panel(&mut self, ui: &mut Ui, palette: &ThemePalette) {
        let panel_rect = ui.max_rect();
        let header_h = 24.0;
        let header_rect = Rect::from_min_size(panel_rect.min, vec2(panel_rect.width(), header_h));

        // Table Header: Category (left) and # (right)
        ui.painter().rect_filled(header_rect, 0.0, palette.toolbar_bg);
        ui.painter().line_segment(
            [
                pos2(header_rect.left(), header_rect.bottom()),
                pos2(header_rect.right(), header_rect.bottom()),
            ],
            Stroke::new(1.0_f32, palette.border),
        );
        let count_col_w = 34.0;
        let sep_x = header_rect.right() - count_col_w;
        ui.painter().line_segment(
            [
                pos2(sep_x, header_rect.top()),
                pos2(sep_x, header_rect.bottom()),
            ],
            Stroke::new(1.0_f32, palette.border),
        );

        ui.painter().text(
            pos2(header_rect.left() + 8.0, header_rect.center().y),
            egui::Align2::LEFT_CENTER,
            "Category",
            FontId::proportional(12.5),
            palette.text,
        );
        ui.painter().text(
            pos2(header_rect.right() - 8.0, header_rect.center().y),
            egui::Align2::RIGHT_CENTER,
            "#",
            FontId::proportional(12.5),
            palette.text,
        );

        let list_rect = Rect::from_min_max(
            pos2(panel_rect.left(), header_rect.bottom() + 1.0),
            panel_rect.max,
        );
        let mut list_ui = ui.new_child(egui::UiBuilder::new().max_rect(list_rect));
        list_ui.set_clip_rect(list_rect);

        let categories_snapshot: Vec<(u64, String, CategoryIcon, usize)> = self
            .config
            .categories
            .iter()
            .map(|c| (c.id, c.name.clone(), c.icon, c.sounds.len()))
            .collect();

        let mut select_cat_id: Option<u64> = None;
        let mut rename_cat: Option<(u64, String, CategoryIcon)> = None;
        let mut delete_cat_id: Option<u64> = None;
        let mut open_new_cat = false;
        let mut drop_target_cat_id: Option<u64> = None;

        ScrollArea::vertical()
            .id_salt("categories_scroll")
            .auto_shrink([false, false])
            .show(&mut list_ui, |ui| {
                ui.spacing_mut().item_spacing = Vec2::ZERO;
                let row_h = 24.0;
                for (cat_id, cat_name, cat_icon, count) in &categories_snapshot {
                    let is_active = *cat_id == self.config.selected_category_id;
                    let (row_rect, resp) = ui.allocate_exact_size(
                        vec2(ui.available_width(), row_h),
                        Sense::click(),
                    );

                    let is_drag_hover = self.dragging_sound_ids.is_some()
                        && resp.hovered()
                        && !is_active;

                    if is_active {
                        ui.painter().rect_filled(row_rect, 0.0, palette.accent);
                    } else if is_drag_hover {
                        ui.painter()
                            .rect_filled(row_rect, 0.0, palette.accent.gamma_multiply(0.25));
                        ui.painter().rect_stroke(
                            row_rect,
                            0.0,
                            Stroke::new(1.0_f32, palette.accent),
                            egui::StrokeKind::Inside,
                        );
                    } else if resp.hovered() {
                        ui.painter().rect_filled(row_rect, 0.0, palette.row_hover);
                    }

                    let icon_rect = Rect::from_center_size(
                        pos2(row_rect.left() + 14.0, row_rect.center().y),
                        vec2(16.0, 16.0),
                    );
                    let fg = if is_active {
                        palette.selected_text
                    } else {
                        palette.text
                    };
                    let icon_color = if is_active {
                        palette.selected_text
                    } else {
                        palette.accent
                    };

                    icons::paint_category_icon(ui.painter(), icon_rect, *cat_icon, icon_color);

                    let cat_text_clip = Rect::from_min_max(
                        pos2(row_rect.left() + 26.0, row_rect.top()),
                        pos2((row_rect.right() - 34.0).max(row_rect.left() + 30.0), row_rect.bottom()),
                    );
                    ui.painter().with_clip_rect(cat_text_clip).text(
                        pos2(row_rect.left() + 28.0, row_rect.center().y),
                        egui::Align2::LEFT_CENTER,
                        cat_name,
                        FontId::proportional(13.0),
                        fg,
                    );
                    ui.painter().text(
                        pos2(row_rect.right() - 8.0, row_rect.center().y),
                        egui::Align2::RIGHT_CENTER,
                        count.to_string(),
                        FontId::proportional(12.5),
                        fg,
                    );

                    // Drop dragged sounds onto this category
                    if is_drag_hover && ui.input(|i| i.pointer.any_released()) {
                        drop_target_cat_id = Some(*cat_id);
                    }

                    if resp.clicked() {
                        select_cat_id = Some(*cat_id);
                    }

                    resp.context_menu(|ui| {
                        if ui.button("New Category").clicked() {
                            open_new_cat = true;
                            ui.close_menu();
                        }
                        if ui.button("Rename Category").clicked() {
                            rename_cat = Some((*cat_id, cat_name.clone(), *cat_icon));
                            ui.close_menu();
                        }
                        if ui
                            .add_enabled(
                                categories_snapshot.len() > 1,
                                egui::Button::new("Delete Category"),
                            )
                            .clicked()
                        {
                            delete_cat_id = Some(*cat_id);
                            ui.close_menu();
                        }
                    });
                }

                // Empty space context menu
                let remaining_h = ui.available_height();
                if remaining_h > 0.0 {
                    let (_, empty_resp) = ui.allocate_exact_size(
                        vec2(ui.available_width(), remaining_h),
                        Sense::click(),
                    );
                    empty_resp.context_menu(|ui| {
                        if ui.button("New Category").clicked() {
                            open_new_cat = true;
                            ui.close_menu();
                        }
                    });
                }
            });

        if let Some(target_id) = drop_target_cat_id {
            if let Some(ids) = self.dragging_sound_ids.take() {
                self.move_sounds_to_category(&ids, target_id);
            }
        }
        if let Some(id) = select_cat_id {
            if self.config.selected_category_id != id {
                self.config.selected_category_id = id;
                self.selected_sound_ids.clear();
                self.last_selected_sound_id = None;
                self.dirty_config = true;
            }
        }
        if open_new_cat {
            self.modal = ModalState::NewCategory {
                text: String::new(),
                icon: CategoryIcon::Folder,
            };
        }
        if let Some((category_id, text, icon)) = rename_cat {
            self.modal = ModalState::RenameCategory {
                category_id,
                text,
                icon,
            };
        }
        if let Some(del_id) = delete_cat_id {
            if self.config.categories.len() > 1 {
                self.config.categories.retain(|c| c.id != del_id);
                self.config.ensure_valid();
                self.selected_sound_ids.clear();
                self.last_selected_sound_id = None;
                self.mark_dirty();
            }
        }
    }

    fn render_right_panel(
        &mut self,
        ui: &mut Ui,
        palette: &ThemePalette,
        status: &PlaybackStatus,
    ) {
        let full_rect = ui.max_rect();
        let footer_h = 65.0;
        let table_rect = Rect::from_min_max(
            full_rect.min,
            pos2(full_rect.right(), (full_rect.bottom() - footer_h).max(full_rect.top() + 40.0)),
        );
        let footer_rect = Rect::from_min_max(
            pos2(full_rect.left(), table_rect.bottom()),
            full_rect.max,
        );

        let mut table_ui = ui.new_child(egui::UiBuilder::new().max_rect(table_rect));
        self.render_sound_table(&mut table_ui, palette, status);

        // 1px top border for category info footer
        ui.painter().line_segment(
            [
                pos2(footer_rect.left(), footer_rect.top()),
                pos2(footer_rect.right(), footer_rect.top()),
            ],
            Stroke::new(1.0_f32, palette.border),
        );

        let mut footer_ui = ui.new_child(egui::UiBuilder::new().max_rect(footer_rect));
        self.render_category_footer(&mut footer_ui, palette);
    }

    fn render_sound_table(
        &mut self,
        ui: &mut Ui,
        palette: &ThemePalette,
        status: &PlaybackStatus,
    ) {
        let panel_rect = ui.max_rect();
        let header_h = 24.0;
        let header_rect = Rect::from_min_size(panel_rect.min, vec2(panel_rect.width(), header_h));

        // Column widths: Index, Tag, Duration, Hotkey, + trailing empty column
        let w_index = 48.0;
        let w_tag = (panel_rect.width() * 0.44).clamp(160.0, 420.0);
        let w_dur = 74.0;
        let w_hotkey = 130.0;

        let x0 = header_rect.left();
        let x1 = x0 + w_index;
        let x2 = x1 + w_tag;
        let x3 = x2 + w_dur;
        let x4 = (x3 + w_hotkey).min(header_rect.right());

        // Header background & 1px borders
        ui.painter().rect_filled(header_rect, 0.0, palette.toolbar_bg);
        ui.painter().line_segment(
            [
                pos2(header_rect.left(), header_rect.bottom()),
                pos2(header_rect.right(), header_rect.bottom()),
            ],
            Stroke::new(1.0_f32, palette.border),
        );
        for &col_x in &[x1, x2, x3, x4] {
            if col_x < header_rect.right() {
                ui.painter().line_segment(
                    [
                        pos2(col_x, header_rect.top()),
                        pos2(col_x, header_rect.bottom()),
                    ],
                    Stroke::new(1.0_f32, palette.border),
                );
            }
        }

        let headers = [
            (x0 + 8.0, "Index"),
            (x1 + 8.0, "Tag"),
            (x2 + 8.0, "Duration"),
            (x3 + 8.0, "Hotkey"),
        ];
        for (hx, label) in headers {
            if hx < header_rect.right() - 10.0 {
                ui.painter().text(
                    pos2(hx, header_rect.center().y),
                    egui::Align2::LEFT_CENTER,
                    label,
                    FontId::proportional(12.5),
                    palette.text,
                );
            }
        }

        let body_rect = Rect::from_min_max(
            pos2(panel_rect.left(), header_rect.bottom() + 1.0),
            panel_rect.max,
        );
        let mut body_ui = ui.new_child(egui::UiBuilder::new().max_rect(body_rect));
        body_ui.set_clip_rect(body_rect);

        let query = self.search_query.trim().to_ascii_lowercase();
        let rows: Vec<(usize, u64, String, String, String)> = self
            .config
            .selected_category()
            .map(|c| {
                c.sounds
                    .iter()
                    .enumerate()
                    .filter(|(_, s)| {
                        if query.is_empty() {
                            true
                        } else {
                            let hk = s
                                .hotkey
                                .as_ref()
                                .map(|h| h.display().to_ascii_lowercase())
                                .unwrap_or_default();
                            s.tag.to_ascii_lowercase().contains(&query) || hk.contains(&query)
                        }
                    })
                    .map(|(idx, s)| {
                        (
                            idx + 1,
                            s.id,
                            s.tag.clone(),
                            SoundEntry::format_duration(s.duration_secs),
                            s.hotkey
                                .as_ref()
                                .map(|h| h.display())
                                .unwrap_or_default(),
                        )
                    })
                    .collect()
            })
            .unwrap_or_default();

        let mut play_id: Option<u64> = None;
        let mut edit_id: Option<u64> = None;
        let mut open_folder_id: Option<u64> = None;
        let mut set_hk: Option<(u64, String)> = None;
        let mut remove_hk_id: Option<u64> = None;
        let mut rename_sound: Option<(u64, String)> = None;
        let mut reset_pc_id: Option<u64> = None;
        let mut remove_id: Option<u64> = None;

        ScrollArea::vertical()
            .id_salt("sounds_table_scroll")
            .auto_shrink([false, false])
            .show(&mut body_ui, |ui| {
                ui.spacing_mut().item_spacing = Vec2::ZERO;
                let row_h = 22.0;

                if rows.is_empty() {
                    let empty_rect = ui.available_rect_before_wrap();
                    ui.painter().text(
                        empty_rect.center(),
                        egui::Align2::CENTER_CENTER,
                        "Drag & drop audio/video files (MP3, WAV, OGG, FLAC, MP4, MKV, WEBM, MOV, M4A, AVI) here or File -> Add Sound Files (Ctrl+O)",
                        FontId::proportional(13.0),
                        palette.muted_text,
                    );
                    ui.allocate_space(empty_rect.size());
                    return;
                }

                for (index_num, sound_id, tag, dur, hk_str) in &rows {
                    let (row_rect, resp) = ui.allocate_exact_size(
                        vec2(ui.available_width(), row_h),
                        Sense::click_and_drag(),
                    );

                    if self.scroll_to_sound_id == Some(*sound_id) {
                        resp.scroll_to_me(None);
                        self.scroll_to_sound_id = None;
                    }

                    let is_selected = self.selected_sound_ids.contains(sound_id);
                    let is_playing_row = status.current_sound_id == Some(*sound_id);

                    if is_selected {
                        ui.painter().rect_filled(row_rect, 0.0, palette.accent);
                    } else if resp.hovered() {
                        ui.painter().rect_filled(row_rect, 0.0, palette.row_hover);
                    }

                    // Thin bottom row separator
                    ui.painter().line_segment(
                        [
                            pos2(row_rect.left(), row_rect.bottom()),
                            pos2(row_rect.right(), row_rect.bottom()),
                        ],
                        Stroke::new(
                            1.0_f32,
                            if is_selected {
                                palette.accent
                            } else {
                                palette.border.gamma_multiply(0.45)
                            },
                        ),
                    );

                    let fg = if is_selected {
                        palette.selected_text
                    } else {
                        palette.text
                    };

                    // Index
                    ui.painter().text(
                        pos2(row_rect.left() + 8.0, row_rect.center().y),
                        egui::Align2::LEFT_CENTER,
                        index_num.to_string(),
                        FontId::proportional(12.5),
                        fg,
                    );

                    // Optional tiny play indicator before tag if currently playing
                    let tag_x = if is_playing_row {
                        let play_r = Rect::from_center_size(
                            pos2(row_rect.left() + w_index + 12.0, row_rect.center().y),
                            vec2(10.0, 10.0),
                        );
                        icons::paint_play(
                            ui.painter(),
                            play_r,
                            if is_selected {
                                Color32::WHITE
                            } else {
                                palette.accent
                            },
                        );
                        row_rect.left() + w_index + 22.0
                    } else {
                        row_rect.left() + w_index + 8.0
                    };

                    // Tag (clipped to Tag column width)
                    let tag_clip = Rect::from_min_max(
                        pos2(row_rect.left() + w_index, row_rect.top()),
                        pos2((row_rect.left() + w_index + w_tag - 4.0).max(row_rect.left() + w_index + 4.0), row_rect.bottom()),
                    );
                    ui.painter().with_clip_rect(tag_clip).text(
                        pos2(tag_x, row_rect.center().y),
                        egui::Align2::LEFT_CENTER,
                        tag,
                        FontId::proportional(12.5),
                        fg,
                    );

                    // Duration
                    ui.painter().text(
                        pos2(row_rect.left() + w_index + w_tag + 8.0, row_rect.center().y),
                        egui::Align2::LEFT_CENTER,
                        dur,
                        FontId::proportional(12.5),
                        fg,
                    );

                    // Hotkey
                    if !hk_str.is_empty() {
                        ui.painter().text(
                            pos2(
                                row_rect.left() + w_index + w_tag + w_dur + 8.0,
                                row_rect.center().y,
                            ),
                            egui::Align2::LEFT_CENTER,
                            hk_str,
                            FontId::proportional(12.5),
                            fg,
                        );
                    }

                    // Selection on click (supports Ctrl+Click multi-select)
                    if resp.clicked() {
                        let ctrl = ui.input(|i| i.modifiers.ctrl);
                        if ctrl {
                            if self.selected_sound_ids.contains(sound_id) {
                                self.selected_sound_ids.remove(sound_id);
                            } else {
                                self.selected_sound_ids.insert(*sound_id);
                                self.last_selected_sound_id = Some(*sound_id);
                            }
                        } else {
                            self.selected_sound_ids.clear();
                            self.selected_sound_ids.insert(*sound_id);
                            self.last_selected_sound_id = Some(*sound_id);
                        }
                    }

                    if resp.double_clicked() {
                        play_id = Some(*sound_id);
                    }

                    if resp.drag_started() {
                        if !self.selected_sound_ids.contains(sound_id) {
                            self.selected_sound_ids.clear();
                            self.selected_sound_ids.insert(*sound_id);
                            self.last_selected_sound_id = Some(*sound_id);
                        }
                        self.dragging_sound_ids =
                            Some(self.selected_sound_ids.iter().copied().collect());
                    }

                    resp.context_menu(|ui| {
                        if ui.button("Play").clicked() {
                            play_id = Some(*sound_id);
                            ui.close_menu();
                        }
                        if ui.button("Edit Sound...").clicked() {
                            edit_id = Some(*sound_id);
                            ui.close_menu();
                        }
                        if ui.button("Open in Folder").clicked() {
                            open_folder_id = Some(*sound_id);
                            ui.close_menu();
                        }
                        ui.separator();
                        if ui.button("Set Hotkey...").clicked() {
                            set_hk = Some((*sound_id, tag.clone()));
                            ui.close_menu();
                        }
                        if ui
                            .add_enabled(!hk_str.is_empty(), egui::Button::new("Remove Hotkey"))
                            .clicked()
                        {
                            remove_hk_id = Some(*sound_id);
                            ui.close_menu();
                        }
                        ui.separator();
                        if ui.button("Rename Tag...").clicked() {
                            rename_sound = Some((*sound_id, tag.clone()));
                            ui.close_menu();
                        }
                        if ui.button("Reset Play Count").clicked() {
                            reset_pc_id = Some(*sound_id);
                            ui.close_menu();
                        }
                        ui.separator();
                        if ui.button("Remove").clicked() {
                            remove_id = Some(*sound_id);
                            ui.close_menu();
                        }
                    });
                }
            });

        if let Some(id) = play_id {
            self.play_sound_by_id(id);
        }
        if let Some(id) = edit_id {
            self.open_sound_editor(id);
        }
        if let Some(id) = open_folder_id {
            if let Some(path) = self.find_sound(id).map(|s| s.path.clone()) {
                open_in_folder(&path);
            }
        }
        if let Some((sound_id, sound_tag)) = set_hk {
            self.modal = ModalState::SetHotkey {
                sound_id,
                sound_tag,
            };
            self.hotkey_bindings_dirty = true;
        }
        if let Some(id) = remove_hk_id {
            if let Some(s) = self.find_sound_mut(id) {
                s.hotkey = None;
            }
            self.mark_dirty();
        }
        if let Some((sound_id, text)) = rename_sound {
            self.modal = ModalState::RenameSound { sound_id, text };
        }
        if let Some(id) = reset_pc_id {
            if let Some(s) = self.find_sound_mut(id) {
                s.play_count = 0;
            }
            self.dirty_config = true;
        }
        if let Some(id) = remove_id {
            if !self.selected_sound_ids.contains(&id) {
                self.selected_sound_ids.clear();
                self.selected_sound_ids.insert(id);
            }
            self.remove_selected_sounds();
        }
    }

    fn render_category_footer(&mut self, ui: &mut Ui, palette: &ThemePalette) {
        let rect = ui.max_rect();
        ui.painter().rect_filled(rect, 0.0, palette.content_bg);

        let (cat_name, cat_icon, total_sounds, selected_play_count) = {
            let cat = self.config.selected_category();
            let total = cat.map(|c| c.sounds.len()).unwrap_or(0);
            let pc = cat
                .and_then(|c| {
                    self.last_selected_sound_id
                        .and_then(|sid| c.sounds.iter().find(|s| s.id == sid))
                })
                .map(|s| s.play_count)
                .unwrap_or(0);
            (
                cat.map(|c| c.name.clone())
                    .unwrap_or_else(|| "My Sounds".to_string()),
                cat.map(|c| c.icon).unwrap_or(CategoryIcon::Home),
                total,
                pc,
            )
        };
        let selected_count = self.selected_sound_ids.len();

        // Left: 48x48 blue icon + bold category name
        let icon_rect = Rect::from_min_size(pos2(rect.left() + 14.0, rect.center().y - 24.0), vec2(48.0, 48.0));
        icons::paint_category_icon(ui.painter(), icon_rect, cat_icon, palette.accent);

        ui.painter().text(
            pos2(icon_rect.right() + 12.0, rect.center().y),
            egui::Align2::LEFT_CENTER,
            &cat_name,
            FontId::proportional(15.0),
            palette.text,
        );

        // Center (two columns): Sounds / Selected | Play count
        let col1_x = (rect.left() + 260.0).max(rect.center().x - 90.0);
        let col2_x = col1_x + 120.0;
        let top_y = rect.center().y - 10.0;
        let bot_y = rect.center().y + 10.0;

        if col1_x < rect.right() - 150.0 {
            ui.painter().text(
                pos2(col1_x, top_y),
                egui::Align2::LEFT_CENTER,
                format!("Sounds: {total_sounds}"),
                FontId::proportional(12.5),
                palette.text,
            );
            ui.painter().text(
                pos2(col1_x, bot_y),
                egui::Align2::LEFT_CENTER,
                format!("Selected: {selected_count}"),
                FontId::proportional(12.5),
                palette.text,
            );
            ui.painter().text(
                pos2(col2_x, top_y),
                egui::Align2::LEFT_CENTER,
                format!("Play count: {selected_play_count}"),
                FontId::proportional(12.5),
                palette.text,
            );
        }

        // Right corner: blue wrench icon
        let wrench_rect = Rect::from_center_size(
            pos2(rect.right() - 22.0, rect.center().y),
            vec2(24.0, 24.0),
        );
        let wrench_resp = ui.interact(wrench_rect, Id::new("category_wrench"), Sense::click());
        let w_color = if wrench_resp.hovered() {
            palette.accent_hover
        } else {
            palette.accent
        };
        icons::paint_wrench(ui.painter(), wrench_rect, w_color);
        if wrench_resp.clicked() {
            self.open_preferences();
        }
        wrench_resp.on_hover_text("Preferences");
    }

    fn render_status_bar(&mut self, ui: &mut Ui, palette: &ThemePalette, status: &PlaybackStatus) {
        ui.horizontal_centered(|ui| {
            ui.spacing_mut().item_spacing.x = 6.0;

            // Left: small blue Play icon + H toggle + A toggle
            let (play_r, play_resp) = ui.allocate_exact_size(vec2(16.0, 16.0), Sense::click());
            icons::paint_play(ui.painter(), play_r, palette.accent);
            if play_resp.clicked() {
                self.play_selected_or_first();
            }

            if status_badge_toggle(
                ui,
                palette,
                "H",
                self.config.hotkeys_enabled,
                "Toggle global hotkeys (H)",
            )
            .clicked()
            {
                self.config.hotkeys_enabled = !self.config.hotkeys_enabled;
                self.mark_dirty();
            }

            if status_badge_toggle(
                ui,
                palette,
                "A",
                self.config.auto_normalize,
                "Toggle auto-normalization (A)",
            )
            .clicked()
            {
                self.config.auto_normalize = !self.config.auto_normalize;
                self.dirty_config = true;
            }

            // Subtle status/error info in the middle (truncated so right-side toggles stay pinned)
            let msg = status
                .last_error
                .as_deref()
                .or(self.status_toast.as_deref())
                .unwrap_or(self.pipewire.status_message.as_str());
            let max_msg_w = (ui.available_width() - 88.0).max(40.0);
            ui.add_sized(
                vec2(max_msg_w, 18.0),
                egui::Label::new(
                    RichText::new(msg)
                        .size(11.5)
                        .color(if status.last_error.is_some() {
                            Color32::from_rgb(198, 40, 40)
                        } else {
                            palette.muted_text
                        }),
                )
                .truncate(),
            );

            // Right: Repeat, Auto-advance, Mic activity / PTT indicator
            ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                ui.spacing_mut().item_spacing.x = 6.0;

                // Mic activity / PTT indicator
                let (ptt_r, ptt_resp) = ui.allocate_exact_size(vec2(20.0, 18.0), Sense::click());
                let mic_active_color = if !self.config.mic_enabled {
                    palette.inactive_icon
                } else if status.mic_peak > 0.02 {
                    Color32::from_rgb(46, 125, 50)
                } else {
                    palette.accent
                };
                icons::paint_ptt(ui.painter(), ptt_r, mic_active_color);
                if ptt_resp.clicked() {
                    self.config.mic_enabled = !self.config.mic_enabled;
                    self.dirty_config = true;
                }
                ptt_resp.on_hover_text(if self.config.mic_enabled {
                    "Virtual Mic Output: Active"
                } else {
                    "Virtual Mic Output: Muted"
                });

                // Auto-advance toggle
                let (adv_r, adv_resp) = ui.allocate_exact_size(vec2(20.0, 18.0), Sense::click());
                icons::paint_auto_advance(
                    ui.painter(),
                    adv_r,
                    if self.config.auto_advance {
                        palette.accent
                    } else {
                        palette.inactive_icon
                    },
                );
                if adv_resp.clicked() {
                    self.config.auto_advance = !self.config.auto_advance;
                    self.dirty_config = true;
                }
                adv_resp.on_hover_text("Auto-advance to next track");

                // Repeat toggle
                let (rep_r, rep_resp) = ui.allocate_exact_size(vec2(20.0, 18.0), Sense::click());
                icons::paint_repeat(
                    ui.painter(),
                    rep_r,
                    if self.config.repeat_enabled {
                        palette.accent
                    } else {
                        palette.inactive_icon
                    },
                );
                if rep_resp.clicked() {
                    self.config.repeat_enabled = !self.config.repeat_enabled;
                    self.dirty_config = true;
                }
                rep_resp.on_hover_text("Repeat current track");
            });
        });
    }

    fn render_mic_volume_popup(&mut self, ctx: &Context, palette: &ThemePalette) {
        if !self.show_mic_vol_popup {
            return;
        }
        let mut open = true;
        egui::Window::new("Microphone Output Volume")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::RIGHT_TOP, vec2(-12.0, 68.0))
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    let (r, _) = ui.allocate_exact_size(vec2(20.0, 20.0), Sense::hover());
                    icons::paint_mic(ui.painter(), r, palette.accent);
                    let mut mic_vol = self.config.mic_volume;
                    if paint_blue_slider(ui, palette, &mut mic_vol, 0.0..=1.5, 160.0, true)
                        .changed()
                    {
                        self.config.mic_volume = mic_vol;
                        self.dirty_config = true;
                    }
                    ui.label(format!("{}%", (self.config.mic_volume * 100.0).round() as u32));
                });
            });
        if !open {
            self.show_mic_vol_popup = false;
        }
    }

    fn render_modals(&mut self, ctx: &Context, palette: &ThemePalette) {
        let mut close_modal = false;
        let mut modal = std::mem::replace(&mut self.modal, ModalState::None);

        match &mut modal {
            ModalState::None => {}
            ModalState::Preferences {
                tab,
                mics,
                cpal_outputs,
                capturing,
            } => {
                let mut open = true;
                egui::Window::new("Preferences")
                    .open(&mut open)
                    .collapsible(false)
                    .resizable(false)
                    .default_width(480.0)
                    .show(ctx, |ui| {
                        // Modular top tab bar
                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing.x = 4.0;
                            for t in PreferencesTab::all() {
                                let selected = *tab == *t;
                                let btn = egui::Button::new(
                                    RichText::new(t.label()).color(if selected {
                                        Color32::WHITE
                                    } else {
                                        palette.text
                                    }),
                                )
                                .fill(if selected {
                                    palette.accent
                                } else {
                                    palette.toolbar_bg
                                });
                                if ui.add(btn).clicked() {
                                    *tab = *t;
                                    if capturing.is_some() {
                                        *capturing = None;
                                        self.hotkey_bindings_dirty = true;
                                    }
                                }
                            }
                        });
                        ui.separator();
                        ui.add_space(4.0);

                        match tab {
                            PreferencesTab::Audio => {
                                self.render_preferences_audio_tab(ui, palette);
                            }
                            PreferencesTab::Hotkeys => {
                                self.render_preferences_hotkeys_tab(ui, palette, capturing);
                            }
                            PreferencesTab::Devices => {
                                self.render_preferences_devices_tab(
                                    ui,
                                    palette,
                                    mics,
                                    cpal_outputs,
                                );
                            }
                            PreferencesTab::Recording => {
                                self.render_preferences_recording_tab(ui, palette);
                            }
                        }

                        ui.add_space(8.0);
                        ui.separator();
                        ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                            if ui.button("Close").clicked() {
                                close_modal = true;
                            }
                        });
                    });
                if !open {
                    close_modal = true;
                }
            }

            ModalState::SetHotkey { sound_tag, .. } => {
                let mut open = true;
                egui::Window::new("Set Hotkey")
                    .open(&mut open)
                    .collapsible(false)
                    .resizable(false)
                    .anchor(egui::Align2::CENTER_CENTER, Vec2::ZERO)
                    .show(ctx, |ui| {
                        ui.label(format!("Press any key combination for \"{sound_tag}\"..."));
                        ui.add_space(8.0);
                        if ui.button("Cancel").clicked() {
                            close_modal = true;
                        }
                    });
                if !open {
                    close_modal = true;
                }
            }

            ModalState::RenameSound { sound_id, text } => {
                let id = *sound_id;
                let mut open = true;
                let mut confirm = false;
                egui::Window::new("Rename Tag")
                    .open(&mut open)
                    .collapsible(false)
                    .resizable(false)
                    .anchor(egui::Align2::CENTER_CENTER, Vec2::ZERO)
                    .show(ctx, |ui| {
                        let resp = ui.text_edit_singleline(text);
                        if resp.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
                            confirm = true;
                        }
                        ui.add_space(6.0);
                        ui.horizontal(|ui| {
                            if ui.button("OK").clicked() {
                                confirm = true;
                            }
                            if ui.button("Cancel").clicked() {
                                close_modal = true;
                            }
                        });
                    });
                if confirm {
                    let new_tag = text.trim().to_string();
                    if !new_tag.is_empty() {
                        if let Some(s) = self.find_sound_mut(id) {
                            s.tag = new_tag;
                        }
                        self.dirty_config = true;
                    }
                    close_modal = true;
                } else if !open {
                    close_modal = true;
                }
            }

            ModalState::NewCategory { text, icon } => {
                let mut open = true;
                let mut create = false;
                egui::Window::new("New Category")
                    .open(&mut open)
                    .collapsible(false)
                    .resizable(false)
                    .anchor(egui::Align2::CENTER_CENTER, Vec2::ZERO)
                    .show(ctx, |ui| {
                        ui.horizontal(|ui| {
                            ui.label("Name:");
                            let resp = ui.text_edit_singleline(text);
                            if resp.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
                                create = true;
                            }
                        });
                        ui.horizontal(|ui| {
                            ui.label("Icon:");
                            egui::ComboBox::from_id_salt("new_cat_icon")
                                .selected_text(icon.label())
                                .show_ui(ui, |ui| {
                                    for ic in CategoryIcon::all() {
                                        ui.selectable_value(icon, *ic, ic.label());
                                    }
                                });
                        });
                        ui.add_space(6.0);
                        ui.horizontal(|ui| {
                            if ui.button("Create").clicked() {
                                create = true;
                            }
                            if ui.button("Cancel").clicked() {
                                close_modal = true;
                            }
                        });
                    });
                if create {
                    let name = text.trim().to_string();
                    if !name.is_empty() {
                        let new_id = self.config.alloc_id();
                        self.config.categories.push(Category {
                            id: new_id,
                            name,
                            icon: *icon,
                            sounds: Vec::new(),
                        });
                        self.config.selected_category_id = new_id;
                        self.dirty_config = true;
                    }
                    close_modal = true;
                } else if !open {
                    close_modal = true;
                }
            }

            ModalState::RenameCategory {
                category_id,
                text,
                icon,
            } => {
                let cid = *category_id;
                let mut open = true;
                let mut save = false;
                egui::Window::new("Rename Category")
                    .open(&mut open)
                    .collapsible(false)
                    .resizable(false)
                    .anchor(egui::Align2::CENTER_CENTER, Vec2::ZERO)
                    .show(ctx, |ui| {
                        ui.horizontal(|ui| {
                            ui.label("Category Name:");
                            let resp = ui.text_edit_singleline(text);
                            if resp.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
                                save = true;
                            }
                        });
                        ui.horizontal(|ui| {
                            ui.label("Category Icon:");
                            egui::ComboBox::from_id_salt("rename_cat_icon")
                                .selected_text(icon.label())
                                .show_ui(ui, |ui| {
                                    for ic in CategoryIcon::all() {
                                        ui.selectable_value(icon, *ic, ic.label());
                                    }
                                });
                        });
                        ui.add_space(6.0);
                        ui.horizontal(|ui| {
                            if ui.button("Save").clicked() {
                                save = true;
                            }
                            if ui.button("Cancel").clicked() {
                                close_modal = true;
                            }
                        });
                    });
                if save {
                    let name = text.trim().to_string();
                    if let Some(c) = self.config.categories.iter_mut().find(|c| c.id == cid) {
                        if !name.is_empty() {
                            c.name = name;
                        }
                        c.icon = *icon;
                        self.dirty_config = true;
                    }
                    close_modal = true;
                } else if !open {
                    close_modal = true;
                }
            }

            ModalState::HotkeyList => {
                let mut open = true;
                egui::Window::new("Hotkey List")
                    .open(&mut open)
                    .collapsible(false)
                    .default_width(380.0)
                    .show(ctx, |ui| {
                        if let Some(ref hk) = self.config.stop_hotkey {
                            ui.label(
                                RichText::new(format!("Stop All: {}", hk.display())).strong(),
                            );
                        }
                        if let Some(ref hk) = self.config.pause_hotkey {
                            ui.label(
                                RichText::new(format!("Pause / Resume: {}", hk.display())).strong(),
                            );
                        }
                        if let Some(ref hk) = self.config.record_hotkey {
                            ui.label(
                                RichText::new(format!(
                                    "Start / Stop Recording: {}",
                                    hk.display()
                                ))
                                .strong(),
                            );
                        }
                        if self.config.stop_hotkey.is_some()
                            || self.config.pause_hotkey.is_some()
                            || self.config.record_hotkey.is_some()
                        {
                            ui.separator();
                        }
                        ScrollArea::vertical().max_height(260.0).show(ui, |ui| {
                            let mut any = false;
                            for cat in &self.config.categories {
                                for s in &cat.sounds {
                                    if let Some(ref hk) = s.hotkey {
                                        any = true;
                                        ui.horizontal(|ui| {
                                            ui.label(RichText::new(hk.display()).strong());
                                            ui.label(format!("— {} ({})", s.tag, cat.name));
                                        });
                                    }
                                }
                            }
                            if !any {
                                ui.label("No sound hotkeys assigned yet. Right-click a sound -> Set Hotkey.");
                            }
                        });
                        ui.add_space(6.0);
                        if ui.button("Close").clicked() {
                            close_modal = true;
                        }
                    });
                if !open {
                    close_modal = true;
                }
            }

            ModalState::About => {
                let mut open = true;
                egui::Window::new("About SoundPat")
                    .open(&mut open)
                    .collapsible(false)
                    .resizable(false)
                    .anchor(egui::Align2::CENTER_CENTER, Vec2::ZERO)
                    .show(ctx, |ui| {
                        ui.heading("SoundPat");
                        ui.label("Native Soundpad analog for Arch Linux / PipeWire / KDE Plasma 6 Wayland.");
                        ui.add_space(4.0);
                        ui.label("Audio: Symphonia + CPAL + PipeWire virtual mic");
                        ui.label("Global Hotkeys: Linux evdev (/dev/input/event*)");
                        ui.add_space(8.0);
                        if ui.button("Close").clicked() {
                            close_modal = true;
                        }
                    });
                if !open {
                    close_modal = true;
                }
            }
        }

        if !close_modal && matches!(self.modal, ModalState::None) {
            self.modal = modal;
        } else {
            self.hotkey_bindings_dirty = true;
        }
    }

    fn render_sound_editor(&mut self, ctx: &Context) {
        let Some(mut ed) = self.editor.take() else {
            return;
        };

        match ed.ui(ctx, self.config.dark_mode) {
            Some(EditorAction::Close) => {
                // Editor closed without saving (or after finishing); drop it.
            }
            Some(EditorAction::Saved {
                sound_id,
                old_path,
                new_path,
                duration_secs,
            }) => {
                self.audio.clear_cache_for(&old_path);
                if new_path != old_path {
                    self.audio.clear_cache_for(&new_path);
                }
                if let Some(entry) = self.find_sound_mut(sound_id) {
                    entry.path = new_path.clone();
                    entry.duration_secs = Some(duration_secs);
                    entry.norm_gain = None;
                }
                match save_config(&self.config) {
                    Ok(()) => {
                        self.dirty_config = false;
                        let name = new_path
                            .file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or_else(|| "sound".to_string());
                        self.status_toast = Some(format!("Saved: {name}"));
                    }
                    Err(e) => self.status_toast = Some(e),
                }
                self.editor = Some(ed);
            }
            Some(EditorAction::SavedAs {
                path,
                duration_secs,
            }) => {
                let id = self.config.alloc_id();
                let tag = path
                    .file_stem()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "Sound".to_string());
                let entry = SoundEntry {
                    id,
                    path: path.clone(),
                    tag,
                    duration_secs: Some(duration_secs),
                    hotkey: None,
                    play_count: 0,
                    norm_gain: None,
                };
                if let Some(cat) = self.config.selected_category_mut() {
                    cat.sounds.push(entry);
                }
                self.selected_sound_ids.clear();
                self.selected_sound_ids.insert(id);
                self.last_selected_sound_id = Some(id);
                self.scroll_to_sound_id = Some(id);
                self.hotkey_bindings_dirty = true;
                match save_config(&self.config) {
                    Ok(()) => {
                        self.dirty_config = false;
                        let name = path
                            .file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or_else(|| "sound.wav".to_string());
                        self.status_toast = Some(format!("Added edited copy: {name}"));
                    }
                    Err(e) => self.status_toast = Some(e),
                }
                self.editor = Some(ed);
            }
            None => {
                self.editor = Some(ed);
            }
        }
    }

    fn render_preferences_audio_tab(&mut self, ui: &mut Ui, palette: &ThemePalette) {
        ui.heading("Volume Levels");
        ui.add_space(6.0);

        egui::Grid::new("pref_audio_grid")
            .num_columns(4)
            .spacing([10.0, 8.0])
            .show(ui, |ui| {
                // 1. Master Volume (0% - 150%) + reset button
                ui.label("Master Sound Volume:");
                let mut master = self.config.master_volume;
                if paint_blue_slider(ui, palette, &mut master, 0.0..=1.5, 175.0, true).changed() {
                    self.config.master_volume = master;
                    self.dirty_config = true;
                }
                ui.label(format!(
                    "{}%",
                    (self.config.master_volume * 100.0).round() as u32
                ));
                if ui.button("100%").on_hover_text("Reset to 100%").clicked() {
                    self.config.master_volume = 1.0;
                    self.dirty_config = true;
                }
                ui.end_row();

                // 2. Speakers Volume (0% - 150%)
                ui.label("Headphones Volume:");
                let mut spk = self.config.speakers_volume;
                if paint_blue_slider(ui, palette, &mut spk, 0.0..=1.5, 175.0, true).changed() {
                    self.config.speakers_volume = spk;
                    self.dirty_config = true;
                }
                ui.label(format!(
                    "{}%",
                    (self.config.speakers_volume * 100.0).round() as u32
                ));
                ui.label("");
                ui.end_row();

                // 3. Virtual Mic Volume (0% - 150%)
                ui.label("Virtual Mic Volume:");
                let mut mic = self.config.mic_volume;
                if paint_blue_slider(ui, palette, &mut mic, 0.0..=1.5, 175.0, true).changed() {
                    self.config.mic_volume = mic;
                    self.dirty_config = true;
                }
                ui.label(format!(
                    "{}%",
                    (self.config.mic_volume * 100.0).round() as u32
                ));
                ui.label("");
                ui.end_row();
            });

        ui.add_space(8.0);
        ui.separator();
        ui.add_space(4.0);

        if ui
            .checkbox(
                &mut self.config.allow_overlap,
                "Allow sound overlap (otherwise new sound stops previous)",
            )
            .changed()
        {
            self.dirty_config = true;
        }
        if ui
            .checkbox(
                &mut self.config.auto_normalize,
                "Auto-normalize volume (prevent mic clipping)",
            )
            .changed()
        {
            self.dirty_config = true;
        }
    }

    fn render_preferences_hotkeys_tab(
        &mut self,
        ui: &mut Ui,
        palette: &ThemePalette,
        capturing: &mut Option<HotkeyCaptureTarget>,
    ) {
        ui.heading("Global Hotkeys (Wayland evdev)");
        ui.add_space(6.0);

        let mut stop_hk = self.config.stop_hotkey.clone();
        let mut pause_hk = self.config.pause_hotkey.clone();
        let mut rec_hk = self.config.record_hotkey.clone();
        let mut changed = false;

        egui::Grid::new("pref_hotkeys_grid")
            .num_columns(4)
            .spacing([10.0, 8.0])
            .show(ui, |ui| {
                changed |= render_hotkey_binding_row(
                    ui,
                    palette,
                    "Stop All:",
                    &mut stop_hk,
                    capturing,
                    HotkeyCaptureTarget::StopAll,
                );
                changed |= render_hotkey_binding_row(
                    ui,
                    palette,
                    "Pause / Resume:",
                    &mut pause_hk,
                    capturing,
                    HotkeyCaptureTarget::PauseResume,
                );
                changed |= render_hotkey_binding_row(
                    ui,
                    palette,
                    "Start / Stop Recording:",
                    &mut rec_hk,
                    capturing,
                    HotkeyCaptureTarget::RecordToggle,
                );
            });

        if changed {
            self.config.stop_hotkey = stop_hk;
            self.config.pause_hotkey = pause_hk;
            self.config.record_hotkey = rec_hk;
            self.mark_dirty();
        }

        ui.add_space(8.0);
        if ui
            .checkbox(
                &mut self.config.hotkeys_enabled,
                "Enable global hotkeys",
            )
            .changed()
        {
            self.mark_dirty();
        }

        ui.add_space(6.0);
        let hk_st = self.hotkeys.status();
        ui.label(
            RichText::new(format!("evdev: {}", hk_st.status_text))
                .size(11.5)
                .color(palette.muted_text),
        );
    }

    fn render_preferences_devices_tab(
        &mut self,
        ui: &mut Ui,
        palette: &ThemePalette,
        mics: &[AudioDeviceInfo],
        cpal_outputs: &[String],
    ) {
        ui.heading("Audio Devices & PipeWire");
        ui.add_space(6.0);

        // Physical microphone selector
        ui.horizontal(|ui| {
            ui.label("Physical Microphone:");
            let selected_label = self
                .config
                .selected_mic_device
                .as_deref()
                .and_then(|n| {
                    mics.iter()
                        .find(|m| m.name == n)
                        .map(|m| m.description.as_str())
                })
                .unwrap_or("System Default");

            let mut changed_mic: Option<Option<String>> = None;
            egui::ComboBox::from_id_salt("pref_mic_combo")
                .selected_text(selected_label)
                .width(270.0)
                .show_ui(ui, |ui| {
                    if ui
                        .selectable_label(
                            self.config.selected_mic_device.is_none(),
                            "System Default",
                        )
                        .clicked()
                    {
                        changed_mic = Some(None);
                    }
                    for mic in mics {
                        let sel =
                            self.config.selected_mic_device.as_deref() == Some(mic.name.as_str());
                        if ui.selectable_label(sel, &mic.description).clicked() {
                            changed_mic = Some(Some(mic.name.clone()));
                        }
                    }
                });

            if let Some(new_mic) = changed_mic {
                self.config.selected_mic_device = new_mic;
                if let Err(e) = self
                    .pipewire
                    .setup(self.config.selected_mic_device.as_deref())
                {
                    self.pipewire.status_message = format!("PipeWire setup error: {e}");
                }
                self.dirty_config = true;
            }
        });

        ui.add_space(4.0);

        // Output device selector (cpal)
        ui.horizontal(|ui| {
            ui.label("Output Device (Headphones):");
            let current = self
                .config
                .selected_output_device
                .as_deref()
                .unwrap_or("System Default");
            let mut changed_out: Option<Option<String>> = None;
            egui::ComboBox::from_id_salt("pref_out_combo")
                .selected_text(current)
                .width(245.0)
                .show_ui(ui, |ui| {
                    if ui
                        .selectable_label(
                            self.config.selected_output_device.is_none(),
                            "System Default",
                        )
                        .clicked()
                    {
                        changed_out = Some(None);
                    }
                    for dev in cpal_outputs {
                        let sel =
                            self.config.selected_output_device.as_deref() == Some(dev.as_str());
                        if ui.selectable_label(sel, dev).clicked() {
                            changed_out = Some(Some(dev.clone()));
                        }
                    }
                });
            if let Some(new_out) = changed_out {
                self.config.selected_output_device = new_out;
                if let Err(e) = self
                    .audio
                    .set_output_device(self.config.selected_output_device.as_deref())
                {
                    self.status_toast = Some(e);
                }
                self.dirty_config = true;
            }
        });

        ui.add_space(8.0);
        ui.label(
            RichText::new(format!("PipeWire: {}", self.pipewire.status_message))
                .size(11.5)
                .color(palette.muted_text),
        );
        ui.add_space(4.0);
        if ui.button("Reconnect Virtual Microphone").clicked() {
            if let Err(e) = self
                .pipewire
                .setup(self.config.selected_mic_device.as_deref())
            {
                self.pipewire.status_message = format!("PipeWire setup error: {e}");
            }
        }
    }

    fn render_preferences_recording_tab(&mut self, ui: &mut Ui, palette: &ThemePalette) {
        ui.heading("Audio Recording");
        ui.add_space(6.0);

        ui.label("Recording Source:");
        ui.add_space(2.0);
        let sources = [
            (
                RecordingSource::Microphone,
                "Microphone (what others hear)",
            ),
            (
                RecordingSource::SystemAudio,
                "Speakers / System Audio (what you hear)",
            ),
            (
                RecordingSource::Both,
                "Both (Microphone + Speakers)",
            ),
        ];
        for (src, label) in sources {
            if ui
                .radio_value(&mut self.config.recording_source, src, label)
                .changed()
            {
                self.dirty_config = true;
            }
        }

        ui.add_space(8.0);
        ui.separator();
        ui.add_space(6.0);

        ui.label("Save Folder:");
        ui.add_space(2.0);
        ui.horizontal(|ui| {
            ui.label(
                RichText::new(self.config.recording_dir.display().to_string())
                    .monospace()
                    .size(11.5)
                    .color(palette.muted_text),
            );
            if ui.button("Browse...").clicked() {
                if let Some(folder) = rfd::FileDialog::new().pick_folder() {
                    self.config.recording_dir = folder;
                    self.dirty_config = true;
                }
            }
        });
    }
}

fn render_hotkey_binding_row(
    ui: &mut Ui,
    palette: &ThemePalette,
    label: &str,
    binding: &mut Option<Hotkey>,
    capturing: &mut Option<HotkeyCaptureTarget>,
    target: HotkeyCaptureTarget,
) -> bool {
    let mut changed = false;
    ui.label(label);

    let is_capturing = *capturing == Some(target);
    let current_text = binding
        .as_ref()
        .map(|h| h.display())
        .unwrap_or_else(|| "Not assigned".to_string());

    ui.label(
        RichText::new(if is_capturing {
            "Press a key..."
        } else {
            current_text.as_str()
        })
        .strong()
        .color(if is_capturing {
            palette.accent
        } else if binding.is_some() {
            palette.text
        } else {
            palette.muted_text
        }),
    );

    let assign_label = if is_capturing { "Cancel" } else { "Assign" };
    if ui.button(assign_label).clicked() {
        *capturing = if is_capturing { None } else { Some(target) };
        changed = true;
    }

    if ui
        .add_enabled(binding.is_some(), egui::Button::new("✕"))
        .on_hover_text("Clear hotkey")
        .clicked()
    {
        *binding = None;
        if is_capturing {
            *capturing = None;
        }
        changed = true;
    }

    ui.end_row();
    changed
}

fn toolbar_icon_btn(
    ui: &mut Ui,
    palette: &ThemePalette,
    active: bool,
    tooltip: &str,
    paint_fn: fn(&egui::Painter, Rect, Color32),
) -> Response {
    let (rect, resp) = ui.allocate_exact_size(vec2(26.0, 26.0), Sense::click());
    if active {
        ui.painter()
            .rect_filled(rect, CornerRadius::same(3), palette.accent);
    } else if resp.hovered() {
        ui.painter()
            .rect_filled(rect, CornerRadius::same(3), palette.row_hover);
    }
    let icon_color = if active {
        Color32::WHITE
    } else {
        palette.accent
    };
    paint_fn(ui.painter(), rect, icon_color);
    resp.on_hover_text(tooltip)
}

fn status_badge_toggle(
    ui: &mut Ui,
    palette: &ThemePalette,
    label: &str,
    active: bool,
    tooltip: &str,
) -> Response {
    let (rect, resp) = ui.allocate_exact_size(vec2(17.0, 17.0), Sense::click());
    let bg = if active {
        palette.accent
    } else {
        palette.inactive_icon
    };
    ui.painter().rect_filled(rect, CornerRadius::same(2), bg);
    ui.painter().text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        label,
        FontId::proportional(11.5),
        Color32::WHITE,
    );
    resp.on_hover_text(tooltip)
}

fn paint_blue_slider(
    ui: &mut Ui,
    palette: &ThemePalette,
    value: &mut f32,
    range: std::ops::RangeInclusive<f32>,
    width: f32,
    enabled: bool,
) -> Response {
    let (rect, mut resp) = ui.allocate_exact_size(
        vec2(width, 20.0),
        if enabled {
            Sense::click_and_drag()
        } else {
            Sense::hover()
        },
    );

    let min = *range.start();
    let max = *range.end();
    let span = (max - min).max(1e-5);

    let track_h = 4.0;
    let track_rect = Rect::from_center_size(rect.center(), vec2(rect.width() - 12.0, track_h));

    if enabled && (resp.dragged() || resp.clicked()) {
        if let Some(pos) = resp.interact_pointer_pos() {
            let t = ((pos.x - track_rect.left()) / track_rect.width().max(1.0)).clamp(0.0, 1.0);
            *value = min + t * span;
            resp.mark_changed();
        }
    }

    let t = ((*value - min) / span).clamp(0.0, 1.0);
    let fill_w = track_rect.width() * t;
    let fill_rect = Rect::from_min_size(track_rect.min, vec2(fill_w, track_h));

    ui.painter()
        .rect_filled(track_rect, CornerRadius::same(2), palette.border);
    let accent = if enabled {
        palette.accent
    } else {
        palette.inactive_icon
    };
    ui.painter()
        .rect_filled(fill_rect, CornerRadius::same(2), accent);

    let knob_x = track_rect.left() + fill_w;
    ui.painter()
        .circle_filled(pos2(knob_x, track_rect.center().y), 6.0, accent);

    resp
}

const SUPPORTED_MEDIA_EXTENSIONS: &[&str] = &[
    "mp3", "wav", "ogg", "flac", "mp4", "mkv", "webm", "mov", "m4a", "avi",
];

fn is_supported_audio(path: &Path) -> bool {
    let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
        return false;
    };
    let lower = ext.to_ascii_lowercase();
    SUPPORTED_MEDIA_EXTENSIONS.contains(&lower.as_str())
}

fn collect_audio_files_recursive(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_audio_files_recursive(&path, out);
        } else if is_supported_audio(&path) {
            out.push(path);
        }
    }
}

fn encode_file_uri(path: &Path) -> String {
    let raw = path.to_string_lossy();
    let mut uri = String::with_capacity(7 + raw.len() * 2);
    uri.push_str("file://");
    for &b in raw.as_bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~' | b'/') {
            uri.push(b as char);
        } else {
            uri.push('%');
            uri.push(char::from_digit((b >> 4) as u32, 16).unwrap_or('0').to_ascii_uppercase());
            uri.push(char::from_digit((b & 0x0f) as u32, 16).unwrap_or('0').to_ascii_uppercase());
        }
    }
    uri
}

fn open_in_folder(path: &Path) {
    let path = path.to_path_buf();
    thread::spawn(move || {
        let canonical = std::fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
        let uri_arg = format!("array:string:{}", encode_file_uri(&canonical));
        let dbus_ok = std::process::Command::new("dbus-send")
            .args([
                "--session",
                "--print-reply",
                "--reply-timeout=1500",
                "--dest=org.freedesktop.FileManager1",
                "--type=method_call",
                "/org/freedesktop/FileManager1",
                "org.freedesktop.FileManager1.ShowItems",
                &uri_arg,
                "string:",
            ])
            .output()
            .is_ok_and(|out| out.status.success());

        if !dbus_ok {
            let parent = canonical
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or_else(|| Path::new("."));
            let _ = std::process::Command::new("xdg-open").arg(parent).spawn();
        }
    });
}
