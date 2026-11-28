use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{SampleFormat, Stream, StreamConfig};
use libpulse_binding::sample::{Format as PaFormat, Spec as PaSpec};
use libpulse_binding::stream::Direction as PaDirection;
use libpulse_simple_binding::Simple as PaSimple;
use ringbuf::traits::{Consumer, Observer, Producer, Split};
use ringbuf::HeapRb;
use std::collections::{HashMap, VecDeque};
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex, RwLock};
use std::thread;
use std::time::Duration;
use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::DecoderOptions;
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

unsafe extern "C" {
    fn malloc_trim(pad: usize) -> i32;
}

#[inline]
fn release_freed_ram_to_os() {
    unsafe {
        let _ = malloc_trim(0);
    }
}

pub const TARGET_SAMPLE_RATE: u32 = 48_000;
pub const TARGET_CHANNELS: usize = 2;
const MIX_CHUNK_FRAMES: usize = 480; // 10 ms at 48 kHz
const STREAM_START_FRAMES: usize = TARGET_SAMPLE_RATE as usize / 10; // 100 ms prebuffer before instant start
const STREAM_BATCH_FRAMES: usize = TARGET_SAMPLE_RATE as usize / 4; // 250 ms chunks pushed while streaming

// Memory bounds for RAM cache: only cache clips <= 30s, and cap total cache at 48 MB
const MAX_CACHEABLE_SAMPLES: usize = TARGET_SAMPLE_RATE as usize * TARGET_CHANNELS * 30;
const MAX_CACHE_TOTAL_SAMPLES: usize = (48 * 1024 * 1024) / std::mem::size_of::<f32>();

#[derive(Debug, Clone)]
pub struct DecodedAudio {
    pub samples: Vec<f32>, // Interleaved stereo @ 48kHz
    pub duration_secs: f32,
    pub norm_gain: f32,
}

pub struct StreamingBuffer {
    pub samples: RwLock<Vec<f32>>,
    pub duration_bits: AtomicU32,
    pub norm_gain_bits: AtomicU32,
    pub is_complete: AtomicBool,
    pub cancel: AtomicBool,
}

impl StreamingBuffer {
    pub fn new_streaming(initial_duration: f32) -> Arc<Self> {
        Arc::new(Self {
            samples: RwLock::new(Vec::with_capacity(
                TARGET_SAMPLE_RATE as usize * TARGET_CHANNELS * 2,
            )),
            duration_bits: AtomicU32::new(initial_duration.to_bits()),
            norm_gain_bits: AtomicU32::new(1.0f32.to_bits()),
            is_complete: AtomicBool::new(false),
            cancel: AtomicBool::new(false),
        })
    }

    #[inline]
    pub fn duration_secs(&self) -> f32 {
        f32::from_bits(self.duration_bits.load(Ordering::Relaxed))
    }

    #[inline]
    pub fn norm_gain(&self) -> f32 {
        f32::from_bits(self.norm_gain_bits.load(Ordering::Relaxed))
    }
}

struct BoundedAudioCache {
    map: HashMap<PathBuf, Arc<StreamingBuffer>>,
    order: VecDeque<PathBuf>,
    total_samples: usize,
}

impl BoundedAudioCache {
    fn new() -> Self {
        Self {
            map: HashMap::new(),
            order: VecDeque::new(),
            total_samples: 0,
        }
    }

    fn get(&self, path: &Path) -> Option<Arc<StreamingBuffer>> {
        self.map.get(path).cloned()
    }

    fn insert(&mut self, path: PathBuf, buf: Arc<StreamingBuffer>, sample_len: usize) {
        if sample_len == 0 || sample_len > MAX_CACHEABLE_SAMPLES {
            return;
        }
        self.remove(&path);
        while self.total_samples + sample_len > MAX_CACHE_TOTAL_SAMPLES {
            let Some(oldest) = self.order.pop_front() else {
                break;
            };
            if let Some(old_buf) = self.map.remove(&oldest) {
                let len = old_buf.samples.read().map(|s| s.len()).unwrap_or(0);
                self.total_samples = self.total_samples.saturating_sub(len);
            }
        }
        self.total_samples += sample_len;
        self.order.push_back(path.clone());
        self.map.insert(path, buf);
    }

    fn remove(&mut self, path: &Path) {
        if let Some(old_buf) = self.map.remove(path) {
            let len = old_buf.samples.read().map(|s| s.len()).unwrap_or(0);
            self.total_samples = self.total_samples.saturating_sub(len);
            self.order.retain(|p| p != path);
        }
    }

    #[allow(dead_code)]
    fn clear(&mut self) {
        self.map.clear();
        self.order.clear();
        self.total_samples = 0;
    }
}

#[derive(Debug, Clone, Default)]
pub struct PlaybackStatus {
    pub current_sound_id: Option<u64>,
    pub position_secs: f32,
    pub duration_secs: f32,
    pub is_playing: bool,
    pub is_paused: bool,
    pub mic_peak: f32,
    pub last_error: Option<String>,
}

struct ActiveVoice {
    sound_id: u64,
    buffer: Arc<StreamingBuffer>,
    frame_pos: usize,
    is_primary: bool,
}

struct MixerInner {
    voices: Vec<ActiveVoice>,
    is_paused: bool,
    speakers_enabled: bool,
    mic_enabled: bool,
    master_volume: f32,
    speakers_volume: f32,
    mic_volume: f32,
    auto_normalize: bool,
    repeat_enabled: bool,
    auto_advance: bool,
    pending_auto_advance: Option<u64>,
    mic_peak: f32,
    last_error: Option<String>,
}

enum DecodeCmd {
    Play {
        sound_id: u64,
        path: PathBuf,
        allow_overlap: bool,
        seq_id: u64,
    },
}

#[derive(Clone)]
pub struct AudioEngineHandle {
    mixer: Arc<Mutex<MixerInner>>,
    decode_tx: mpsc::Sender<DecodeCmd>,
    cache: Arc<Mutex<BoundedAudioCache>>,
    play_gen: Arc<AtomicU64>,
}

