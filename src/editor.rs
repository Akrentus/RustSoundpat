use crate::audio::{decode_audio_file, soft_limit, TARGET_CHANNELS, TARGET_SAMPLE_RATE};
use crate::recorder::{build_wav_header, finalize_wav_header};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{SampleFormat, Stream, StreamConfig};
use egui::{
    pos2, vec2, Align, Color32, Context, CornerRadius, CursorIcon, FontId, Key, Modifiers, Rect,
    RichText, Sense, Stroke, Vec2,
};
use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const PEAK_BLOCK_FRAMES: usize = 256;
const MIN_GAIN_DB: f32 = -24.0;
const MAX_GAIN_DB: f32 = 24.0;

#[derive(Debug, Clone)]
pub enum EditorAction {
    Saved {
        sound_id: u64,
        old_path: PathBuf,
        new_path: PathBuf,
        duration_secs: f32,
    },
    SavedAs {
        path: PathBuf,
        duration_secs: f32,
    },
    Close,
}

#[derive(Clone)]
struct EditorSnapshot {
    samples: Vec<f32>,
    gain_db: f32,
    selection: Option<(usize, usize)>,
    cursor_frame: usize,
    revision: u64,
    samples_revision: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WaveformDrag {
    NewSelection { anchor_frame: usize },
    MoveStart,
    MoveEnd,
}

struct PreviewBuffer {
    samples: Arc<Vec<f32>>,
    end_frame: usize,
}

struct PreviewShared {
    buf: Mutex<PreviewBuffer>,
    pos_frame: AtomicUsize,
    playing: AtomicBool,
    gain_bits: AtomicU32,
}

struct EditorPreviewPlayer {
    shared: Arc<PreviewShared>,
    stream: Option<Stream>,
    output_device: Option<String>,
}

impl EditorPreviewPlayer {
    fn new(output_device: Option<&str>) -> Self {
        Self {
            shared: Arc::new(PreviewShared {
                buf: Mutex::new(PreviewBuffer {
                    samples: Arc::new(Vec::new()),
                    end_frame: 0,
                }),
                pos_frame: AtomicUsize::new(0),
                playing: AtomicBool::new(false),
                gain_bits: AtomicU32::new(1.0f32.to_bits()),
            }),
            stream: None,
            output_device: output_device.map(|s| s.to_string()),
        }
    }

    fn set_gain(&self, gain_linear: f32) {
        self.shared
            .gain_bits
            .store(gain_linear.to_bits(), Ordering::Relaxed);
    }

    fn is_playing(&self) -> bool {
        self.shared.playing.load(Ordering::Relaxed)
    }

    fn current_frame(&self) -> usize {
        self.shared.pos_frame.load(Ordering::Relaxed)
    }

    fn stop(&self) {
        self.shared.playing.store(false, Ordering::Relaxed);
    }

    fn play_range(
        &mut self,
        samples: &[f32],
        start_frame: usize,
        end_frame: usize,
        gain_linear: f32,
    ) -> Result<(), String> {
        let total_frames = samples.len() / TARGET_CHANNELS;
        let start = start_frame.min(total_frames);
        let end = end_frame.min(total_frames);
        if start >= end {
            self.stop();
            return Ok(());
        }

        if self.stream.is_none() {
            self.ensure_stream()?;
        }

        self.shared
            .gain_bits
            .store(gain_linear.to_bits(), Ordering::Relaxed);

        if let Ok(mut guard) = self.shared.buf.lock() {
            guard.samples = Arc::new(samples.to_vec());
            guard.end_frame = end;
        }
        self.shared.pos_frame.store(start, Ordering::Relaxed);
        self.shared.playing.store(true, Ordering::Relaxed);
        Ok(())
    }

