use crate::audio::soft_limit;
use crate::model::RecordingSource;
use crate::pipewire::PipewireVirtualMic;
use libpulse_binding::def::BufferAttr;
use libpulse_binding::sample::{Format as PaFormat, Spec as PaSpec};
use libpulse_binding::stream::Direction as PaDirection;
use libpulse_simple_binding::Simple as PaSimple;
use std::fs::{self, File};
use std::io::{BufWriter, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub const RECORD_SAMPLE_RATE: u32 = 48_000;
pub const RECORD_CHANNELS: u16 = 2;
pub const RECORD_BITS_PER_SAMPLE: u16 = 16;
const CHUNK_FRAMES: usize = 1024; // Aligned with PipeWire 1024-frame quantum
const CHUNK_SAMPLES: usize = CHUNK_FRAMES * RECORD_CHANNELS as usize;
const CHUNK_F32_BYTES: usize = CHUNK_SAMPLES * std::mem::size_of::<f32>();
const DUAL_SYNC_TIMEOUT: Duration = Duration::from_millis(25);

struct ActiveRecording {
    stop_flag: Arc<AtomicBool>,
    started_at: Instant,
    worker: JoinHandle<Result<PathBuf, String>>,
}

pub struct AudioRecorder {
    active: Option<ActiveRecording>,
}

impl AudioRecorder {
    pub fn new() -> Self {
        Self { active: None }
    }

    pub fn is_recording(&self) -> bool {
        self.active.is_some()
    }

    pub fn elapsed_secs(&self) -> f32 {
        self.active
            .as_ref()
            .map(|a| a.started_at.elapsed().as_secs_f32())
            .unwrap_or(0.0)
    }

    pub fn start(
        &mut self,
        source: RecordingSource,
        selected_mic: Option<&str>,
        output_dir: &Path,
    ) -> Result<(), String> {
        if self.active.is_some() {
            return Err("Recording is already in progress".to_string());
        }

        fs::create_dir_all(output_dir)
            .map_err(|e| format!("Failed to create recording directory {}: {e}", output_dir.display()))?;

        let mic_source = selected_mic
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .or_else(PipewireVirtualMic::detect_default_physical_mic);

        let (primary_src, secondary_src) = match source {
            RecordingSource::Microphone => (
                mic_source.ok_or_else(|| "No physical microphone source found".to_string())?,
                None,
            ),
            RecordingSource::SystemAudio => (default_sink_monitor()?, None),
            RecordingSource::Both => (
                mic_source.ok_or_else(|| "No physical microphone source found".to_string())?,
                Some(default_sink_monitor()?),
            ),
        };

        let out_path = unique_recording_path(output_dir);
        let file = File::create(&out_path)
            .map_err(|e| format!("Failed to create recording file {}: {e}", out_path.display()))?;

        let stop_flag = Arc::new(AtomicBool::new(false));
        let stop_bg = Arc::clone(&stop_flag);
        let (ready_tx, ready_rx) = mpsc::sync_channel::<Result<(), String>>(1);
        let path_bg = out_path.clone();

        let worker = thread::spawn(move || {
            if let Some(sec_src) = secondary_src {
                record_dual_loop(file, path_bg, primary_src, sec_src, stop_bg, ready_tx)
            } else {
                record_single_loop(file, path_bg, primary_src, stop_bg, ready_tx)
            }
        });

        match ready_rx.recv() {
            Ok(Ok(())) => {
                self.active = Some(ActiveRecording {
                    stop_flag,
                    started_at: Instant::now(),
                    worker,
                });
                Ok(())
            }
            Ok(Err(e)) => {
                stop_flag.store(true, Ordering::Relaxed);
                let _ = worker.join();
                let _ = fs::remove_file(&out_path);
                Err(e)
            }
            Err(_) => {
                stop_flag.store(true, Ordering::Relaxed);
                let res = worker.join();
                let _ = fs::remove_file(&out_path);
                match res {
                    Ok(Err(e)) => Err(e),
                    _ => Err("Recording thread failed to initialize".to_string()),
                }
            }
        }
    }

    pub fn stop(&mut self) -> Result<Option<PathBuf>, String> {
        let Some(active) = self.active.take() else {
            return Ok(None);
        };
        active.stop_flag.store(true, Ordering::Relaxed);
        match active.worker.join() {
            Ok(Ok(path)) => Ok(Some(path)),
            Ok(Err(e)) => Err(e),
            Err(_) => Err("Recording thread panicked".to_string()),
        }
    }

    pub fn poll_finished(&mut self) -> Option<Result<PathBuf, String>> {
        if self.active.as_ref().is_some_and(|a| a.worker.is_finished()) {
            let active = self.active.take()?;
            match active.worker.join() {
                Ok(res) => Some(res),
                Err(_) => Some(Err("Recording thread panicked".to_string())),
            }
        } else {
            None
        }
    }
}

impl Drop for AudioRecorder {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

fn default_sink_monitor() -> Result<String, String> {
    let output = Command::new("pactl")
        .args(["get-default-sink"])
        .output()
        .map_err(|e| format!("Failed to run pactl get-default-sink: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(if stderr.is_empty() {
            format!("pactl get-default-sink exited with status {}", output.status)
        } else {
            format!("pactl get-default-sink failed: {stderr}")
        });
    }

    let sink = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if sink.is_empty() {
        return Err("No default sink returned by pactl get-default-sink".to_string());
    }
    Ok(format!("{sink}.monitor"))
}

fn open_record_stream(source: &str, stream_name: &str) -> Result<PaSimple, String> {
    let spec = PaSpec {
        format: PaFormat::FLOAT32NE,
        channels: RECORD_CHANNELS as u8,
        rate: RECORD_SAMPLE_RATE,
    };
    let attr = BufferAttr {
        maxlength: u32::MAX,
        tlength: u32::MAX,
        prebuf: u32::MAX,
        minreq: u32::MAX,
        fragsize: CHUNK_F32_BYTES as u32,
    };
    PaSimple::new(
        None,
        "SoundPat",
        PaDirection::Record,
        Some(source),
        stream_name,
        &spec,
        None,
        Some(&attr),
    )
    .map_err(|e| format!("Failed to open PulseAudio record stream '{source}': {e:?}"))
}

fn bytes_ne_to_f32(raw: &[u8], out: &mut Vec<f32>) {
    out.clear();
    out.reserve(raw.len() / 4);
    for chunk in raw.chunks_exact(4) {
        out.push(f32::from_ne_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]));
    }
}

