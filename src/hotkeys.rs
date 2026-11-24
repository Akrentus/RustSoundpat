use crate::model::Hotkey;
use evdev::{EventSummary, KeyCode};
use std::collections::HashSet;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
pub enum HotkeyAction {
    PlaySound(u64),
    StopAll,
    TogglePause,
    ToggleRecord,
    Captured(Hotkey),
}

#[derive(Debug, Clone)]
pub struct HotkeyBindings {
    pub enabled: bool,
    pub stop_hotkey: Option<Hotkey>,
    pub pause_hotkey: Option<Hotkey>,
    pub record_hotkey: Option<Hotkey>,
    pub sounds: Vec<(u64, Hotkey)>,
    pub capture_mode: bool,
}

#[derive(Debug, Clone, Default)]
pub struct HotkeyStatus {
    pub permission_denied: bool,
    pub device_count: usize,
    pub status_text: String,
}

pub struct HotkeyManager {
    bindings: Arc<Mutex<HotkeyBindings>>,
    status: Arc<Mutex<HotkeyStatus>>,
    shutdown: Arc<AtomicBool>,
    rx: mpsc::Receiver<HotkeyAction>,
}

impl HotkeyManager {
    pub fn start(repaint_ctx: Option<egui::Context>) -> Self {
        let bindings = Arc::new(Mutex::new(HotkeyBindings {
            enabled: true,
            stop_hotkey: None,
            pause_hotkey: None,
            record_hotkey: None,
            sounds: Vec::new(),
            capture_mode: false,
        }));
        let status = Arc::new(Mutex::new(HotkeyStatus::default()));
        let shutdown = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::channel();

        let bindings_bg = Arc::clone(&bindings);
        let status_bg = Arc::clone(&status);
        let shutdown_bg = Arc::clone(&shutdown);

        thread::spawn(move || {
            run_evdev_loop(bindings_bg, status_bg, shutdown_bg, tx, repaint_ctx);
        });

        Self {
            bindings,
            status,
            shutdown,
            rx,
        }
    }

    pub fn update_bindings(
        &self,
        enabled: bool,
        stop_hotkey: Option<Hotkey>,
        pause_hotkey: Option<Hotkey>,
        record_hotkey: Option<Hotkey>,
        sounds: Vec<(u64, Hotkey)>,
        capture_mode: bool,
    ) {
        if let Ok(mut g) = self.bindings.lock() {
            g.enabled = enabled;
            g.stop_hotkey = stop_hotkey;
            g.pause_hotkey = pause_hotkey;
            g.record_hotkey = record_hotkey;
            g.sounds = sounds;
            g.capture_mode = capture_mode;
        }
    }

    pub fn status(&self) -> HotkeyStatus {
        self.status
            .lock()
            .map(|g| g.clone())
            .unwrap_or_default()
    }

    pub fn poll_actions(&self) -> Vec<HotkeyAction> {
        let mut out = Vec::new();
        while let Ok(act) = self.rx.try_recv() {
            out.push(act);
        }
        out
    }
}

impl Drop for HotkeyManager {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
    }
}

fn open_keyboard_devices() -> (Vec<(PathBuf, evdev::Device)>, bool) {
    let mut devices = Vec::new();
    let mut saw_permission_denied = false;

    if let Ok(entries) = fs::read_dir("/dev/input") {
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if !name.starts_with("event") {
                continue;
            }
            match evdev::Device::open(&path) {
                Ok(dev) => {
                    let is_keyboard = dev.supported_keys().is_some_and(|keys| {
                        keys.contains(KeyCode::KEY_A)
                            || keys.contains(KeyCode::KEY_SPACE)
                            || keys.contains(KeyCode::KEY_ENTER)
                            || keys.contains(KeyCode::KEY_F1)
                            || keys.contains(KeyCode::KEY_ESC)
                    });
                    if is_keyboard && dev.set_nonblocking(true).is_ok() {
                        devices.push((path, dev));
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                    saw_permission_denied = true;
                }
                Err(_) => {}
            }
        }
    }

    (devices, saw_permission_denied)
}