impl AudioEngineHandle {
    pub fn play(&self, sound_id: u64, path: PathBuf, allow_overlap: bool) {
        let seq_id = if !allow_overlap {
            let g = self.play_gen.fetch_add(1, Ordering::SeqCst) + 1;
            let mut dropped_voices = Vec::new();
            if let Ok(mut guard) = self.mixer.lock() {
                for v in &guard.voices {
                    v.buffer.cancel.store(true, Ordering::Relaxed);
                }
                std::mem::swap(&mut dropped_voices, &mut guard.voices);
            }
            if !dropped_voices.is_empty() {
                drop(dropped_voices);
                release_freed_ram_to_os();
            }
            g
        } else {
            self.play_gen.load(Ordering::SeqCst)
        };

        // Zero-copy fast path if already in RAM cache
        if let Some(cached_buf) = self.cache.lock().ok().and_then(|c| c.get(&path)) {
            if let Ok(mut guard) = self.mixer.lock() {
                guard.last_error = None;
                if !allow_overlap {
                    for v in &guard.voices {
                        v.buffer.cancel.store(true, Ordering::Relaxed);
                    }
                    guard.voices.clear();
                } else {
                    for v in &mut guard.voices {
                        v.is_primary = false;
                    }
                }
                guard.is_paused = false;
                guard.voices.push(ActiveVoice {
                    sound_id,
                    buffer: cached_buf,
                    frame_pos: 0,
                    is_primary: true,
                });
            }
            return;
        }

        let _ = self.decode_tx.send(DecodeCmd::Play {
            sound_id,
            path,
            allow_overlap,
            seq_id,
        });
    }

    pub fn toggle_pause(&self) {
        if let Ok(mut g) = self.mixer.lock() {
            if !g.voices.is_empty() {
                g.is_paused = !g.is_paused;
            }
        }
    }

    pub fn set_paused(&self, paused: bool) {
        if let Ok(mut g) = self.mixer.lock() {
            if !g.voices.is_empty() {
                g.is_paused = paused;
            }
        }
    }

    pub fn stop_all(&self) {
        self.play_gen.fetch_add(1, Ordering::SeqCst);
        let mut dropped_voices = Vec::new();
        if let Ok(mut g) = self.mixer.lock() {
            for v in &g.voices {
                v.buffer.cancel.store(true, Ordering::Relaxed);
            }
            std::mem::swap(&mut dropped_voices, &mut g.voices);
            g.is_paused = false;
            g.mic_peak = 0.0;
        }
        drop(dropped_voices);
        release_freed_ram_to_os();
    }

    pub fn status(&self) -> PlaybackStatus {
        let Ok(g) = self.mixer.lock() else {
            return PlaybackStatus::default();
        };
        let primary = g
            .voices
            .iter()
            .rev()
            .find(|v| v.is_primary)
            .or_else(|| g.voices.last());

        if let Some(v) = primary {
            PlaybackStatus {
                current_sound_id: Some(v.sound_id),
                position_secs: v.frame_pos as f32 / TARGET_SAMPLE_RATE as f32,
                duration_secs: v.buffer.duration_secs(),
                is_playing: !g.is_paused,
                is_paused: g.is_paused,
                mic_peak: g.mic_peak,
                last_error: g.last_error.clone(),
            }
        } else {
            PlaybackStatus {
                mic_peak: g.mic_peak,
                last_error: g.last_error.clone(),
                ..Default::default()
            }
        }
    }
}

pub struct AudioEngine {
    mixer: Arc<Mutex<MixerInner>>,
    decode_tx: mpsc::Sender<DecodeCmd>,
    cache: Arc<Mutex<BoundedAudioCache>>,
    play_gen: Arc<AtomicU64>,
    cpal_stream: Option<Stream>,
    speaker_rb_prod: Arc<Mutex<ringbuf::HeapProd<f32>>>,
    shutdown: Arc<AtomicBool>,
}

impl AudioEngine {
    pub fn new(initial_output_device: Option<&str>) -> Self {
        let rb = HeapRb::<f32>::new(TARGET_SAMPLE_RATE as usize * TARGET_CHANNELS / 4);
        let (prod, cons) = rb.split();
        let speaker_rb_prod = Arc::new(Mutex::new(prod));

        let mixer = Arc::new(Mutex::new(MixerInner {
            voices: Vec::new(),
            is_paused: false,
            speakers_enabled: true,
            mic_enabled: true,
            master_volume: 1.0,
            speakers_volume: 0.85,
            mic_volume: 0.85,
            auto_normalize: true,
            repeat_enabled: false,
            auto_advance: false,
            pending_auto_advance: None,
            mic_peak: 0.0,
            last_error: None,
        }));

        let cache = Arc::new(Mutex::new(BoundedAudioCache::new()));
        let play_gen = Arc::new(AtomicU64::new(0));
        let shutdown = Arc::new(AtomicBool::new(false));
        let (decode_tx, decode_rx) = mpsc::channel::<DecodeCmd>();

        // Single reusable decode worker thread for non-overlap mode (avoids glibc per-thread arena bloat),
        // spawns extra thread only when allow_overlap is true.
        {
            let mixer = Arc::clone(&mixer);
            let cache = Arc::clone(&cache);
            let play_gen = Arc::clone(&play_gen);
            thread::spawn(move || {
                while let Ok(mut cmd) = decode_rx.recv() {
                    // Drain queue to latest command if user clicked rapidly in non-overlap mode
                    while let Ok(newer) = decode_rx.try_recv() {
                        let DecodeCmd::Play {
                            sound_id: cur_sid,
                            path: ref cur_path,
                            allow_overlap: cur_ov,
                            seq_id: cur_seq,
                        } = cmd;
                        let DecodeCmd::Play {
                            allow_overlap: new_ov,
                            ..
                        } = newer;
                        if !new_ov {
                            cmd = newer;
                        } else {
                            let m = Arc::clone(&mixer);
                            let c = Arc::clone(&cache);
                            let g = Arc::clone(&play_gen);
                            let p = cur_path.clone();
                            thread::spawn(move || {
                                stream_decode_and_play(cur_sid, p, cur_ov, cur_seq, m, c, g);
                                release_freed_ram_to_os();
                            });
                            cmd = newer;
                        }
                    }

                    match cmd {
                        DecodeCmd::Play {
                            sound_id,
                            path,
                            allow_overlap,
                            seq_id,
                        } => {
                            if !allow_overlap && play_gen.load(Ordering::SeqCst) != seq_id {
                                continue;
                            }

                            if let Some(cached_buf) = cache.lock().ok().and_then(|c| c.get(&path)) {
                                if let Ok(mut guard) = mixer.lock() {
                                    if !allow_overlap && play_gen.load(Ordering::SeqCst) != seq_id {
                                        continue;
                                    }
                                    guard.last_error = None;
                                    if !allow_overlap {
                                        for v in &guard.voices {
                                            v.buffer.cancel.store(true, Ordering::Relaxed);
                                        }
                                        guard.voices.clear();
                                    } else {
                                        for v in &mut guard.voices {
                                            v.is_primary = false;
                                        }
                                    }
                                    guard.is_paused = false;
                                    guard.voices.push(ActiveVoice {
                                        sound_id,
                                        buffer: cached_buf,
                                        frame_pos: 0,
                                        is_primary: true,
                                    });
                                }
                                continue;
                            }

                            if allow_overlap {
                                let m = Arc::clone(&mixer);
                                let c = Arc::clone(&cache);
                                let g = Arc::clone(&play_gen);
                                thread::spawn(move || {
                                    stream_decode_and_play(sound_id, path, true, seq_id, m, c, g);
                                    release_freed_ram_to_os();
                                });
                            } else {
                                stream_decode_and_play(
                                    sound_id,
                                    path,
                                    false,
                                    seq_id,
                                    Arc::clone(&mixer),
                                    Arc::clone(&cache),
                                    Arc::clone(&play_gen),
                                );
                                release_freed_ram_to_os();
                            }
                        }
                    }
                }
            });
        }

        // Mixer + PipeWire virtual sink writer thread
        {
            let mixer = Arc::clone(&mixer);
            let prod = Arc::clone(&speaker_rb_prod);
            let shutdown = Arc::clone(&shutdown);
            thread::spawn(move || {
                mixer_loop(mixer, prod, shutdown);
            });
        }

        let mut engine = Self {
            mixer,
            decode_tx,
            cache,
            play_gen,
            cpal_stream: None,
            speaker_rb_prod,
            shutdown,
        };

        if let Err(e) = engine.start_cpal_stream(initial_output_device, cons) {
            if let Ok(mut g) = engine.mixer.lock() {
                g.last_error = Some(e);
            }
        }

        engine
    }