#[inline]
fn f32_to_s16(sample: f32) -> i16 {
    let limited = if sample.is_finite() {
        soft_limit(sample)
    } else {
        0.0
    };
    (limited.clamp(-1.0, 1.0) * i16::MAX as f32).round() as i16
}

fn record_single_loop(
    file: File,
    out_path: PathBuf,
    source: String,
    stop_flag: Arc<AtomicBool>,
    ready_tx: mpsc::SyncSender<Result<(), String>>,
) -> Result<PathBuf, String> {
    let stream = match open_record_stream(&source, "SoundPat Record") {
        Ok(s) => s,
        Err(e) => {
            let _ = ready_tx.send(Err(e.clone()));
            return Err(e);
        }
    };

    let mut writer = BufWriter::new(file);
    if let Err(e) = writer.write_all(&build_wav_header(0)) {
        let msg = format!("Failed to write WAV header: {e}");
        let _ = ready_tx.send(Err(msg.clone()));
        return Err(msg);
    }

    let _ = ready_tx.send(Ok(()));

    let mut raw_buf = [0u8; CHUNK_F32_BYTES];
    let mut f32_buf = Vec::with_capacity(CHUNK_SAMPLES);
    let mut wav_bytes = Vec::with_capacity(CHUNK_SAMPLES * 2);
    let mut data_bytes: u64 = 0;
    let mut stream_err: Option<String> = None;

    while !stop_flag.load(Ordering::Relaxed) {
        if let Err(e) = stream.read(&mut raw_buf) {
            stream_err = Some(format!("Audio capture error: {e:?}"));
            break;
        }
        bytes_ne_to_f32(&raw_buf, &mut f32_buf);
        mix_f32_to_s16_bytes(&f32_buf, &[], &mut wav_bytes);
        if let Err(e) = writer.write_all(&wav_bytes) {
            stream_err = Some(format!("Failed to write audio data: {e}"));
            break;
        }
        data_bytes = data_bytes.saturating_add(wav_bytes.len() as u64);
    }

    let mut file = writer
        .into_inner()
        .map_err(|e| format!("Failed to flush WAV buffer: {}", e.error()))?;
    finalize_wav_header(&mut file, data_bytes)?;

    if let Some(err) = stream_err {
        return Err(err);
    }
    Ok(out_path)
}