    fn ensure_stream(&mut self) -> Result<(), String> {
        let host = cpal::default_host();
        let device = if let Some(ref name) = self.output_device.as_deref().filter(|s| !s.is_empty())
        {
            host.output_devices()
                .ok()
                .and_then(|mut devs| devs.find(|d| d.name().ok().as_deref() == Some(*name)))
                .or_else(|| host.default_output_device())
        } else {
            host.default_output_device()
        }
        .ok_or_else(|| "No cpal audio output device found for preview".to_string())?;

        let supported = device
            .default_output_config()
            .map_err(|e| format!("Failed to get preview output config: {e}"))?;

        let out_channels = supported.channels().max(1) as usize;
        let out_rate = supported.sample_rate().0;
        let sample_format = supported.sample_format();
        let config = StreamConfig {
            channels: supported.channels(),
            sample_rate: supported.sample_rate(),
            buffer_size: cpal::BufferSize::Default,
        };

        let shared = Arc::clone(&self.shared);
        let ratio = TARGET_SAMPLE_RATE as f64 / out_rate.max(1) as f64;
        let mut frac_pos = 0.0f64;

        let mut next_stereo = move || -> (f32, f32) {
            if !shared.playing.load(Ordering::Relaxed) {
                frac_pos = 0.0;
                return (0.0, 0.0);
            }

            let Ok(guard) = shared.buf.try_lock() else {
                return (0.0, 0.0);
            };

            let pos = shared.pos_frame.load(Ordering::Relaxed);
            if pos >= guard.end_frame || guard.samples.len() < TARGET_CHANNELS {
                shared.playing.store(false, Ordering::Relaxed);
                frac_pos = 0.0;
                return (0.0, 0.0);
            }

            let gain = f32::from_bits(shared.gain_bits.load(Ordering::Relaxed));
            let max_frame = (guard.samples.len() / TARGET_CHANNELS).saturating_sub(1);

            if (ratio - 1.0).abs() < 1e-4 {
                let idx = pos.min(max_frame) * TARGET_CHANNELS;
                let l = soft_limit(guard.samples[idx] * gain);
                let r = soft_limit(guard.samples[idx + 1] * gain);
                let next = pos + 1;
                shared.pos_frame.store(next, Ordering::Relaxed);
                if next >= guard.end_frame {
                    shared.playing.store(false, Ordering::Relaxed);
                }
                return (l, r);
            }

            let f0 = pos.min(max_frame);
            let f1 = (pos + 1).min(guard.end_frame.saturating_sub(1)).min(max_frame);
            let frac = frac_pos as f32;

            let i0 = f0 * TARGET_CHANNELS;
            let i1 = f1 * TARGET_CHANNELS;
            let l0 = guard.samples[i0];
            let r0 = guard.samples[i0 + 1];
            let l1 = guard.samples[i1];
            let r1 = guard.samples[i1 + 1];

            let l = soft_limit((l0 + (l1 - l0) * frac) * gain);
            let r = soft_limit((r0 + (r1 - r0) * frac) * gain);

            frac_pos += ratio;
            let advance = frac_pos.floor() as usize;
            if advance > 0 {
                frac_pos -= advance as f64;
                let next = pos.saturating_add(advance);
                shared.pos_frame.store(next, Ordering::Relaxed);
                if next >= guard.end_frame {
                    shared.playing.store(false, Ordering::Relaxed);
                    frac_pos = 0.0;
                }
            }

            (l, r)
        };

        let err_fn = |err| {
            eprintln!("Editor preview cpal stream error: {err}");
        };

        let stream = match sample_format {
            SampleFormat::F32 => device.build_output_stream(
                &config,
                move |data: &mut [f32], _| {
                    for frame in data.chunks_exact_mut(out_channels) {
                        let (l, r) = next_stereo();
                        if out_channels == 1 {
                            frame[0] = (l + r) * 0.5;
                        } else {
                            frame[0] = l;
                            frame[1] = r;
                            for ch in 2..out_channels {
                                frame[ch] = 0.0;
                            }
                        }
                    }
                },
                err_fn,
                None,
            ),
            SampleFormat::I16 => device.build_output_stream(
                &config,
                move |data: &mut [i16], _| {
                    let to_i16 = |s: f32| (s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16;
                    for frame in data.chunks_exact_mut(out_channels) {
                        let (l, r) = next_stereo();
                        if out_channels == 1 {
                            frame[0] = to_i16((l + r) * 0.5);
                        } else {
                            frame[0] = to_i16(l);
                            frame[1] = to_i16(r);
                            for ch in 2..out_channels {
                                frame[ch] = 0;
                            }
                        }
                    }
                },
                err_fn,
                None,
            ),
            SampleFormat::U16 => device.build_output_stream(
                &config,
                move |data: &mut [u16], _| {
                    let to_u16 =
                        |s: f32| ((s.clamp(-1.0, 1.0) * 0.5 + 0.5) * u16::MAX as f32) as u16;
                    for frame in data.chunks_exact_mut(out_channels) {
                        let (l, r) = next_stereo();
                        if out_channels == 1 {
                            frame[0] = to_u16((l + r) * 0.5);
                        } else {
                            frame[0] = to_u16(l);
                            frame[1] = to_u16(r);
                            for ch in 2..out_channels {
                                frame[ch] = to_u16(0.0);
                            }
                        }
                    }
                },
                err_fn,
                None,
            ),
            other => {
                return Err(format!("Unsupported preview sample format: {other:?}"));
            }
        }
        .map_err(|e| format!("Failed to build preview stream: {e}"))?;

        stream
            .play()
            .map_err(|e| format!("Failed to start preview stream: {e}"))?;
        self.stream = Some(stream);
        Ok(())
    }
}

pub struct SoundEditor {
    sound_id: u64,
    sound_tag: String,
    source_path: PathBuf,
    file_size_bytes: u64,
    format_label: String,
    samples: Vec<f32>,
    peaks_256: Vec<(f32, f32)>,
    gain_db: f32,
    gain_drag_snapshot: Option<EditorSnapshot>,
    selection: Option<(usize, usize)>,
    cursor_frame: usize,
    zoom: f32,
    scroll_offset: f32,
    drag_mode: Option<WaveformDrag>,
    undo_stack: Vec<EditorSnapshot>,
    redo_stack: Vec<EditorSnapshot>,
    revision: u64,
    saved_revision: u64,
    samples_revision: u64,
    saved_samples_revision: u64,
    next_rev_counter: u64,
    preview: EditorPreviewPlayer,
    status_message: Option<String>,
}

impl SoundEditor {
    pub fn open(
        sound_id: u64,
        sound_tag: String,
        path: PathBuf,
        output_device: Option<&str>,
    ) -> Result<Self, String> {
        let decoded = decode_audio_file(&path)?;
        let file_size_bytes = fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        let format_label = format_from_path(&path);
        let peaks_256 = build_peak_cache(&decoded.samples);

        Ok(Self {
            sound_id,
            sound_tag,
            source_path: path,
            file_size_bytes,
            format_label,
            samples: decoded.samples,
            peaks_256,
            gain_db: 0.0,
            gain_drag_snapshot: None,
            selection: None,
            cursor_frame: 0,
            zoom: 1.0,
            scroll_offset: 0.0,
            drag_mode: None,
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
            revision: 0,
            saved_revision: 0,
            samples_revision: 0,
            saved_samples_revision: 0,
            next_rev_counter: 1,
            preview: EditorPreviewPlayer::new(output_device),
            status_message: None,
        })
    }

    #[cfg(test)]
    pub fn from_samples_for_test(samples: Vec<f32>, path: PathBuf) -> Self {
        let peaks_256 = build_peak_cache(&samples);
        Self {
            sound_id: 1,
            sound_tag: "Test".to_string(),
            source_path: path,
            file_size_bytes: 0,
            format_label: "WAV".to_string(),
            samples,
            peaks_256,
            gain_db: 0.0,
            gain_drag_snapshot: None,
            selection: None,
            cursor_frame: 0,
            zoom: 1.0,
            scroll_offset: 0.0,
            drag_mode: None,
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
            revision: 0,
            saved_revision: 0,
            samples_revision: 0,
            saved_samples_revision: 0,
            next_rev_counter: 1,
            preview: EditorPreviewPlayer::new(None),
            status_message: None,
        }
    }

    pub fn total_frames(&self) -> usize {
        self.samples.len() / TARGET_CHANNELS
    }

    pub fn duration_secs(&self) -> f32 {
        self.total_frames() as f32 / TARGET_SAMPLE_RATE as f32
    }

    pub fn gain_linear(&self) -> f32 {
        10.0f32.powf(self.gain_db / 20.0)
    }

    pub fn has_unsaved_changes(&self) -> bool {
        self.samples_revision != self.saved_samples_revision || self.gain_db.abs() > 0.05
    }

    fn alloc_revision(&mut self) -> u64 {
        let r = self.next_rev_counter;
        self.next_rev_counter = self.next_rev_counter.saturating_add(1);
        r
    }

    fn snapshot(&self) -> EditorSnapshot {
        EditorSnapshot {
            samples: self.samples.clone(),
            gain_db: self.gain_db,
            selection: self.selection,
            cursor_frame: self.cursor_frame,
            revision: self.revision,
            samples_revision: self.samples_revision,
        }
    }