fn run_evdev_loop(
    bindings: Arc<Mutex<HotkeyBindings>>,
    status: Arc<Mutex<HotkeyStatus>>,
    shutdown: Arc<AtomicBool>,
    tx: mpsc::Sender<HotkeyAction>,
    repaint_ctx: Option<egui::Context>,
) {
    let (mut devices, mut perm_denied) = open_keyboard_devices();
    update_status(&status, &devices, perm_denied);

    let mut held_keys: HashSet<KeyCode> = HashSet::new();
    let mut last_rescan = Instant::now();

    while !shutdown.load(Ordering::Relaxed) {
        if last_rescan.elapsed() >= Duration::from_secs(5) {
            last_rescan = Instant::now();
            if devices.is_empty() {
                let (new_devs, denied) = open_keyboard_devices();
                devices = new_devs;
                perm_denied = denied;
                update_status(&status, &devices, perm_denied);
            }
        }

        let mut had_error = false;
        for (_, dev) in &mut devices {
            match dev.fetch_events() {
                Ok(events) => {
                    for ev in events {
                        if let EventSummary::Key(_, code, value) = ev.destructure() {
                            if value == 1 {
                                held_keys.insert(code);
                                handle_key_press(
                                    code,
                                    &held_keys,
                                    &bindings,
                                    &tx,
                                    repaint_ctx.as_ref(),
                                );
                            } else if value == 0 {
                                held_keys.remove(&code);
                            }
                        }
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(_) => {
                    had_error = true;
                }
            }
        }

        if had_error {
            let (new_devs, denied) = open_keyboard_devices();
            devices = new_devs;
            perm_denied = denied;
            held_keys.clear();
            update_status(&status, &devices, perm_denied);
        }

        thread::sleep(Duration::from_millis(8));
    }
}

fn update_status(
    status: &Arc<Mutex<HotkeyStatus>>,
    devices: &[(PathBuf, evdev::Device)],
    perm_denied: bool,
) {
    if let Ok(mut g) = status.lock() {
        g.device_count = devices.len();
        g.permission_denied = devices.is_empty() && perm_denied;
        g.status_text = if g.permission_denied {
            "No permission to read /dev/input/event*".to_string()
        } else if devices.is_empty() {
            "No keyboard input devices detected".to_string()
        } else {
            format!("Listening on {} evdev input device(s)", devices.len())
        };
    }
}

fn is_modifier(code: KeyCode) -> bool {
    matches!(
        code,
        KeyCode::KEY_LEFTCTRL
            | KeyCode::KEY_RIGHTCTRL
            | KeyCode::KEY_LEFTALT
            | KeyCode::KEY_RIGHTALT
            | KeyCode::KEY_LEFTSHIFT
            | KeyCode::KEY_RIGHTSHIFT
            | KeyCode::KEY_LEFTMETA
            | KeyCode::KEY_RIGHTMETA
    )
}

fn current_modifiers(held: &HashSet<KeyCode>) -> (bool, bool, bool, bool) {
    let ctrl = held.contains(&KeyCode::KEY_LEFTCTRL) || held.contains(&KeyCode::KEY_RIGHTCTRL);
    let alt = held.contains(&KeyCode::KEY_LEFTALT) || held.contains(&KeyCode::KEY_RIGHTALT);
    let shift = held.contains(&KeyCode::KEY_LEFTSHIFT) || held.contains(&KeyCode::KEY_RIGHTSHIFT);
    let super_key = held.contains(&KeyCode::KEY_LEFTMETA) || held.contains(&KeyCode::KEY_RIGHTMETA);
    (ctrl, alt, shift, super_key)
}

fn handle_key_press(
    code: KeyCode,
    held: &HashSet<KeyCode>,
    bindings: &Arc<Mutex<HotkeyBindings>>,
    tx: &mpsc::Sender<HotkeyAction>,
    repaint_ctx: Option<&egui::Context>,
) {
    if is_modifier(code) {
        return;
    }
    let Some(key_name) = keycode_to_name(code) else {
        return;
    };
    let (ctrl, alt, shift, super_key) = current_modifiers(held);

    let Ok(g) = bindings.lock() else {
        return;
    };

    if g.capture_mode {
        let _ = tx.send(HotkeyAction::Captured(Hotkey::new(
            ctrl, alt, shift, super_key, key_name,
        )));
        if let Some(ctx) = repaint_ctx {
            ctx.request_repaint();
        }
        return;
    }

    if !g.enabled {
        return;
    }

    if let Some(ref stop_hk) = g.stop_hotkey {
        if stop_hk.matches(ctrl, alt, shift, super_key, &key_name) {
            let _ = tx.send(HotkeyAction::StopAll);
            if let Some(ctx) = repaint_ctx {
                ctx.request_repaint();
            }
            return;
        }
    }

    if let Some(ref pause_hk) = g.pause_hotkey {
        if pause_hk.matches(ctrl, alt, shift, super_key, &key_name) {
            let _ = tx.send(HotkeyAction::TogglePause);
            if let Some(ctx) = repaint_ctx {
                ctx.request_repaint();
            }
            return;
        }
    }

    if let Some(ref rec_hk) = g.record_hotkey {
        if rec_hk.matches(ctrl, alt, shift, super_key, &key_name) {
            let _ = tx.send(HotkeyAction::ToggleRecord);
            if let Some(ctx) = repaint_ctx {
                ctx.request_repaint();
            }
            return;
        }
    }

    for (sound_id, hk) in &g.sounds {
        if hk.matches(ctrl, alt, shift, super_key, &key_name) {
            let _ = tx.send(HotkeyAction::PlaySound(*sound_id));
            if let Some(ctx) = repaint_ctx {
                ctx.request_repaint();
            }
            break;
        }
    }
}

pub fn keycode_to_name(code: KeyCode) -> Option<String> {
    let s = match code {
        KeyCode::KEY_A => "A",
        KeyCode::KEY_B => "B",
        KeyCode::KEY_C => "C",
        KeyCode::KEY_D => "D",
        KeyCode::KEY_E => "E",
        KeyCode::KEY_F => "F",
        KeyCode::KEY_G => "G",
        KeyCode::KEY_H => "H",
        KeyCode::KEY_I => "I",
        KeyCode::KEY_J => "J",
        KeyCode::KEY_K => "K",
        KeyCode::KEY_L => "L",
        KeyCode::KEY_M => "M",
        KeyCode::KEY_N => "N",
        KeyCode::KEY_O => "O",
        KeyCode::KEY_P => "P",
        KeyCode::KEY_Q => "Q",
        KeyCode::KEY_R => "R",
        KeyCode::KEY_S => "S",
        KeyCode::KEY_T => "T",
        KeyCode::KEY_U => "U",
        KeyCode::KEY_V => "V",
        KeyCode::KEY_W => "W",
        KeyCode::KEY_X => "X",
        KeyCode::KEY_Y => "Y",
        KeyCode::KEY_Z => "Z",
        KeyCode::KEY_0 => "0",
        KeyCode::KEY_1 => "1",
        KeyCode::KEY_2 => "2",
        KeyCode::KEY_3 => "3",
        KeyCode::KEY_4 => "4",
        KeyCode::KEY_5 => "5",
        KeyCode::KEY_6 => "6",
        KeyCode::KEY_7 => "7",
        KeyCode::KEY_8 => "8",
        KeyCode::KEY_9 => "9",
        KeyCode::KEY_F1 => "F1",
        KeyCode::KEY_F2 => "F2",
        KeyCode::KEY_F3 => "F3",
        KeyCode::KEY_F4 => "F4",
        KeyCode::KEY_F5 => "F5",
        KeyCode::KEY_F6 => "F6",
        KeyCode::KEY_F7 => "F7",
        KeyCode::KEY_F8 => "F8",
        KeyCode::KEY_F9 => "F9",
        KeyCode::KEY_F10 => "F10",
        KeyCode::KEY_F11 => "F11",
        KeyCode::KEY_F12 => "F12",
        KeyCode::KEY_F13 => "F13",
        KeyCode::KEY_F14 => "F14",
        KeyCode::KEY_F15 => "F15",
        KeyCode::KEY_F16 => "F16",
        KeyCode::KEY_F17 => "F17",
        KeyCode::KEY_F18 => "F18",
        KeyCode::KEY_F19 => "F19",
        KeyCode::KEY_F20 => "F20",
        KeyCode::KEY_KP0 => "Num0",
        KeyCode::KEY_KP1 => "Num1",
        KeyCode::KEY_KP2 => "Num2",
        KeyCode::KEY_KP3 => "Num3",
        KeyCode::KEY_KP4 => "Num4",
        KeyCode::KEY_KP5 => "Num5",
        KeyCode::KEY_KP6 => "Num6",
        KeyCode::KEY_KP7 => "Num7",
        KeyCode::KEY_KP8 => "Num8",
        KeyCode::KEY_KP9 => "Num9",
        KeyCode::KEY_KPASTERISK => "Num*",
        KeyCode::KEY_KPPLUS => "Num+",
        KeyCode::KEY_KPMINUS => "Num-",
        KeyCode::KEY_KPDOT => "Num.",
        KeyCode::KEY_KPSLASH => "Num/",
        KeyCode::KEY_KPENTER => "NumEnter",
        KeyCode::KEY_SPACE => "Space",
        KeyCode::KEY_TAB => "Tab",
        KeyCode::KEY_ENTER => "Enter",
        KeyCode::KEY_ESC => "Escape",
        KeyCode::KEY_BACKSPACE => "Backspace",
        KeyCode::KEY_INSERT => "Insert",
        KeyCode::KEY_DELETE => "Delete",
        KeyCode::KEY_HOME => "Home",
        KeyCode::KEY_END => "End",
        KeyCode::KEY_PAGEUP => "PageUp",
        KeyCode::KEY_PAGEDOWN => "PageDown",
        KeyCode::KEY_UP => "Up",
        KeyCode::KEY_DOWN => "Down",
        KeyCode::KEY_LEFT => "Left",
        KeyCode::KEY_RIGHT => "Right",
        KeyCode::KEY_PAUSE => "Pause",
        KeyCode::KEY_SCROLLLOCK => "ScrollLock",
        KeyCode::KEY_MINUS => "-",
        KeyCode::KEY_EQUAL => "=",
        KeyCode::KEY_LEFTBRACE => "[",
        KeyCode::KEY_RIGHTBRACE => "]",
        KeyCode::KEY_SEMICOLON => ";",
        KeyCode::KEY_APOSTROPHE => "'",
        KeyCode::KEY_GRAVE => "`",
        KeyCode::KEY_BACKSLASH => "\\",
        KeyCode::KEY_COMMA => ",",
        KeyCode::KEY_DOT => ".",
        KeyCode::KEY_SLASH => "/",
        _ => return None,
    };
    Some(s.to_string())
}

pub fn egui_key_to_name(key: egui::Key) -> Option<String> {
    use egui::Key;
    let s = match key {
        Key::A => "A",
        Key::B => "B",
        Key::C => "C",
        Key::D => "D",
        Key::E => "E",
        Key::F => "F",
        Key::G => "G",
        Key::H => "H",
        Key::I => "I",
        Key::J => "J",
        Key::K => "K",
        Key::L => "L",
        Key::M => "M",
        Key::N => "N",
        Key::O => "O",
        Key::P => "P",
        Key::Q => "Q",
        Key::R => "R",
        Key::S => "S",
        Key::T => "T",
        Key::U => "U",
        Key::V => "V",
        Key::W => "W",
        Key::X => "X",
        Key::Y => "Y",
        Key::Z => "Z",
        Key::Num0 => "0",
        Key::Num1 => "1",
        Key::Num2 => "2",
        Key::Num3 => "3",
        Key::Num4 => "4",
        Key::Num5 => "5",
        Key::Num6 => "6",
        Key::Num7 => "7",
        Key::Num8 => "8",
        Key::Num9 => "9",
        Key::F1 => "F1",
        Key::F2 => "F2",
        Key::F3 => "F3",
        Key::F4 => "F4",
        Key::F5 => "F5",
        Key::F6 => "F6",
        Key::F7 => "F7",
        Key::F8 => "F8",
        Key::F9 => "F9",
        Key::F10 => "F10",
        Key::F11 => "F11",
        Key::F12 => "F12",
        Key::F13 => "F13",
        Key::F14 => "F14",
        Key::F15 => "F15",
        Key::F16 => "F16",
        Key::F17 => "F17",
        Key::F18 => "F18",
        Key::F19 => "F19",
        Key::F20 => "F20",
        Key::Space => "Space",
        Key::Tab => "Tab",
        Key::Enter => "Enter",
        Key::Escape => "Escape",
        Key::Backspace => "Backspace",
        Key::Insert => "Insert",
        Key::Delete => "Delete",
        Key::Home => "Home",
        Key::End => "End",
        Key::PageUp => "PageUp",
        Key::PageDown => "PageDown",
        Key::ArrowUp => "Up",
        Key::ArrowDown => "Down",
        Key::ArrowLeft => "Left",
        Key::ArrowRight => "Right",
        Key::Minus => "-",
        Key::Equals => "=",
        Key::OpenBracket => "[",
        Key::CloseBracket => "]",
        Key::Semicolon => ";",
        Key::Backslash => "\\",
        Key::Comma => ",",
        Key::Period => ".",
        Key::Slash => "/",
        _ => return None,
    };
    Some(s.to_string())
}