fn spawn_f32_reader(
    source: String,
    stream_name: &'static str,
    stop_flag: Arc<AtomicBool>,
) -> (
    mpsc::Receiver<Vec<f32>>,
    mpsc::Receiver<Result<(), String>>,
    JoinHandle<()>,
) {
    let (pcm_tx, pcm_rx) = mpsc::sync_channel::<Vec<f32>>(64);
    let (ready_tx, ready_rx) = mpsc::sync_channel::<Result<(), String>>(1);

    let handle = thread::spawn(move || {
        let stream = match open_record_stream(&source, stream_name) {
            Ok(s) => s,
            Err(e) => {
                let _ = ready_tx.send(Err(e));
                return;
            }
        };
        let _ = ready_tx.send(Ok(()));

        let mut raw = [0u8; CHUNK_F32_BYTES];
        while !stop_flag.load(Ordering::Relaxed) {
            if stream.read(&mut raw).is_err() {
                break;
            }
            let mut samples = Vec::with_capacity(CHUNK_SAMPLES);
            bytes_ne_to_f32(&raw, &mut samples);
            if pcm_tx.send(samples).is_err() {
                break;
            }
        }
    });

    (pcm_rx, ready_rx, handle)
}

fn record_dual_loop(
    file: File,
    out_path: PathBuf,
    mic_source: String,
    sys_source: String,
    stop_flag: Arc<AtomicBool>,
    ready_tx: mpsc::SyncSender<Result<(), String>>,
) -> Result<PathBuf, String> {
    let (mic_rx, mic_ready, mic_handle) =
        spawn_f32_reader(mic_source, "SoundPat Record Mic", Arc::clone(&stop_flag));
    let (sys_rx, sys_ready, sys_handle) =
        spawn_f32_reader(sys_source, "SoundPat Record System", Arc::clone(&stop_flag));

    let mic_init = mic_ready
        .recv()
        .unwrap_or_else(|_| Err("Microphone reader failed to start".to_string()));
    let sys_init = sys_ready
        .recv()
        .unwrap_or_else(|_| Err("System audio reader failed to start".to_string()));

    if let Err(e) = mic_init.and(sys_init) {
        stop_flag.store(true, Ordering::Relaxed);
        drop(mic_rx);
        drop(sys_rx);
        let _ = mic_handle.join();
        let _ = sys_handle.join();
        let _ = ready_tx.send(Err(e.clone()));
        return Err(e);
    }

    let mut writer = BufWriter::new(file);
    if let Err(e) = writer.write_all(&build_wav_header(0)) {
        let msg = format!("Failed to write WAV header: {e}");
        stop_flag.store(true, Ordering::Relaxed);
        drop(mic_rx);
        drop(sys_rx);
        let _ = mic_handle.join();
        let _ = sys_handle.join();
        let _ = ready_tx.send(Err(msg.clone()));
        return Err(msg);
    }

    let _ = ready_tx.send(Ok(()));

    let mut mixed_bytes = Vec::with_capacity(CHUNK_SAMPLES * 2);
    let mut data_bytes: u64 = 0;
    let mut write_err: Option<String> = None;
    let mut mic_open = true;
    let mut sys_open = true;

    while !stop_flag.load(Ordering::Relaxed) && (mic_open || sys_open) {
        let mut mic_timed_out = false;
        let mic_chunk = if mic_open {
            match mic_rx.recv_timeout(DUAL_SYNC_TIMEOUT) {
                Ok(c) => Some(c),
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    mic_timed_out = true;
                    None
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    mic_open = false;
                    None
                }
            }
        } else {
            None
        };

        let sys_chunk = if sys_open {
            if mic_timed_out {
                match sys_rx.try_recv() {
                    Ok(c) => Some(c),
                    Err(mpsc::TryRecvError::Empty) => None,
                    Err(mpsc::TryRecvError::Disconnected) => {
                        sys_open = false;
                        None
                    }
                }
            } else {
                match sys_rx.recv_timeout(DUAL_SYNC_TIMEOUT) {
                    Ok(c) => Some(c),
                    Err(mpsc::RecvTimeoutError::Timeout) => None,
                    Err(mpsc::RecvTimeoutError::Disconnected) => {
                        sys_open = false;
                        None
                    }
                }
            }
        } else {
            None
        };

        match (mic_chunk, sys_chunk) {
            (Some(a), Some(b)) => mix_f32_to_s16_bytes(&a, &b, &mut mixed_bytes),
            (Some(a), None) => mix_f32_to_s16_bytes(&a, &[], &mut mixed_bytes),
            (None, Some(b)) => mix_f32_to_s16_bytes(&[], &b, &mut mixed_bytes),
            (None, None) => continue,
        }

        if let Err(e) = writer.write_all(&mixed_bytes) {
            write_err = Some(format!("Failed to write audio data: {e}"));
            break;
        }
        data_bytes = data_bytes.saturating_add(mixed_bytes.len() as u64);
    }

    stop_flag.store(true, Ordering::Relaxed);
    drop(mic_rx);
    drop(sys_rx);
    let _ = mic_handle.join();
    let _ = sys_handle.join();

    let mut file = writer
        .into_inner()
        .map_err(|e| format!("Failed to flush WAV buffer: {}", e.error()))?;
    finalize_wav_header(&mut file, data_bytes)?;

    if let Some(err) = write_err {
        return Err(err);
    }
    Ok(out_path)
}