    fn restore_snapshot(&mut self, snap: EditorSnapshot) {
        self.preview.stop();
        self.samples = snap.samples;
        self.peaks_256 = build_peak_cache(&self.samples);
        self.gain_db = snap.gain_db;
        self.selection = snap.selection;
        self.cursor_frame = snap.cursor_frame.min(self.total_frames());
        self.revision = snap.revision;
        self.samples_revision = snap.samples_revision;
        self.clamp_scroll();
        self.preview.set_gain(self.gain_linear());
    }

    fn normalized_selection(&self) -> Option<(usize, usize)> {
        let (a, b) = self.selection?;
        let total = self.total_frames();
        let start = a.min(b).min(total);
        let end = a.max(b).min(total);
        if end > start {
            Some((start, end))
        } else {
            None
        }
    }

    pub fn trim_selection(&mut self) -> bool {
        let Some((start, end)) = self.normalized_selection() else {
            return false;
        };
        let total = self.total_frames();
        if start == 0 && end == total {
            return false;
        }
        self.preview.stop();
        self.undo_stack.push(self.snapshot());
        self.redo_stack.clear();

        let s_idx = start * TARGET_CHANNELS;
        let e_idx = end * TARGET_CHANNELS;
        self.samples = self.samples[s_idx..e_idx].to_vec();
        self.peaks_256 = build_peak_cache(&self.samples);
        self.selection = None;
        self.cursor_frame = 0;
        self.zoom = 1.0;
        self.scroll_offset = 0.0;
        self.revision = self.alloc_revision();
        self.samples_revision = self.revision;
        self.status_message = None;
        true
    }

    pub fn cut_selection(&mut self) -> bool {
        let Some((start, end)) = self.normalized_selection() else {
            return false;
        };
        self.preview.stop();
        self.undo_stack.push(self.snapshot());
        self.redo_stack.clear();

        let s_idx = start * TARGET_CHANNELS;
        let e_idx = end * TARGET_CHANNELS;
        self.samples.drain(s_idx..e_idx);
        self.peaks_256 = build_peak_cache(&self.samples);
        self.selection = None;
        self.cursor_frame = start.min(self.total_frames());
        self.clamp_scroll();
        self.revision = self.alloc_revision();
        self.samples_revision = self.revision;
        self.status_message = None;
        true
    }

    pub fn undo(&mut self) -> bool {
        let Some(prev) = self.undo_stack.pop() else {
            return false;
        };
        self.redo_stack.push(self.snapshot());
        self.restore_snapshot(prev);
        true
    }

    pub fn redo(&mut self) -> bool {
        let Some(next) = self.redo_stack.pop() else {
            return false;
        };
        self.undo_stack.push(self.snapshot());
        self.restore_snapshot(next);
        true
    }

    fn clamp_scroll(&mut self) {
        let visible_frac = (1.0 / self.zoom.max(1.0)).clamp(0.001, 1.0);
        let max_scroll = (1.0 - visible_frac).max(0.0);
        self.scroll_offset = self.scroll_offset.clamp(0.0, max_scroll);
    }

    fn export_samples_with_gain(&self) -> Vec<f32> {
        let gain = self.gain_linear();
        if (gain - 1.0).abs() <= 1e-4 {
            return self.samples.clone();
        }
        self.samples
            .iter()
            .map(|&s| soft_limit(s * gain))
            .collect()
    }

    fn play_all_or_from_cursor(&mut self) {
        let total = self.total_frames();
        if total == 0 {
            return;
        }
        let start = if self.cursor_frame >= total {
            0
        } else {
            self.cursor_frame
        };
        if let Err(e) = self
            .preview
            .play_range(&self.samples, start, total, self.gain_linear())
        {
            self.status_message = Some(e);
        }
    }

    fn play_selection(&mut self) {
        let Some((start, end)) = self.normalized_selection() else {
            return;
        };
        if let Err(e) = self
            .preview
            .play_range(&self.samples, start, end, self.gain_linear())
        {
            self.status_message = Some(e);
        }
    }

    fn save_in_place(&mut self) -> Result<EditorAction, String> {
        self.preview.stop();
        let processed = self.export_samples_with_gain();
        let old_path = self.source_path.clone();
        let ext = old_path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase())
            .unwrap_or_default();

        let saved_path = if ext == "wav" {
            write_wav_pcm16(&old_path, &processed)?;
            old_path.clone()
        } else {
            match save_with_ffmpeg(&old_path, &processed) {
                Ok(()) => old_path.clone(),
                Err(ff_err) => {
                    let wav_path = old_path.with_extension("wav");
                    write_wav_pcm16(&wav_path, &processed).map_err(|wav_err| {
                        format!("ffmpeg failed ({ff_err}) and WAV fallback failed ({wav_err})")
                    })?;
                    self.status_message = Some(format!(
                        "FFmpeg unavailable/failed ({ff_err}); saved as {}",
                        wav_path
                            .file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or_else(|| "WAV".to_string())
                    ));
                    wav_path
                }
            }
        };

        self.samples = processed;
        self.peaks_256 = build_peak_cache(&self.samples);
        self.gain_db = 0.0;
        self.preview.set_gain(1.0);
        self.source_path = saved_path.clone();
        self.file_size_bytes = fs::metadata(&saved_path).map(|m| m.len()).unwrap_or(0);
        self.format_label = format_from_path(&saved_path);
        let rev = self.alloc_revision();
        self.revision = rev;
        self.saved_revision = rev;
        self.samples_revision = rev;
        self.saved_samples_revision = rev;

        Ok(EditorAction::Saved {
            sound_id: self.sound_id,
            old_path,
            new_path: saved_path,
            duration_secs: self.duration_secs(),
        })
    }

    fn save_as_dialog(&mut self) -> Result<Option<EditorAction>, String> {
        let default_stem = self
            .source_path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| self.sound_tag.clone());
        let default_name = format!("{default_stem}_edited.wav");

        let mut dlg = rfd::FileDialog::new()
            .set_title("Save Sound As")
            .add_filter("WAV Audio", &["wav"])
            .set_file_name(&default_name);
        if let Some(parent) = self.source_path.parent() {
            dlg = dlg.set_directory(parent);
        }

        let Some(mut chosen) = dlg.save_file() else {
            return Ok(None);
        };
        if chosen.extension().is_none() {
            chosen.set_extension("wav");
        }

        self.preview.stop();
        let processed = self.export_samples_with_gain();
        write_wav_pcm16(&chosen, &processed)?;

        let dur = (processed.len() / TARGET_CHANNELS) as f32 / TARGET_SAMPLE_RATE as f32;
        Ok(Some(EditorAction::SavedAs {
            path: chosen,
            duration_secs: dur,
        }))
    }

