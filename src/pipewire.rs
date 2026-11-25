use std::process::Command;
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioDeviceInfo {
    pub name: String,
    pub description: String,
}

#[derive(Clone)]
pub struct PipewireCleanup {
    module_ids: Arc<Mutex<Vec<u32>>>,
}

impl PipewireCleanup {
    pub fn unload_all(&self) {
        let ids: Vec<u32> = {
            let mut guard = self.module_ids.lock().unwrap_or_else(|e| e.into_inner());
            guard.drain(..).rev().collect()
        };
        for id in ids {
            let _ = Command::new("pactl")
                .args(["unload-module", &id.to_string()])
                .output();
        }
    }
}

pub struct PipewireVirtualMic {
    module_ids: Arc<Mutex<Vec<u32>>>,
    pub status_message: String,
    pub is_ready: bool,
    pub active_mic_source: Option<String>,
}

impl PipewireVirtualMic {
    pub fn new() -> Self {
        Self {
            module_ids: Arc::new(Mutex::new(Vec::new())),
            status_message: "Initializing PipeWire virtual mic...".to_string(),
            is_ready: false,
            active_mic_source: None,
        }
    }

    pub fn cleanup_handle(&self) -> PipewireCleanup {
        PipewireCleanup {
            module_ids: Arc::clone(&self.module_ids),
        }
    }

    pub fn cleanup(&self) {
        self.cleanup_handle().unload_all();
    }

    pub fn setup(&mut self, preferred_mic: Option<&str>) -> Result<(), String> {
        self.cleanup();
        self.is_ready = false;

        // 1. Create virtual null-sink that receives soundboard audio + looped mic audio
        let null_sink_id = Self::load_module(&[
            "module-null-sink",
            "sink_name=rust_soundpad_sink",
            "sink_properties=device.description=\"Rust Soundpad Mix\"",
        ])
        .or_else(|_| {
            Self::load_module(&[
                "module-null-sink",
                "sink_name=rust_soundpad_sink",
                "sink_properties=device.description=Rust_Soundpad_Mix",
            ])
        })?;
        self.push_module(null_sink_id);

        // 2. Expose the monitor of rust_soundpad_sink as a real virtual microphone source
        // so Discord, Steam, OBS, and games list it as an input device.
        if let Ok(remap_id) = Self::load_module(&[
            "module-remap-source",
            "master=rust_soundpad_sink.monitor",
            "source_name=rust_soundpad_virtual_mic",
            "source_properties=device.description=\"Rust Soundpad Virtual Mic\"",
        ])
        .or_else(|_| {
            Self::load_module(&[
                "module-remap-source",
                "master=rust_soundpad_sink.monitor",
                "source_name=rust_soundpad_virtual_mic",
                "source_properties=device.description=Rust_Soundpad_Virtual_Mic",
            ])
        }) {
            self.push_module(remap_id);
        }

        // 3. Connect selected physical microphone (or default non-monitor physical mic) via loopback
        let mic_source = preferred_mic
            .map(|s| s.to_string())
            .filter(|s| !s.is_empty())
            .or_else(Self::detect_default_physical_mic);

        self.active_mic_source = mic_source.clone();

        if let Some(ref src) = mic_source {
            let source_arg = format!("source={src}");
            match Self::load_module(&[
                "module-loopback",
                &source_arg,
                "sink=rust_soundpad_sink",
                "latency_msec=20",
                "source_dont_move=true",
                "sink_dont_move=true",
            ]) {
                Ok(loopback_id) => {
                    self.push_module(loopback_id);
                    self.status_message =
                        format!("Virtual Mic active (Rust Soundpad Virtual Mic + {src})");
                }
                Err(e) => {
                    self.status_message = format!(
                        "Virtual sink active (mic loopback failed for {src}: {e})"
                    );
                }
            }
        } else {
            self.status_message =
                "Virtual Mic active (soundboard only; no physical mic found)".to_string();
        }

        self.is_ready = true;
        Ok(())
    }

    fn push_module(&self, id: u32) {
        if let Ok(mut guard) = self.module_ids.lock() {
            guard.push(id);
        }
    }

    fn load_module(args: &[&str]) -> Result<u32, String> {
        let mut cmd_args = Vec::with_capacity(args.len() + 1);
        cmd_args.push("load-module");
        cmd_args.extend_from_slice(args);

        let output = Command::new("pactl")
            .args(&cmd_args)
            .output()
            .map_err(|e| format!("Failed to execute pactl: {e}"))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            return Err(if stderr.is_empty() {
                format!("pactl exited with status {}", output.status)
            } else {
                stderr
            });
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        stdout
            .trim()
            .parse::<u32>()
            .map_err(|e| format!("Unexpected pactl module id '{}': {e}", stdout.trim()))
    }

    pub fn list_physical_mics() -> Vec<AudioDeviceInfo> {
        parse_pactl_devices("sources")
            .into_iter()
            .filter(|d| {
                !d.name.ends_with(".monitor")
                    && !d.name.starts_with("rust_soundpad_")
            })
            .collect()
    }

    pub fn detect_default_physical_mic() -> Option<String> {
        if let Ok(out) = Command::new("pactl").args(["get-default-source"]).output() {
            if out.status.success() {
                let def = String::from_utf8_lossy(&out.stdout).trim().to_string();
                if !def.is_empty()
                    && !def.ends_with(".monitor")
                    && !def.starts_with("rust_soundpad_")
                {
                    return Some(def);
                }
            }
        }
        Self::list_physical_mics().into_iter().next().map(|d| d.name)
    }
}

impl Drop for PipewireVirtualMic {
    fn drop(&mut self) {
        self.cleanup();
    }
}

fn parse_pactl_devices(kind: &str) -> Vec<AudioDeviceInfo> {
    let Ok(output) = Command::new("pactl").args(["list", kind]).output() else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }

    let text = String::from_utf8_lossy(&output.stdout);
    let mut devices = Vec::new();
    let mut current_name: Option<String> = None;
    let mut current_desc: Option<String> = None;

    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("Source #") || trimmed.starts_with("Sink #") {
            if let Some(name) = current_name.take() {
                let description = current_desc.take().unwrap_or_else(|| name.clone());
                devices.push(AudioDeviceInfo { name, description });
            }
            current_desc = None;
        } else if let Some(rest) = trimmed.strip_prefix("Name:") {
            current_name = Some(rest.trim().to_string());
        } else if let Some(rest) = trimmed.strip_prefix("Description:") {
            current_desc = Some(rest.trim().to_string());
        }
    }

    if let Some(name) = current_name.take() {
        let description = current_desc.take().unwrap_or_else(|| name.clone());
        devices.push(AudioDeviceInfo { name, description });
    }

    devices
}