    pub fn clone_handle(&self) -> AudioEngineHandle {
        AudioEngineHandle {
            mixer: Arc::clone(&self.mixer),
            decode_tx: self.decode_tx.clone(),
            cache: Arc::clone(&self.cache),
            play_gen: Arc::clone(&self.play_gen),
        }
    }

    fn start_cpal_stream(
        &mut self,
        device_name: Option<&str>,
        mut cons: ringbuf::HeapCons<f32>,
    ) -> Result<(), String> {
        let host = cpal::default_host();
        let device = if let Some(name) = device_name.filter(|s| !s.is_empty()) {
            host.output_devices()
                .ok()
                .and_then(|mut devs| devs.find(|d| d.name().ok().as_deref() == Some(name)))
                .or_else(|| host.default_output_device())
        } else {
            host.default_output_device()
        }
        .ok_or_else(|| "No cpal audio output device found".to_string())?;

        let supported = device
            .default_output_config()
            .map_err(|e| format!("Failed to get default output config: {e}"))?;

        let out_channels = supported.channels().max(1) as usize;
        let out_rate = supported.sample_rate().0;
        let sample_format = supported.sample_format();
        let config = StreamConfig {
            channels: supported.channels(),
            sample_rate: supported.sample_rate(),
            buffer_size: cpal::BufferSize::Default,
        };

        let ratio = TARGET_SAMPLE_RATE as f64 / out_rate.max(1) as f64;
        let mut frac_pos = 0.0f64;
        let mut l0 = 0.0f32;
        let mut r0 = 0.0f32;
        let mut l1 = 0.0f32;
        let mut r1 = 0.0f32;
        let mut initialized = false;

        let err_fn = |err| {
            eprintln!("cpal output stream error: {err}");
        };

        let mut next_frame = move || -> (f32, f32) {
            if (ratio - 1.0).abs() < 1e-4 {
                let l = cons.try_pop().unwrap_or(0.0);
                let r = cons.try_pop().unwrap_or(l);
                return (l, r);
            }

            if !initialized {
                l0 = cons.try_pop().unwrap_or(0.0);
                r0 = cons.try_pop().unwrap_or(l0);
                l1 = cons.try_pop().unwrap_or(l0);
                r1 = cons.try_pop().unwrap_or(r0);
                initialized = true;
            }

            let frac = frac_pos as f32;
            let l = l0 + (l1 - l0) * frac;
            let r = r0 + (r1 - r0) * frac;

            frac_pos += ratio;
            while frac_pos >= 1.0 {
                frac_pos -= 1.0;
                l0 = l1;
                r0 = r1;
                l1 = cons.try_pop().unwrap_or(0.0);
                r1 = cons.try_pop().unwrap_or(l1);
            }

            (l, r)
        };

        let stream = match sample_format {
            SampleFormat::F32 => device.build_output_stream(
                &config,
                move |data: &mut [f32], _| {
                    for frame in data.chunks_exact_mut(out_channels) {
                        let (l, r) = next_frame();
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
                        let (l, r) = next_frame();
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
                        let (l, r) = next_frame();
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
                return Err(format!("Unsupported cpal sample format: {other:?}"));
            }
        }
        .map_err(|e| format!("Failed to build cpal output stream: {e}"))?;

        stream
            .play()
            .map_err(|e| format!("Failed to start cpal output stream: {e}"))?;
        self.cpal_stream = Some(stream);
        Ok(())
    }

    pub fn set_output_device(&mut self, device_name: Option<&str>) -> Result<(), String> {
        self.cpal_stream = None;
        let rb = HeapRb::<f32>::new(TARGET_SAMPLE_RATE as usize * TARGET_CHANNELS / 4);
        let (prod, cons) = rb.split();
        if let Ok(mut guard) = self.speaker_rb_prod.lock() {
            *guard = prod;
        }
        self.start_cpal_stream(device_name, cons)
    }

    pub fn list_cpal_output_devices() -> Vec<String> {
        let host = cpal::default_host();
        let Ok(devices) = host.output_devices() else {
            return Vec::new();
        };
        devices.filter_map(|d| d.name().ok()).collect()
    }

    pub fn play(&self, sound_id: u64, path: PathBuf, allow_overlap: bool) {
        self.clone_handle().play(sound_id, path, allow_overlap);
    }

    pub fn toggle_pause(&self) {
        self.clone_handle().toggle_pause();
    }

    pub fn set_paused(&self, paused: bool) {
        self.clone_handle().set_paused(paused);
    }

    pub fn stop_all(&self) {
        self.clone_handle().stop_all();
    }

    pub fn seek(&self, target_secs: f32) {
        if let Ok(mut g) = self.mixer.lock() {
            if let Some(v) = g.voices.iter_mut().rev().find(|v| v.is_primary) {
                let available_frames = v
                    .buffer
                    .samples
                    .read()
                    .map(|s| s.len() / TARGET_CHANNELS)
                    .unwrap_or(0);
                let target_frame = ((target_secs.max(0.0) * TARGET_SAMPLE_RATE as f32) as usize)
                    .min(available_frames);
                v.frame_pos = target_frame;
            }
        }
    }

    pub fn sync_settings(
        &self,
        speakers_enabled: bool,
        mic_enabled: bool,
        master_volume: f32,
        speakers_volume: f32,
        mic_volume: f32,
        auto_normalize: bool,
        repeat_enabled: bool,
        auto_advance: bool,
    ) {
        if let Ok(mut g) = self.mixer.lock() {
            g.speakers_enabled = speakers_enabled;
            g.mic_enabled = mic_enabled;
            g.master_volume = master_volume;
            g.speakers_volume = speakers_volume;
            g.mic_volume = mic_volume;
            g.auto_normalize = auto_normalize;
            g.repeat_enabled = repeat_enabled;
            g.auto_advance = auto_advance;
        }
    }

    pub fn status(&self) -> PlaybackStatus {
        self.clone_handle().status()
    }

    pub fn take_auto_advance(&self) -> Option<u64> {
        self.mixer
            .lock()
            .ok()
            .and_then(|mut g| g.pending_auto_advance.take())
    }

    pub fn clear_cache_for(&self, path: &Path) {
        if let Ok(mut c) = self.cache.lock() {
            c.remove(path);
        }
        release_freed_ram_to_os();
    }
}

impl Drop for AudioEngine {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
    }
}

struct StreamingResampler {
    src_rate: u32,
    dst_rate: u32,
    pos: f64,
    prev: Option<(f32, f32)>,
}

impl StreamingResampler {
    fn new(src_rate: u32, dst_rate: u32) -> Self {
        Self {
            src_rate: src_rate.max(1),
            dst_rate: dst_rate.max(1),
            pos: 0.0,
            prev: None,
        }
    }

    fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
        if input.is_empty() {
            return;
        }
        if self.src_rate == self.dst_rate {
            out.extend_from_slice(input);
            return;
        }
        let in_frames = input.len() / 2;
        if in_frames == 0 {
            return;
        }
        let ratio = self.src_rate as f64 / self.dst_rate as f64;

        let (first_l, first_r) = (input[0], input[1]);
        let (prev_l, prev_r) = self.prev.unwrap_or((first_l, first_r));

        let get_frame = |idx: isize| -> (f32, f32) {
            if idx < 0 {
                (prev_l, prev_r)
            } else {
                let i = (idx as usize).min(in_frames - 1);
                (input[i * 2], input[i * 2 + 1])
            }
        };

        while self.pos < (in_frames - 1) as f64 {
            let idx = self.pos.floor() as isize;
            let frac = (self.pos - idx as f64) as f32;
            let (l0, r0) = get_frame(idx);
            let (l1, r1) = get_frame(idx + 1);
            out.push(l0 + (l1 - l0) * frac);
            out.push(r0 + (r1 - r0) * frac);
            self.pos += ratio;
        }

        self.prev = Some((input[(in_frames - 1) * 2], input[(in_frames - 1) * 2 + 1]));
        self.pos -= in_frames as f64;
    }
}

fn activate_streaming_voice(
    sound_id: u64,
    allow_overlap: bool,
    seq_id: u64,
    buffer: &Arc<StreamingBuffer>,
    mixer: &Arc<Mutex<MixerInner>>,
    play_gen: &Arc<AtomicU64>,
) -> bool {
    let mut dropped_voices = Vec::new();
    let ok = {
        let Ok(mut guard) = mixer.lock() else {
            return false;
        };
        if !allow_overlap && play_gen.load(Ordering::SeqCst) != seq_id {
            buffer.cancel.store(true, Ordering::Relaxed);
            return false;
        }
        guard.last_error = None;
        if !allow_overlap {
            for v in &guard.voices {
                v.buffer.cancel.store(true, Ordering::Relaxed);
            }
            std::mem::swap(&mut dropped_voices, &mut guard.voices);
        } else {
            for v in &mut guard.voices {
                v.is_primary = false;
            }
        }
        guard.is_paused = false;
        guard.voices.push(ActiveVoice {
            sound_id,
            buffer: Arc::clone(buffer),
            frame_pos: 0,
            is_primary: true,
        });
        true
    };
    drop(dropped_voices);
    ok
}

fn stream_decode_and_play(
    sound_id: u64,
    path: PathBuf,
    allow_overlap: bool,
    seq_id: u64,
    mixer: Arc<Mutex<MixerInner>>,
    cache: Arc<Mutex<BoundedAudioCache>>,
    play_gen: Arc<AtomicU64>,
) {
    let sym_res = stream_with_symphonia(
        sound_id,
        &path,
        allow_overlap,
        seq_id,
        &mixer,
        &cache,
        &play_gen,
    );
    if let Err(sym_err) = sym_res {
        if !allow_overlap && play_gen.load(Ordering::SeqCst) != seq_id {
            return;
        }
        if let Err(ff_err) = stream_with_ffmpeg(
            sound_id,
            &path,
            allow_overlap,
            seq_id,
            &mixer,
            &cache,
            &play_gen,
        ) {
            if let Ok(mut g) = mixer.lock() {
                g.last_error = Some(format!("{sym_err}; ffmpeg fallback failed: {ff_err}"));
            }
        }
    }
}