    pub fn ui(&mut self, ctx: &Context, dark_mode: bool) -> Option<EditorAction> {
        if self.preview.is_playing() {
            ctx.request_repaint_after(Duration::from_millis(25));
        }

        let mut action: Option<EditorAction> = None;
        let mut open = true;
        let title = format!("Sound Editor — {}", self.sound_tag);

        let bg_canvas = if dark_mode {
            Color32::from_rgb(24, 27, 33)
        } else {
            Color32::from_rgb(245, 247, 250)
        };
        let border_col = if dark_mode {
            Color32::from_rgb(66, 70, 78)
        } else {
            Color32::from_rgb(198, 204, 212)
        };
        let wave_col = if dark_mode {
            Color32::from_rgb(76, 166, 255)
        } else {
            Color32::from_rgb(25, 118, 210)
        };
        let sel_fill = Color32::from_rgba_unmultiplied(30, 136, 229, 55);
        let sel_edge = Color32::from_rgb(255, 179, 0);
        let ruler_text_col = if dark_mode {
            Color32::from_rgb(180, 186, 196)
        } else {
            Color32::from_rgb(80, 86, 96)
        };

        egui::Window::new(title)
            .open(&mut open)
            .collapsible(false)
            .resizable(true)
            .default_size(vec2(780.0, 440.0))
            .min_size(vec2(580.0, 340.0))
            .anchor(egui::Align2::CENTER_CENTER, Vec2::ZERO)
            .show(ctx, |ui| {
                // Editor keyboard shortcuts when window is shown
                if !ctx.wants_keyboard_input() {
                    let (ctrl_z, ctrl_y, ctrl_shift_z, space, del) = ctx.input_mut(|i| {
                        (
                            i.consume_key(Modifiers::CTRL, Key::Z),
                            i.consume_key(Modifiers::CTRL, Key::Y),
                            i.consume_key(Modifiers::CTRL | Modifiers::SHIFT, Key::Z),
                            i.consume_key(Modifiers::NONE, Key::Space),
                            i.consume_key(Modifiers::NONE, Key::Delete),
                        )
                    });
                    if ctrl_shift_z || ctrl_y {
                        self.redo();
                    } else if ctrl_z {
                        self.undo();
                    }
                    if space {
                        if self.preview.is_playing() {
                            self.preview.stop();
                        } else {
                            self.play_all_or_from_cursor();
                        }
                    }
                    if del {
                        self.cut_selection();
                    }
                }

                let has_selection = self.normalized_selection().is_some();
                let is_playing = self.preview.is_playing();
                let total_frames = self.total_frames();

                // Top Toolbar
                ui.horizontal_wrapped(|ui| {
                    ui.spacing_mut().item_spacing.x = 5.0;

                    if ui
                        .add_enabled(total_frames > 0, egui::Button::new("Play"))
                        .on_hover_text("Play from cursor (Space)")
                        .clicked()
                    {
                        self.play_all_or_from_cursor();
                    }

                    if ui
                        .add_enabled(has_selection, egui::Button::new("Play Selection"))
                        .on_hover_text("Play only the selected range")
                        .clicked()
                    {
                        self.play_selection();
                    }

                    if ui
                        .add_enabled(is_playing, egui::Button::new("Stop"))
                        .on_hover_text("Stop preview playback")
                        .clicked()
                    {
                        self.preview.stop();
                    }

                    ui.separator();

                    let can_trim = self
                        .normalized_selection()
                        .is_some_and(|(s, e)| s > 0 || e < total_frames);
                    if ui
                        .add_enabled(can_trim, egui::Button::new("✂ Trim"))
                        .on_hover_text("Keep only the selected segment")
                        .clicked()
                    {
                        self.trim_selection();
                    }

                    if ui
                        .add_enabled(has_selection, egui::Button::new("Cut"))
                        .on_hover_text("Delete the selected segment (Del)")
                        .clicked()
                    {
                        self.cut_selection();
                    }

                    ui.separator();

                    if ui
                        .add_enabled(!self.undo_stack.is_empty(), egui::Button::new("Undo"))
                        .on_hover_text("Undo (Ctrl+Z)")
                        .clicked()
                    {
                        self.undo();
                    }
                    if ui
                        .add_enabled(!self.redo_stack.is_empty(), egui::Button::new("Redo"))
                        .on_hover_text("Redo (Ctrl+Y)")
                        .clicked()
                    {
                        self.redo();
                    }

                    ui.separator();

                    ui.label("Gain:");
                    let gain_before = self.gain_db;
                    let gain_slider = ui.add(
                        egui::Slider::new(&mut self.gain_db, MIN_GAIN_DB..=MAX_GAIN_DB)
                            .show_value(false)
                            .step_by(0.1),
                    );
                    if gain_slider.drag_started() {
                        let mut snap = self.snapshot();
                        snap.gain_db = gain_before;
                        self.gain_drag_snapshot = Some(snap);
                    }
                    if gain_slider.changed() {
                        if self.gain_drag_snapshot.is_none() && (gain_before - self.gain_db).abs() > 0.01 {
                            let mut snap = self.snapshot();
                            snap.gain_db = gain_before;
                            self.undo_stack.push(snap);
                            self.redo_stack.clear();
                        }
                        self.revision = self.alloc_revision();
                        self.preview.set_gain(self.gain_linear());
                    }
                    if gain_slider.drag_stopped() {
                        if let Some(prev) = self.gain_drag_snapshot.take() {
                            if (prev.gain_db - self.gain_db).abs() > 0.01 {
                                self.undo_stack.push(prev);
                                self.redo_stack.clear();
                            }
                        }
                    }

                    let pct = self.gain_linear() * 100.0;
                    ui.label(
                        RichText::new(format!("{:+.1} dB ({:.0}%)", self.gain_db, pct))
                            .monospace()
                            .size(12.0),
                    );
                    if ui
                        .add_enabled(self.gain_db.abs() > 0.05, egui::Button::new("0 dB"))
                        .on_hover_text("Reset gain to 0 dB (100%)")
                        .clicked()
                    {
                        self.undo_stack.push(self.snapshot());
                        self.redo_stack.clear();
                        self.gain_db = 0.0;
                        self.revision = self.alloc_revision();
                        self.preview.set_gain(1.0);
                    }

                    ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                        if ui.button("+").on_hover_text("Zoom in").clicked() {
                            self.zoom = (self.zoom * 1.5).min(128.0);
                            self.clamp_scroll();
                        }
                        ui.label(
                            RichText::new(format!("{:.1}x", self.zoom))
                                .monospace()
                                .size(12.0),
                        );
                        if ui
                            .add_enabled(self.zoom > 1.01, egui::Button::new("-"))
                            .on_hover_text("Zoom out")
                            .clicked()
                        {
                            self.zoom = (self.zoom / 1.5).max(1.0);
                            self.clamp_scroll();
                        }
                    });
                });

                ui.add_space(4.0);

                // Main Waveform + Time Ruler area
                let avail_h = (ui.available_height() - 82.0).max(140.0);
                let ruler_h = 22.0;
                let scroll_h = 14.0;
                let wave_h = (avail_h - ruler_h - scroll_h - 6.0).max(90.0);

                let total_secs = self.duration_secs();
                let visible_frac = (1.0 / self.zoom.max(1.0)).clamp(0.001, 1.0);
                self.clamp_scroll();
                let view_start_frac = self.scroll_offset;
                let view_end_frac = (view_start_frac + visible_frac).min(1.0);

                let view_start_frame =
                    ((view_start_frac as f64) * total_frames as f64).floor() as usize;
                let view_end_frame =
                    (((view_end_frac as f64) * total_frames as f64).ceil() as usize)
                        .clamp(view_start_frame, total_frames);
                let view_span_frames = view_end_frame.saturating_sub(view_start_frame).max(1);

                // 1. Time Ruler
                let (ruler_rect, _) =
                    ui.allocate_exact_size(vec2(ui.available_width(), ruler_h), Sense::hover());
                let p = ui.painter_at(ruler_rect);
                p.rect_filled(ruler_rect, CornerRadius::ZERO, bg_canvas);
                p.line_segment(
                    [
                        pos2(ruler_rect.left(), ruler_rect.bottom()),
                        pos2(ruler_rect.right(), ruler_rect.bottom()),
                    ],
                    Stroke::new(1.0_f32, border_col),
                );

                if total_secs > 0.0 {
                    let view_start_secs = view_start_frac * total_secs;
                    let view_span_secs = (view_end_frac - view_start_frac) * total_secs;
                    let step = choose_ruler_step(view_span_secs);
                    let first_tick = (view_start_secs / step).floor() as i64;
                    let last_tick = ((view_start_secs + view_span_secs) / step).ceil() as i64;

                    for k in first_tick..=last_tick {
                        let t = k as f32 * step;
                        if t < 0.0 || t > total_secs + 1e-4 {
                            continue;
                        }
                        let norm_x = (t - view_start_secs) / view_span_secs.max(1e-5);
                        let x = ruler_rect.left() + norm_x * ruler_rect.width();
                        p.line_segment(
                            [pos2(x, ruler_rect.bottom() - 7.0), pos2(x, ruler_rect.bottom())],
                            Stroke::new(1.0_f32, ruler_text_col),
                        );
                        let label = format_ruler_time(t);
                        p.text(
                            pos2(x + 3.0, ruler_rect.top() + 2.0),
                            egui::Align2::LEFT_TOP,
                            label,
                            FontId::monospace(10.5),
                            ruler_text_col,
                        );
                    }
                }

                // 2. Waveform Canvas
                let (wave_rect, wave_resp) = ui.allocate_exact_size(
                    vec2(ui.available_width(), wave_h),
                    Sense::click_and_drag(),
                );

                // Handle zoom / horizontal pan with scroll wheel while hovering waveform
                if wave_resp.hovered() {
                    let (scroll_delta, ctrl_down) = ui.input(|i| {
                        (
                            i.smooth_scroll_delta,
                            i.modifiers.ctrl || i.modifiers.command,
                        )
                    });
                    if ctrl_down && scroll_delta.y.abs() > 0.1 {
                        let old_zoom = self.zoom;
                        let factor = if scroll_delta.y > 0.0 { 1.18 } else { 1.0 / 1.18 };
                        self.zoom = (self.zoom * factor).clamp(1.0, 128.0);
                        if (self.zoom - old_zoom).abs() > 1e-4 {
                            let cursor_norm = wave_resp
                                .hover_pos()
                                .map(|pos| {
                                    ((pos.x - wave_rect.left()) / wave_rect.width().max(1.0))
                                        .clamp(0.0, 1.0)
                                })
                                .unwrap_or(0.5);
                            let anchor = view_start_frac + cursor_norm / old_zoom;
                            self.scroll_offset = anchor - cursor_norm / self.zoom;
                            self.clamp_scroll();
                        }
                    } else if self.zoom > 1.0 && (scroll_delta.x.abs() > 0.1 || scroll_delta.y.abs() > 0.1)
                    {
                        let delta = if scroll_delta.x.abs() > scroll_delta.y.abs() {
                            -scroll_delta.x
                        } else {
                            -scroll_delta.y
                        };
                        self.scroll_offset += (delta / wave_rect.width().max(1.0)) / self.zoom;
                        self.clamp_scroll();
                    }
                }

                let frame_at_x = |x: f32| -> usize {
                    let t = ((x - wave_rect.left()) / wave_rect.width().max(1.0)).clamp(0.0, 1.0);
                    let f = view_start_frame as f64 + (t as f64) * (view_span_frames as f64);
                    (f.round() as usize).min(total_frames)
                };
                let x_at_frame = |frame: usize| -> f32 {
                    let t = (frame as f64 - view_start_frame as f64) / (view_span_frames as f64);
                    wave_rect.left() + (t as f32) * wave_rect.width()
                };

                // Selection & edge drag interaction
                let current_sel = self.normalized_selection();
                if let Some(hover_pos) = wave_resp.hover_pos() {
                    if let Some((s0, s1)) = current_sel {
                        let x0 = x_at_frame(s0);
                        let x1 = x_at_frame(s1);
                        if (hover_pos.x - x0).abs() <= 6.0 || (hover_pos.x - x1).abs() <= 6.0 {
                            ctx.set_cursor_icon(CursorIcon::ResizeHorizontal);
                        }
                    }
                }

                if wave_resp.drag_started() {
                    if let Some(pos) = wave_resp.interact_pointer_pos() {
                        let mode = if let Some((s0, s1)) = current_sel {
                            let x0 = x_at_frame(s0);
                            let x1 = x_at_frame(s1);
                            if (pos.x - x0).abs() <= 7.0 {
                                WaveformDrag::MoveStart
                            } else if (pos.x - x1).abs() <= 7.0 {
                                WaveformDrag::MoveEnd
                            } else {
                                let f = frame_at_x(pos.x);
                                self.selection = Some((f, f));
                                self.cursor_frame = f;
                                WaveformDrag::NewSelection { anchor_frame: f }
                            }
                        } else {
                            let f = frame_at_x(pos.x);
                            self.selection = Some((f, f));
                            self.cursor_frame = f;
                            WaveformDrag::NewSelection { anchor_frame: f }
                        };
                        self.drag_mode = Some(mode);
                    }
                }

                if wave_resp.dragged() {
                    if let (Some(mode), Some(pos)) = (self.drag_mode, wave_resp.interact_pointer_pos())
                    {
                        let f = frame_at_x(pos.x);
                        match mode {
                            WaveformDrag::NewSelection { anchor_frame } => {
                                self.selection = Some((anchor_frame.min(f), anchor_frame.max(f)));
                                self.cursor_frame = anchor_frame.min(f);
                            }
                            WaveformDrag::MoveStart => {
                                if let Some((_, end)) = self.selection {
                                    self.selection = Some((f.min(end), f.max(end)));
                                    self.cursor_frame = f.min(end);
                                }
                            }
                            WaveformDrag::MoveEnd => {
                                if let Some((start, _)) = self.selection {
                                    self.selection = Some((start.min(f), start.max(f)));
                                    self.cursor_frame = start.min(f);
                                }
                            }
                        }
                    }
                }

                if wave_resp.drag_stopped() {
                    self.drag_mode = None;
                    if let Some((a, b)) = self.selection {
                        if a == b {
                            self.selection = None;
                        }
                    }
                }

                if wave_resp.clicked() {
                    if let Some(pos) = wave_resp.interact_pointer_pos() {
                        let f = frame_at_x(pos.x);
                        self.cursor_frame = f;
                        self.selection = None;
                    }
                }

                // Paint waveform
                let wp = ui.painter_at(wave_rect);
                wp.rect_filled(wave_rect, CornerRadius::ZERO, bg_canvas);
                wp.rect_stroke(
                    wave_rect,
                    CornerRadius::ZERO,
                    Stroke::new(1.0_f32, border_col),
                    egui::StrokeKind::Inside,
                );

                let mid_y = wave_rect.center().y;
                wp.line_segment(
                    [
                        pos2(wave_rect.left(), mid_y),
                        pos2(wave_rect.right(), mid_y),
                    ],
                    Stroke::new(1.0_f32, border_col.gamma_multiply(0.6)),
                );

                if total_frames > 0 && wave_rect.width() >= 2.0 {
                    let gain = self.gain_linear();
                    let half_h = (wave_rect.height() * 0.46).max(2.0);
                    let width_px = wave_rect.width().floor() as usize;

                    for px in 0..width_px {
                        let f0 = view_start_frame
                            + (px * view_span_frames) / width_px.max(1);
                        let f1 = (view_start_frame
                            + ((px + 1) * view_span_frames) / width_px.max(1))
                        .max(f0 + 1)
                        .min(total_frames);

                        let (min_v, max_v) =
                            peak_range(&self.samples, &self.peaks_256, f0, f1);
                        let y_top = mid_y - (max_v * gain).clamp(-1.0, 1.0) * half_h;
                        let y_bot = mid_y - (min_v * gain).clamp(-1.0, 1.0) * half_h;
                        let x = wave_rect.left() + px as f32 + 0.5;
                        wp.line_segment(
                            [pos2(x, y_top), pos2(x, y_bot.max(y_top + 1.0))],
                            Stroke::new(1.0_f32, wave_col),
                        );
                    }
                }

                // Paint selection overlay & draggable handles
                if let Some((s0, s1)) = self.normalized_selection() {
                    let x0 = x_at_frame(s0).clamp(wave_rect.left(), wave_rect.right());
                    let x1 = x_at_frame(s1).clamp(wave_rect.left(), wave_rect.right());
                    if x1 > x0 {
                        let sel_rect = Rect::from_min_max(
                            pos2(x0, wave_rect.top()),
                            pos2(x1, wave_rect.bottom()),
                        );
                        wp.rect_filled(sel_rect, CornerRadius::ZERO, sel_fill);
                    }
                    for hx in [x_at_frame(s0), x_at_frame(s1)] {
                        if hx >= wave_rect.left() - 1.0 && hx <= wave_rect.right() + 1.0 {
                            wp.line_segment(
                                [pos2(hx, wave_rect.top()), pos2(hx, wave_rect.bottom())],
                                Stroke::new(2.0_f32, sel_edge),
                            );
                            let handle = Rect::from_center_size(
                                pos2(hx, wave_rect.top() + 6.0),
                                vec2(8.0, 10.0),
                            );
                            wp.rect_filled(handle, CornerRadius::same(2), sel_edge);
                        }
                    }
                }

                // Paint cursor / live playback head
                let playhead_frame = if is_playing {
                    self.preview.current_frame()
                } else {
                    self.cursor_frame
                };
                if playhead_frame >= view_start_frame && playhead_frame <= view_end_frame {
                    let cx = x_at_frame(playhead_frame);
                    let cursor_col = if is_playing {
                        Color32::from_rgb(239, 83, 80)
                    } else if dark_mode {
                        Color32::from_rgb(230, 230, 230)
                    } else {
                        Color32::from_rgb(40, 40, 40)
                    };
                    wp.line_segment(
                        [pos2(cx, wave_rect.top()), pos2(cx, wave_rect.bottom())],
                        Stroke::new(1.5_f32, cursor_col),
                    );
                }

                ui.add_space(2.0);

                // 3. Horizontal scrollbar
                let (sb_rect, sb_resp) = ui.allocate_exact_size(
                    vec2(ui.available_width(), scroll_h),
                    Sense::click_and_drag(),
                );
                let sbp = ui.painter_at(sb_rect);
                sbp.rect_filled(sb_rect, CornerRadius::same(3), bg_canvas);
                sbp.rect_stroke(
                    sb_rect,
                    CornerRadius::same(3),
                    Stroke::new(1.0_f32, border_col),
                    egui::StrokeKind::Inside,
                );
                let thumb_w = (sb_rect.width() * visible_frac).max(24.0).min(sb_rect.width());
                let max_thumb_x = (sb_rect.width() - thumb_w).max(0.0);
                let max_scroll = (1.0 - visible_frac).max(1e-5);
                let thumb_x = if self.zoom <= 1.001 {
                    sb_rect.left()
                } else {
                    sb_rect.left() + (self.scroll_offset / max_scroll).clamp(0.0, 1.0) * max_thumb_x
                };
                let thumb_rect = Rect::from_min_size(
                    pos2(thumb_x, sb_rect.top() + 2.0),
                    vec2(thumb_w, sb_rect.height() - 4.0),
                );
                let thumb_col = if self.zoom <= 1.001 {
                    border_col
                } else if sb_resp.hovered() || sb_resp.dragged() {
                    Color32::from_rgb(76, 166, 255)
                } else {
                    Color32::from_rgb(30, 136, 229)
                };
                sbp.rect_filled(thumb_rect, CornerRadius::same(3), thumb_col);

                if self.zoom > 1.001 && (sb_resp.dragged() || sb_resp.clicked()) {
                    if let Some(pos) = sb_resp.interact_pointer_pos() {
                        let target_center = (pos.x - sb_rect.left() - thumb_w * 0.5)
                            / max_thumb_x.max(1.0);
                        self.scroll_offset = target_center.clamp(0.0, 1.0) * max_scroll;
                        self.clamp_scroll();
                    }
                }

                if let Some(ref msg) = self.status_message {
                    ui.add_space(2.0);
                    ui.label(
                        RichText::new(msg)
                            .color(Color32::from_rgb(229, 115, 115))
                            .size(11.5),
                    );
                }

                ui.add_space(4.0);
                ui.separator();

                // Bottom bar: metadata on the left, Save as / Save / Cancel on the right
                let sel_info = self.normalized_selection().map(|(s, e)| {
                    let d = (e - s) as f32 / TARGET_SAMPLE_RATE as f32;
                    format!("  |  Selection: {}", format_duration_ms(d))
                }).unwrap_or_default();

                ui.horizontal(|ui| {
                    ui.vertical(|ui| {
                        ui.label(
                            RichText::new(format!(
                                "Size: {}  |  Format: {}  |  Sample rate: {} Hz",
                                format_file_size(self.file_size_bytes),
                                self.format_label,
                                TARGET_SAMPLE_RATE
                            ))
                            .size(11.5),
                        );
                        ui.label(
                            RichText::new(format!(
                                "Duration: {}  |  Channels: stereo  |  Volume: {:.0}% ({:+.1} dB){}",
                                format_duration_ms(self.duration_secs()),
                                self.gain_linear() * 100.0,
                                self.gain_db,
                                sel_info
                            ))
                            .size(11.5),
                        );
                    });

                    ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                        if ui.button("Cancel").clicked() {
                            action = Some(EditorAction::Close);
                        }

                        let dirty = self.has_unsaved_changes();
                        if ui
                            .add_enabled(dirty, egui::Button::new("Save"))
                            .clicked()
                        {
                            match self.save_in_place() {
                                Ok(act) => action = Some(act),
                                Err(e) => self.status_message = Some(format!("Save error: {e}")),
                            }
                        }

                        if ui.button("Save as...").clicked() {
                            match self.save_as_dialog() {
                                Ok(Some(act)) => action = Some(act),
                                Ok(None) => {}
                                Err(e) => {
                                    self.status_message = Some(format!("Save as error: {e}"))
                                }
                            }
                        }
                    });
                });
            });

        if !open && action.is_none() {
            action = Some(EditorAction::Close);
        }
        if matches!(action, Some(EditorAction::Close)) {
            self.preview.stop();
        }
        action
    }
}