pub fn mix_f32_to_s16_bytes(a: &[f32], b: &[f32], out: &mut Vec<u8>) {
    out.clear();
    let len = a.len().max(b.len());
    out.reserve(len * 2);
    for i in 0..len {
        let sa = a.get(i).copied().unwrap_or(0.0);
        let sb = b.get(i).copied().unwrap_or(0.0);
        let mixed = f32_to_s16(sa + sb);
        out.extend_from_slice(&mixed.to_le_bytes());
    }
}

pub fn build_wav_header(data_bytes: u32) -> [u8; 44] {
    let byte_rate: u32 =
        RECORD_SAMPLE_RATE * RECORD_CHANNELS as u32 * (RECORD_BITS_PER_SAMPLE as u32 / 8);
    let block_align: u16 = RECORD_CHANNELS * (RECORD_BITS_PER_SAMPLE / 8);
    let riff_chunk_size: u32 = 36u32.saturating_add(data_bytes);

    let mut hdr = [0u8; 44];
    hdr[0..4].copy_from_slice(b"RIFF");
    hdr[4..8].copy_from_slice(&riff_chunk_size.to_le_bytes());
    hdr[8..12].copy_from_slice(b"WAVE");
    hdr[12..16].copy_from_slice(b"fmt ");
    hdr[16..20].copy_from_slice(&16u32.to_le_bytes()); // PCM fmt chunk size
    hdr[20..22].copy_from_slice(&1u16.to_le_bytes()); // AudioFormat = 1 (PCM)
    hdr[22..24].copy_from_slice(&RECORD_CHANNELS.to_le_bytes());
    hdr[24..28].copy_from_slice(&RECORD_SAMPLE_RATE.to_le_bytes());
    hdr[28..32].copy_from_slice(&byte_rate.to_le_bytes());
    hdr[32..34].copy_from_slice(&block_align.to_le_bytes());
    hdr[34..36].copy_from_slice(&RECORD_BITS_PER_SAMPLE.to_le_bytes());
    hdr[36..40].copy_from_slice(b"data");
    hdr[40..44].copy_from_slice(&data_bytes.to_le_bytes());
    hdr
}

pub fn finalize_wav_header(file: &mut File, data_bytes: u64) -> Result<(), String> {
    let data_u32 = data_bytes.min((u32::MAX - 36) as u64) as u32;
    let riff_size = 36u32.saturating_add(data_u32);

    file.seek(SeekFrom::Start(4))
        .map_err(|e| format!("Failed to seek WAV RIFF size: {e}"))?;
    file.write_all(&riff_size.to_le_bytes())
        .map_err(|e| format!("Failed to write WAV RIFF size: {e}"))?;

    file.seek(SeekFrom::Start(40))
        .map_err(|e| format!("Failed to seek WAV data size: {e}"))?;
    file.write_all(&data_u32.to_le_bytes())
        .map_err(|e| format!("Failed to write WAV data size: {e}"))?;

    file.flush()
        .map_err(|e| format!("Failed to flush WAV file: {e}"))?;
    Ok(())
}