fn push_batch_to_stream(
    batch: &mut Vec<f32>,
    buffer: &Arc<StreamingBuffer>,
    sum_sq: &mut f64,
    total_samples: &mut usize,
) {
    if batch.is_empty() {
        return;
    }
    for &s in batch.iter() {
        *sum_sq += (s as f64) * (s as f64);
    }
    *total_samples += batch.len();
    if *total_samples > 0 {
        let rms = (*sum_sq / *total_samples as f64).sqrt() as f32;
        let gain = if rms > 1e-4 {
            (0.18 / rms).clamp(0.15, 2.5)
        } else {
            1.0
        };
        buffer
            .norm_gain_bits
            .store(gain.to_bits(), Ordering::Relaxed);
    }

    if let Ok(mut guard) = buffer.samples.write() {
        guard.append(batch);
        let cur_dur = (guard.len() / TARGET_CHANNELS) as f32 / TARGET_SAMPLE_RATE as f32;
        if cur_dur > buffer.duration_secs() {
            buffer
                .duration_bits
                .store(cur_dur.to_bits(), Ordering::Relaxed);
        }
    }
}

fn finalize_stream_into_cache(
    path: &Path,
    buffer: &Arc<StreamingBuffer>,
    cache: &Arc<Mutex<BoundedAudioCache>>,
    sum_sq: f64,
    total_samples: usize,
) {
    if buffer.cancel.load(Ordering::Relaxed) || total_samples == 0 {
        return;
    }

    let sample_len = if let Ok(mut g) = buffer.samples.write() {
        g.shrink_to_fit();
        g.len()
    } else {
        return;
    };
    if sample_len == 0 {
        return;
    }

    let duration_secs = (sample_len / TARGET_CHANNELS) as f32 / TARGET_SAMPLE_RATE as f32;
    let rms = (sum_sq / total_samples as f64).sqrt() as f32;
    let norm_gain = if rms > 1e-4 {
        (0.18 / rms).clamp(0.15, 2.5)
    } else {
        1.0
    };

    buffer
        .duration_bits
        .store(duration_secs.to_bits(), Ordering::Relaxed);
    buffer
        .norm_gain_bits
        .store(norm_gain.to_bits(), Ordering::Relaxed);
    buffer.is_complete.store(true, Ordering::Release);

    // Zero-copy cache insertion: only cache short/medium clips (<= 30s) within 48 MB total budget
    if sample_len <= MAX_CACHEABLE_SAMPLES {
        if let Ok(mut c) = cache.lock() {
            c.insert(path.to_path_buf(), Arc::clone(buffer), sample_len);
        }
    }
}

fn stream_with_symphonia(
    sound_id: u64,
    path: &Path,
    allow_overlap: bool,
    seq_id: u64,
    mixer: &Arc<Mutex<MixerInner>>,
    cache: &Arc<Mutex<BoundedAudioCache>>,
    play_gen: &Arc<AtomicU64>,
) -> Result<(), String> {
    let file = File::open(path).map_err(|e| format!("Cannot open {}: {e}", path.display()))?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());

    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }

    let probed = symphonia::default::get_probe()
        .format(
            &hint,
            mss,
            &FormatOptions::default(),
            &MetadataOptions::default(),
        )
        .map_err(|e| format!("Unsupported or invalid audio {}: {e}", path.display()))?;

    let mut format = probed.format;
    let track = format
        .tracks()
        .iter()
        .find(|t| t.codec_params.codec != symphonia::core::codecs::CODEC_TYPE_NULL)
        .ok_or_else(|| format!("No audio track found in {}", path.display()))?
        .clone();

    let track_id = track.id;
    let src_rate = track.codec_params.sample_rate.unwrap_or(TARGET_SAMPLE_RATE);
    let est_dur = track
        .codec_params
        .n_frames
        .map(|n| n as f32 / src_rate.max(1) as f32)
        .unwrap_or(0.0);

    let mut decoder = symphonia::default::get_codecs()
        .make(&track.codec_params, &DecoderOptions::default())
        .map_err(|e| format!("Decoder init failed for {}: {e}", path.display()))?;

    let stream_buf = StreamingBuffer::new_streaming(est_dur);
    let mut resampler = StreamingResampler::new(src_rate, TARGET_SAMPLE_RATE);
    let mut sample_buf: Option<SampleBuffer<f32>> = None;
    let mut raw_stereo = Vec::with_capacity(4096);
    let mut resampled_batch = Vec::with_capacity(STREAM_BATCH_FRAMES * TARGET_CHANNELS);

    let mut voice_started = false;
    let mut sum_sq = 0.0f64;
    let mut total_samples = 0usize;

    loop {
        if stream_buf.cancel.load(Ordering::Relaxed)
            || (!allow_overlap && play_gen.load(Ordering::SeqCst) != seq_id)
        {
            stream_buf.cancel.store(true, Ordering::Relaxed);
            return Ok(());
        }

        let packet = match format.next_packet() {
            Ok(p) => p,
            Err(SymphoniaError::IoError(e))
                if e.kind() == std::io::ErrorKind::UnexpectedEof =>
            {
                break;
            }
            Err(SymphoniaError::ResetRequired) => {
                decoder.reset();
                continue;
            }
            Err(e) => {
                if voice_started {
                    break;
                }
                return Err(format!("Read error in {}: {e}", path.display()));
            }
        };

        if packet.track_id() != track_id {
            continue;
        }

        match decoder.decode(&packet) {
            Ok(decoded) => {
                let spec = *decoded.spec();
                if spec.rate > 0 && spec.rate != resampler.src_rate {
                    resampler.src_rate = spec.rate;
                }
                let channels = spec.channels.count().max(1);
                let frames = decoded.frames();
                if frames == 0 {
                    continue;
                }

                let buf = sample_buf.get_or_insert_with(|| {
                    SampleBuffer::<f32>::new(decoded.capacity() as u64, spec)
                });
                if buf.capacity() < frames * channels {
                    *buf = SampleBuffer::<f32>::new(decoded.capacity() as u64, spec);
                }
                buf.copy_interleaved_ref(decoded);
                let raw = buf.samples();

                raw_stereo.clear();
                raw_stereo.reserve(frames * 2);
                for f in 0..frames {
                    let base = f * channels;
                    let (l, r) = if channels == 1 {
                        (raw[base], raw[base])
                    } else {
                        (raw[base], raw[base + 1])
                    };
                    raw_stereo.push(l);
                    raw_stereo.push(r);
                }

                resampler.process(&raw_stereo, &mut resampled_batch);

                let threshold_samples = if !voice_started {
                    STREAM_START_FRAMES * TARGET_CHANNELS
                } else {
                    STREAM_BATCH_FRAMES * TARGET_CHANNELS
                };

                if resampled_batch.len() >= threshold_samples {
                    push_batch_to_stream(
                        &mut resampled_batch,
                        &stream_buf,
                        &mut sum_sq,
                        &mut total_samples,
                    );
                    if !voice_started {
                        if !activate_streaming_voice(
                            sound_id,
                            allow_overlap,
                            seq_id,
                            &stream_buf,
                            mixer,
                            play_gen,
                        ) {
                            return Ok(());
                        }
                        voice_started = true;
                    }
                }
            }
            Err(SymphoniaError::IoError(_)) | Err(SymphoniaError::DecodeError(_)) => continue,
            Err(e) => {
                if voice_started {
                    break;
                }
                return Err(format!("Decode error in {}: {e}", path.display()));
            }
        }
    }

    if !resampled_batch.is_empty() {
        push_batch_to_stream(
            &mut resampled_batch,
            &stream_buf,
            &mut sum_sq,
            &mut total_samples,
        );
    }

    if total_samples == 0 {
        return Err(format!("No audio samples decoded from {}", path.display()));
    }

    if !voice_started {
        if !activate_streaming_voice(
            sound_id,
            allow_overlap,
            seq_id,
            &stream_buf,
            mixer,
            play_gen,
        ) {
            return Ok(());
        }
    }

    finalize_stream_into_cache(path, &stream_buf, cache, sum_sq, total_samples);
    Ok(())
}