pub fn write_wav_pcm16(path: &Path, samples: &[f32]) -> Result<(), String> {
    let tmp_path = temp_sibling_path(path, "wav");
    let res = (|| -> Result<(), String> {
        let file = File::create(&tmp_path)
            .map_err(|e| format!("Failed to create {}: {e}", tmp_path.display()))?;
        let mut writer = BufWriter::new(file);
        writer
            .write_all(&build_wav_header(0))
            .map_err(|e| format!("Failed to write WAV header: {e}"))?;

        let mut data_bytes: u64 = 0;
        let mut chunk_bytes = Vec::with_capacity(4096 * 2);
        for block in samples.chunks(4096) {
            chunk_bytes.clear();
            for &s in block {
                let limited = if s.is_finite() { soft_limit(s) } else { 0.0 };
                let pcm = (limited.clamp(-1.0, 1.0) * i16::MAX as f32).round() as i16;
                chunk_bytes.extend_from_slice(&pcm.to_le_bytes());
            }
            writer
                .write_all(&chunk_bytes)
                .map_err(|e| format!("Failed to write WAV data: {e}"))?;
            data_bytes = data_bytes.saturating_add(chunk_bytes.len() as u64);
        }

        let mut file = writer
            .into_inner()
            .map_err(|e| format!("Failed to flush WAV buffer: {}", e.error()))?;
        finalize_wav_header(&mut file, data_bytes)?;
        Ok(())
    })();

    if let Err(e) = res {
        let _ = fs::remove_file(&tmp_path);
        return Err(e);
    }

    fs::rename(&tmp_path, path).map_err(|e| {
        let _ = fs::remove_file(&tmp_path);
        format!("Failed to replace {}: {e}", path.display())
    })
}