fn format_local_timestamp() -> String {
    if let Ok(output) = Command::new("date").arg("+%Y-%m-%d_%H-%M-%S").output() {
        if output.status.success() {
            let s = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !s.is_empty() {
                return s;
            }
        }
    }

    // Fallback: UTC calendar breakdown from SystemTime
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let hour = rem / 3_600;
    let minute = (rem % 3_600) / 60;
    let second = rem % 60;
    let (year, month, day) = civil_from_days(days);
    format!("{year:04}-{month:02}-{day:02}_{hour:02}-{minute:02}-{second:02}")
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = y + if m <= 2 { 1 } else { 0 };
    (year, m as u32, d as u32)
}

fn unique_recording_path(output_dir: &Path) -> PathBuf {
    let stamp = format_local_timestamp();
    let candidate = output_dir.join(format!("rec_{stamp}.wav"));
    if !candidate.exists() {
        return candidate;
    }
    for idx in 1..1000u32 {
        let numbered = output_dir.join(format!("rec_{stamp}_{idx}.wav"));
        if !numbered.exists() {
            return numbered;
        }
    }
    candidate
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    #[test]
    fn wav_header_and_finalize_roundtrip() {
        let tmp = std::env::temp_dir().join("soundpat_test_wav_hdr.wav");
        {
            let mut f = File::create(&tmp).unwrap();
            f.write_all(&build_wav_header(0)).unwrap();
            let pcm = [0u8; 3840];
            f.write_all(&pcm).unwrap();
            finalize_wav_header(&mut f, pcm.len() as u64).unwrap();
        }

        let mut bytes = Vec::new();
        File::open(&tmp).unwrap().read_to_end(&mut bytes).unwrap();
        let _ = fs::remove_file(&tmp);

        assert_eq!(bytes.len(), 44 + 3840);
        assert_eq!(&bytes[0..4], b"RIFF");
        assert_eq!(u32::from_le_bytes(bytes[4..8].try_into().unwrap()), 36 + 3840);
        assert_eq!(&bytes[8..12], b"WAVE");
        assert_eq!(&bytes[12..16], b"fmt ");
        assert_eq!(u32::from_le_bytes(bytes[16..20].try_into().unwrap()), 16);
        assert_eq!(u16::from_le_bytes(bytes[20..22].try_into().unwrap()), 1);
        assert_eq!(u16::from_le_bytes(bytes[22..24].try_into().unwrap()), 2);
        assert_eq!(u32::from_le_bytes(bytes[24..28].try_into().unwrap()), 48_000);
        assert_eq!(u32::from_le_bytes(bytes[28..32].try_into().unwrap()), 192_000);
        assert_eq!(u16::from_le_bytes(bytes[32..34].try_into().unwrap()), 4);
        assert_eq!(u16::from_le_bytes(bytes[34..36].try_into().unwrap()), 16);
        assert_eq!(&bytes[36..40], b"data");
        assert_eq!(u32::from_le_bytes(bytes[40..44].try_into().unwrap()), 3840);
    }

    #[test]
    fn mix_f32_applies_soft_limit_and_converts_to_s16() {
        let a = [0.5f32, -0.5, 1.5, -1.5, 0.25];
        let b = [0.25f32, -0.25, 1.0, -1.0];
        let mut out = Vec::new();
        mix_f32_to_s16_bytes(&a, &b, &mut out);
        assert_eq!(out.len(), 10);

        let s0 = i16::from_le_bytes([out[0], out[1]]);
        let s1 = i16::from_le_bytes([out[2], out[3]]);
        let s2 = i16::from_le_bytes([out[4], out[5]]);
        let s3 = i16::from_le_bytes([out[6], out[7]]);
        let s4 = i16::from_le_bytes([out[8], out[9]]);

        assert!((s0 - (0.75 * i16::MAX as f32) as i16).abs() <= 1);
        assert!((s1 - (-0.75 * i16::MAX as f32) as i16).abs() <= 1);
        // 2.5 passed through tanh() is ~0.9866 -> below full clip, smooth limiting
        assert!(s2 > 31_500 && s2 < i16::MAX);
        assert!(s3 < -31_500 && s3 > -i16::MAX);
        assert!((s4 - (0.25 * i16::MAX as f32) as i16).abs() <= 1);
    }
}