fn stream_with_ffmpeg(
    sound_id: u64,
    path: &Path,
    allow_overlap: bool,
    seq_id: u64,
    mixer: &Arc<Mutex<MixerInner>>,
    cache: &Arc<Mutex<BoundedAudioCache>>,
    play_gen: &Arc<AtomicU64>,
) -> Result<(), String> {
    let mut child = Command::new("ffmpeg")
        .args(["-v", "error", "-i"])
        .arg(path)
        .args(["-vn", "-f", "f32le", "-ac", "2", "-ar", "48000", "pipe:1"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("failed to spawn ffmpeg: {e}"))?;

    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| "ffmpeg stdout pipe missing".to_string())?;

    let stream_buf = StreamingBuffer::new_streaming(0.0);
    let mut byte_buf = [0u8; 16384];
    let mut pending = Vec::<u8>::with_capacity(16);
    let mut batch = Vec::<f32>::with_capacity(STREAM_BATCH_FRAMES * TARGET_CHANNELS);

    let mut voice_started = false;
    let mut sum_sq = 0.0f64;
    let mut total_samples = 0usize;

    loop {
        if stream_buf.cancel.load(Ordering::Relaxed)
            || (!allow_overlap && play_gen.load(Ordering::SeqCst) != seq_id)
        {
            stream_buf.cancel.store(true, Ordering::Relaxed);
            let _ = child.kill();
            let _ = child.wait();
            return Ok(());
        }

        let n = match stdout.read(&mut byte_buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("ffmpeg read error: {e}"));
            }
        };

        pending.extend_from_slice(&byte_buf[..n]);
        let usable = pending.len() - (pending.len() % 8); // 8 bytes = 1 stereo f32 frame
        if usable > 0 {
            for chunk in pending[..usable].chunks_exact(4) {
                batch.push(f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]));
            }
            pending.drain(..usable);
        }

        let threshold_samples = if !voice_started {
            STREAM_START_FRAMES * TARGET_CHANNELS
        } else {
            STREAM_BATCH_FRAMES * TARGET_CHANNELS
        };

        if batch.len() >= threshold_samples {
            push_batch_to_stream(&mut batch, &stream_buf, &mut sum_sq, &mut total_samples);
            if !voice_started {
                if !activate_streaming_voice(
                    sound_id,
                    allow_overlap,
                    seq_id,
                    &stream_buf,
                    mixer,
                    play_gen,
                ) {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Ok(());
                }
                voice_started = true;
            }
        }
    }

    if !batch.is_empty() {
        push_batch_to_stream(&mut batch, &stream_buf, &mut sum_sq, &mut total_samples);
    }

    let status = child.wait().map_err(|e| format!("ffmpeg wait failed: {e}"))?;
    if total_samples == 0 {
        return Err(format!("ffmpeg produced no audio samples (status {status})"));
    }

    if !voice_started {
        if !activate_streaming_voice(
            sound_id,
            allow_overlap,
            seq_id,
            &stream_buf,
            mixer,
            play_gen,
        ) {
            return Ok(());
        }
    }

    finalize_stream_into_cache(path, &stream_buf, cache, sum_sq, total_samples);
    Ok(())
}

fn open_pulse_virtual_sink() -> Option<PaSimple> {
    let spec = PaSpec {
        format: PaFormat::FLOAT32NE,
        channels: TARGET_CHANNELS as u8,
        rate: TARGET_SAMPLE_RATE,
    };
    let attr = libpulse_binding::def::BufferAttr {
        maxlength: u32::MAX,
        tlength: (480 * 2 * 4 * 6) as u32,
        prebuf: u32::MAX,
        minreq: (480 * 2 * 4) as u32,
        fragsize: u32::MAX,
    };
    PaSimple::new(
        None,
        "RustSoundpad",
        PaDirection::Playback,
        Some("rust_soundpad_sink"),
        "Soundboard",
        &spec,
        None,
        Some(&attr),
    )
    .ok()
}