fn save_with_ffmpeg(path: &Path, samples: &[f32]) -> Result<(), String> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .filter(|e| !e.is_empty())
        .unwrap_or("wav");
    let tmp_path = temp_sibling_path(path, ext);

    let mut child = Command::new("ffmpeg")
        .args([
            "-y",
            "-v",
            "error",
            "-f",
            "f32le",
            "-ar",
            "48000",
            "-ac",
            "2",
            "-i",
            "pipe:0",
        ])
        .arg(&tmp_path)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("Failed to launch ffmpeg: {e}"))?;

    if let Some(mut stdin) = child.stdin.take() {
        let mut raw = Vec::with_capacity(4096 * 4);
        for block in samples.chunks(4096) {
            raw.clear();
            for &s in block {
                let v = if s.is_finite() { s } else { 0.0 };
                raw.extend_from_slice(&v.to_le_bytes());
            }
            if let Err(e) = stdin.write_all(&raw) {
                let _ = child.kill();
                let _ = child.wait();
                let _ = fs::remove_file(&tmp_path);
                return Err(format!("Failed to pipe audio to ffmpeg: {e}"));
            }
        }
    }

    let output = child
        .wait_with_output()
        .map_err(|e| format!("Failed waiting for ffmpeg: {e}"))?;
    if !output.status.success() {
        let _ = fs::remove_file(&tmp_path);
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(if stderr.is_empty() {
            format!("ffmpeg exited with status {}", output.status)
        } else {
            stderr
        });
    }

    fs::rename(&tmp_path, path).map_err(|e| {
        let _ = fs::remove_file(&tmp_path);
        format!("Failed to replace {}: {e}", path.display())
    })
}