fn mixer_loop(
    mixer: Arc<Mutex<MixerInner>>,
    speaker_rb_prod: Arc<Mutex<ringbuf::HeapProd<f32>>>,
    shutdown: Arc<AtomicBool>,
) {
    let mut pa_sink: Option<PaSimple> = open_pulse_virtual_sink();
    let mut retry_ticks = 0u32;
    let chunk_samples = MIX_CHUNK_FRAMES * TARGET_CHANNELS;
    let mut spk_buf = vec![0.0f32; chunk_samples];
    let mut mic_buf = vec![0.0f32; chunk_samples];

    let tick_dur = Duration::from_millis(10);
    let mut next_tick = std::time::Instant::now() + tick_dur;
    let mut was_playing = false;

    while !shutdown.load(Ordering::Relaxed) {
        spk_buf.fill(0.0);
        mic_buf.fill(0.0);

        let (mic_enabled, has_audio);
        let mut finished_voices_to_drop: Vec<ActiveVoice> = Vec::new();
        {
            let mut g = match mixer.lock() {
                Ok(g) => g,
                Err(_) => {
                    thread::sleep(tick_dur);
                    continue;
                }
            };

            mic_enabled = g.mic_enabled;
            let spk_enabled = g.speakers_enabled;
            let master_vol = g.master_volume;
            let spk_vol = g.speakers_volume * master_vol;
            let mic_vol = g.mic_volume * master_vol;
            let auto_norm = g.auto_normalize;
            let repeat = g.repeat_enabled;
            let auto_adv = g.auto_advance;
            let paused = g.is_paused;

            if paused || g.voices.is_empty() {
                has_audio = false;
                g.mic_peak *= 0.85;
            } else {
                has_audio = true;
                let mut finished_primary_id: Option<u64> = None;

                let mut i = 0;
                while i < g.voices.len() {
                    let is_complete = g.voices[i].buffer.is_complete.load(Ordering::Acquire);
                    let norm = if auto_norm {
                        g.voices[i].buffer.norm_gain()
                    } else {
                        1.0
                    };
                    let is_primary = g.voices[i].is_primary;
                    let mut pos = g.voices[i].frame_pos;
                    let mut voice_done = false;

                    if let Ok(samples) = g.voices[i].buffer.samples.read() {
                        let available_frames = samples.len() / TARGET_CHANNELS;

                        for f in 0..MIX_CHUNK_FRAMES {
                            if pos >= available_frames {
                                if is_complete {
                                    if repeat && is_primary && available_frames > 0 {
                                        pos = 0;
                                    } else {
                                        voice_done = true;
                                        break;
                                    }
                                } else {
                                    break;
                                }
                            }
                            let s_idx = pos * TARGET_CHANNELS;
                            let l = samples[s_idx] * norm;
                            let r = samples[s_idx + 1] * norm;
                            let o_idx = f * TARGET_CHANNELS;
                            if spk_enabled {
                                spk_buf[o_idx] += l * spk_vol;
                                spk_buf[o_idx + 1] += r * spk_vol;
                            }
                            if mic_enabled {
                                mic_buf[o_idx] += l * mic_vol;
                                mic_buf[o_idx + 1] += r * mic_vol;
                            }
                            pos += 1;
                        }

                        if is_complete
                            && pos >= available_frames
                            && !(repeat && is_primary && available_frames > 0)
                        {
                            voice_done = true;
                        }
                    }

                    g.voices[i].frame_pos = pos;
                    if voice_done {
                        let finished = g.voices.remove(i);
                        if finished.is_primary && auto_adv {
                            finished_primary_id = Some(finished.sound_id);
                        }
                        finished_voices_to_drop.push(finished);
                    } else {
                        i += 1;
                    }
                }

                if let Some(fid) = finished_primary_id {
                    g.pending_auto_advance = Some(fid);
                }

                let mut peak = 0.0f32;
                for s in &mut spk_buf {
                    *s = soft_limit(*s);
                }
                for s in &mut mic_buf {
                    *s = soft_limit(*s);
                    let a = s.abs();
                    if a > peak {
                        peak = a;
                    }
                }
                g.mic_peak = if mic_enabled { peak } else { 0.0 };
            }
        }

        if !finished_voices_to_drop.is_empty() {
            drop(finished_voices_to_drop);
            release_freed_ram_to_os();
        }

        if has_audio {
            if let Ok(mut prod) = speaker_rb_prod.lock() {
                if !was_playing && prod.occupied_len() < 2880 {
                    let prebuf = [0.0f32; 2880];
                    let _ = prod.push_slice(&prebuf);
                }
                let _ = prod.push_slice(&spk_buf);
            }
        }
        was_playing = has_audio;

        if mic_enabled && has_audio {
            if pa_sink.is_none() {
                retry_ticks += 1;
                if retry_ticks >= 50 {
                    retry_ticks = 0;
                    pa_sink = open_pulse_virtual_sink();
                }
            }
            if let Some(ref sink) = pa_sink {
                let bytes: &[u8] = unsafe {
                    std::slice::from_raw_parts(
                        mic_buf.as_ptr() as *const u8,
                        mic_buf.len() * std::mem::size_of::<f32>(),
                    )
                };
                if sink.write(bytes).is_err() {
                    pa_sink = None;
                }
            }
        }

        let now = std::time::Instant::now();
        if next_tick > now {
            thread::sleep(next_tick - now);
            next_tick += tick_dur;
        } else {
            next_tick = now + tick_dur;
        }
    }
}

#[inline]
pub fn soft_limit(x: f32) -> f32 {
    if x.abs() <= 0.95 {
        x
    } else {
        x.tanh()
    }
}

pub fn probe_audio_duration(path: &Path) -> Option<f32> {
    if let Some(dur) = probe_duration_symphonia(path) {
        return Some(dur);
    }
    if let Some(dur) = probe_duration_ffprobe(path) {
        return Some(dur);
    }
    None
}

fn probe_duration_symphonia(path: &Path) -> Option<f32> {
    let file = File::open(path).ok()?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());
    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }

    let probed = symphonia::default::get_probe()
        .format(
            &hint,
            mss,
            &FormatOptions::default(),
            &MetadataOptions::default(),
        )
        .ok()?;

    let format = probed.format;
    let track = format
        .tracks()
        .iter()
        .find(|t| t.codec_params.codec != symphonia::core::codecs::CODEC_TYPE_NULL)?;

    if let (Some(n_frames), Some(rate)) =
        (track.codec_params.n_frames, track.codec_params.sample_rate)
    {
        if rate > 0 {
            return Some(n_frames as f32 / rate as f32);
        }
    }
    None
}

fn probe_duration_ffprobe(path: &Path) -> Option<f32> {
    let output = std::process::Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-show_entries",
            "format=duration",
            "-of",
            "default=noprint_wrappers=1:nokey=1",
        ])
        .arg(path)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let secs = text.trim().parse::<f32>().ok()?;
    if secs.is_finite() && secs >= 0.0 {
        Some(secs)
    } else {
        None
    }
}