fn temp_sibling_path(target: &Path, ext: &str) -> PathBuf {
    let parent = target.parent().unwrap_or_else(|| Path::new("."));
    let stem = target
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("sound");
    parent.join(format!(".{stem}.soundpat_tmp.{ext}"))
}

fn build_peak_cache(samples: &[f32]) -> Vec<(f32, f32)> {
    let total_frames = samples.len() / TARGET_CHANNELS;
    let blocks = (total_frames + PEAK_BLOCK_FRAMES - 1) / PEAK_BLOCK_FRAMES;
    let mut peaks = Vec::with_capacity(blocks);
    for block in 0..blocks {
        let start = block * PEAK_BLOCK_FRAMES;
        let end = ((block + 1) * PEAK_BLOCK_FRAMES).min(total_frames);
        let mut min_v = 0.0f32;
        let mut max_v = 0.0f32;
        for &s in &samples[start * TARGET_CHANNELS..end * TARGET_CHANNELS] {
            if s < min_v {
                min_v = s;
            }
            if s > max_v {
                max_v = s;
            }
        }
        peaks.push((min_v, max_v));
    }
    peaks
}

fn peak_range(samples: &[f32], peaks_256: &[(f32, f32)], f0: usize, f1: usize) -> (f32, f32) {
    if f0 >= f1 || samples.is_empty() {
        return (0.0, 0.0);
    }
    let span = f1 - f0;
    if span <= PEAK_BLOCK_FRAMES * 2 || peaks_256.is_empty() {
        let s0 = (f0 * TARGET_CHANNELS).min(samples.len());
        let s1 = (f1 * TARGET_CHANNELS).min(samples.len());
        let mut min_v = 0.0f32;
        let mut max_v = 0.0f32;
        for &s in &samples[s0..s1] {
            if s < min_v {
                min_v = s;
            }
            if s > max_v {
                max_v = s;
            }
        }
        return (min_v, max_v);
    }

    let b0 = (f0 / PEAK_BLOCK_FRAMES).min(peaks_256.len().saturating_sub(1));
    let b1 = ((f1 + PEAK_BLOCK_FRAMES - 1) / PEAK_BLOCK_FRAMES).min(peaks_256.len());
    let mut min_v = 0.0f32;
    let mut max_v = 0.0f32;
    for &(b_min, b_max) in &peaks_256[b0..b1] {
        if b_min < min_v {
            min_v = b_min;
        }
        if b_max > max_v {
            max_v = b_max;
        }
    }
    (min_v, max_v)
}