pub fn decode_audio_file(path: &Path) -> Result<DecodedAudio, String> {
    match decode_with_symphonia(path) {
        Ok(audio) => Ok(audio),
        Err(sym_err) => match decode_with_ffmpeg(path) {
            Ok(audio) => Ok(audio),
            Err(ff_err) => Err(format!("{sym_err}; ffmpeg fallback failed: {ff_err}")),
        },
    }
}

fn decode_with_symphonia(path: &Path) -> Result<DecodedAudio, String> {
    let file = File::open(path).map_err(|e| format!("Cannot open {}: {e}", path.display()))?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());

    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }

    let probed = symphonia::default::get_probe()
        .format(
            &hint,
            mss,
            &FormatOptions::default(),
            &MetadataOptions::default(),
        )
        .map_err(|e| format!("Unsupported or invalid audio {}: {e}", path.display()))?;

    let mut format = probed.format;
    let track = format
        .tracks()
        .iter()
        .find(|t| t.codec_params.codec != symphonia::core::codecs::CODEC_TYPE_NULL)
        .ok_or_else(|| format!("No audio track found in {}", path.display()))?
        .clone();

    let track_id = track.id;
    let src_rate = track.codec_params.sample_rate.unwrap_or(TARGET_SAMPLE_RATE);
    let mut decoder = symphonia::default::get_codecs()
        .make(&track.codec_params, &DecoderOptions::default())
        .map_err(|e| format!("Decoder init failed for {}: {e}", path.display()))?;

    let mut stereo_src: Vec<f32> = Vec::new();
    let mut sample_buf: Option<SampleBuffer<f32>> = None;
    let mut detected_rate = src_rate;

    loop {
        let packet = match format.next_packet() {
            Ok(p) => p,
            Err(SymphoniaError::IoError(e))
                if e.kind() == std::io::ErrorKind::UnexpectedEof =>
            {
                break;
            }
            Err(SymphoniaError::ResetRequired) => {
                decoder.reset();
                continue;
            }
            Err(e) => return Err(format!("Read error in {}: {e}", path.display())),
        };

        if packet.track_id() != track_id {
            continue;
        }

        match decoder.decode(&packet) {
            Ok(decoded) => {
                let spec = *decoded.spec();
                detected_rate = spec.rate;
                let channels = spec.channels.count().max(1);
                let frames = decoded.frames();
                if frames == 0 {
                    continue;
                }

                let buf = sample_buf.get_or_insert_with(|| {
                    SampleBuffer::<f32>::new(decoded.capacity() as u64, spec)
                });
                if buf.capacity() < frames * channels {
                    *buf = SampleBuffer::<f32>::new(decoded.capacity() as u64, spec);
                }
                buf.copy_interleaved_ref(decoded);
                let raw = buf.samples();

                for f in 0..frames {
                    let base = f * channels;
                    let (l, r) = if channels == 1 {
                        (raw[base], raw[base])
                    } else {
                        (raw[base], raw[base + 1])
                    };
                    stereo_src.push(l);
                    stereo_src.push(r);
                }
            }
            Err(SymphoniaError::IoError(_)) | Err(SymphoniaError::DecodeError(_)) => continue,
            Err(e) => return Err(format!("Decode error in {}: {e}", path.display())),
        }
    }

    let samples = resample_stereo(&stereo_src, detected_rate, TARGET_SAMPLE_RATE);
    drop(stereo_src);
    release_freed_ram_to_os();
    let total_frames = samples.len() / TARGET_CHANNELS;
    let duration_secs = total_frames as f32 / TARGET_SAMPLE_RATE as f32;
    let norm_gain = compute_norm_gain(&samples);

    Ok(DecodedAudio {
        samples,
        duration_secs,
        norm_gain,
    })
}

fn decode_with_ffmpeg(path: &Path) -> Result<DecodedAudio, String> {
    let output = std::process::Command::new("ffmpeg")
        .args(["-v", "error", "-i"])
        .arg(path)
        .args(["-vn", "-f", "f32le", "-ac", "2", "-ar", "48000", "pipe:1"])
        .output()
        .map_err(|e| format!("failed to spawn ffmpeg: {e}"))?;

    if !output.status.success() {
        let err = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(if err.is_empty() {
            format!("ffmpeg exited with {}", output.status)
        } else {
            err
        });
    }

    let usable_bytes = output.stdout.len() - (output.stdout.len() % (4 * TARGET_CHANNELS));
    if usable_bytes == 0 {
        return Err("ffmpeg produced no audio samples".to_string());
    }

    let mut samples = Vec::with_capacity(usable_bytes / 4);
    for chunk in output.stdout[..usable_bytes].chunks_exact(4) {
        samples.push(f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]));
    }
    drop(output);
    release_freed_ram_to_os();

    let total_frames = samples.len() / TARGET_CHANNELS;
    let duration_secs = total_frames as f32 / TARGET_SAMPLE_RATE as f32;
    let norm_gain = compute_norm_gain(&samples);

    Ok(DecodedAudio {
        samples,
        duration_secs,
        norm_gain,
    })
}

fn compute_norm_gain(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return 1.0;
    }
    let sum_sq: f64 = samples.iter().map(|&s| (s as f64) * (s as f64)).sum();
    let rms = (sum_sq / samples.len() as f64).sqrt() as f32;
    if rms > 1e-4 {
        (0.18 / rms).clamp(0.15, 2.5)
    } else {
        1.0
    }
}

fn resample_stereo(input: &[f32], src_rate: u32, dst_rate: u32) -> Vec<f32> {
    if input.is_empty() || src_rate == 0 || src_rate == dst_rate {
        return input.to_vec();
    }
    let src_frames = input.len() / 2;
    if src_frames < 2 {
        return input.to_vec();
    }
    let ratio = src_rate as f64 / dst_rate as f64;
    let dst_frames = ((src_frames as f64) / ratio).ceil() as usize;
    let mut out = Vec::with_capacity(dst_frames * 2);

    for i in 0..dst_frames {
        let pos = i as f64 * ratio;
        let idx = (pos.floor() as usize).min(src_frames - 1);
        let next_idx = (idx + 1).min(src_frames - 1);
        let frac = (pos - idx as f64) as f32;

        let l0 = input[idx * 2];
        let r0 = input[idx * 2 + 1];
        let l1 = input[next_idx * 2];
        let r1 = input[next_idx * 2 + 1];

        out.push(l0 + (l1 - l0) * frac);
        out.push(r0 + (r1 - r0) * frac);
    }

    out
}