fn choose_ruler_step(view_span_secs: f32) -> f32 {
    const STEPS: &[f32] = &[0.05, 0.1, 0.25, 0.5, 1.0, 2.0, 5.0, 10.0, 15.0, 30.0, 60.0];
    for &step in STEPS {
        if view_span_secs / step <= 10.0 {
            return step;
        }
    }
    60.0
}

pub fn format_ruler_time(secs: f32) -> String {
    let tenths_total = (secs.max(0.0) * 10.0).round() as u64;
    let mins = tenths_total / 600;
    let rem_tenths = tenths_total % 600;
    let whole_secs = rem_tenths / 10;
    let tenths = rem_tenths % 10;
    format!("{mins}:{whole_secs:02}.{tenths}")
}

pub fn format_duration_ms(secs: f32) -> String {
    if !secs.is_finite() || secs < 0.0 {
        return "0:00.000".to_string();
    }
    let total_ms = (secs * 1000.0).round() as u64;
    let mins = total_ms / 60_000;
    let rem_secs = (total_ms % 60_000) / 1000;
    let ms = total_ms % 1000;
    format!("{mins}:{rem_secs:02}.{ms:03}")
}

pub fn format_file_size(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{:.2} MB", bytes as f64 / (1024.0 * 1024.0))
    }
}

fn format_from_path(path: &Path) -> String {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_uppercase())
        .filter(|e| !e.is_empty())
        .unwrap_or_else(|| "AUDIO".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_stereo_frames(n_frames: usize) -> Vec<f32> {
        let mut out = Vec::with_capacity(n_frames * 2);
        for i in 0..n_frames {
            let v = (i as f32 / n_frames.max(1) as f32) * 0.5;
            out.push(v);
            out.push(-v);
        }
        out
    }

    #[test]
    fn trim_cut_and_undo_redo_roundtrip() {
        let samples = make_stereo_frames(4800); // 100 ms
        let mut ed = SoundEditor::from_samples_for_test(samples.clone(), PathBuf::from("a.wav"));
        assert!(!ed.has_unsaved_changes());

        // Select middle [1200, 3600) and trim -> 2400 frames
        ed.selection = Some((1200, 3600));
        assert!(ed.trim_selection());
        assert_eq!(ed.total_frames(), 2400);
        assert!(ed.has_unsaved_changes());

        // Cut [400, 1000) -> 1800 frames
        ed.selection = Some((400, 1000));
        assert!(ed.cut_selection());
        assert_eq!(ed.total_frames(), 1800);

        // Undo Cut -> 2400 frames
        assert!(ed.undo());
        assert_eq!(ed.total_frames(), 2400);

        // Undo Trim -> 4800 frames, clean again
        assert!(ed.undo());
        assert_eq!(ed.total_frames(), 4800);
        assert_eq!(ed.samples, samples);
        assert!(!ed.has_unsaved_changes());

        // Redo Trim -> 2400 frames
        assert!(ed.redo());
        assert_eq!(ed.total_frames(), 2400);
        assert!(ed.has_unsaved_changes());
    }

    #[test]
    fn write_wav_and_decode_back() {
        let tmp = std::env::temp_dir().join("soundpat_editor_write_test.wav");
        let mut samples = Vec::with_capacity(4800 * 2);
        for i in 0..4800 {
            let t = i as f32 / TARGET_SAMPLE_RATE as f32;
            let v = (2.0 * std::f32::consts::PI * 440.0 * t).sin() * 0.5;
            samples.push(v);
            samples.push(v);
        }

        write_wav_pcm16(&tmp, &samples).unwrap();
        let decoded = decode_audio_file(&tmp).unwrap();
        let _ = fs::remove_file(&tmp);

        assert!((decoded.duration_secs - 0.1).abs() < 0.01);
        assert_eq!(decoded.samples.len() % 2, 0);
    }

    #[test]
    fn formatting_helpers() {
        assert_eq!(format_ruler_time(0.5), "0:00.5");
        assert_eq!(format_ruler_time(1.0), "0:01.0");
        assert_eq!(format_duration_ms(1.234), "0:01.234");
        assert_eq!(format_file_size(512), "512 B");
        assert_eq!(format_file_size(2048), "2.0 KB");
    }
}
