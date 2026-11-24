use crate::model::{AppConfig, Category, CategoryIcon, Hotkey, SoundEntry};
use quick_xml::events::{BytesDecl, BytesEnd, BytesStart, Event};
use quick_xml::{Reader, Writer};
use std::collections::HashMap;
use std::fs;
use std::io::Cursor;
use std::path::{Path, PathBuf};

pub fn config_path() -> PathBuf {
    let base = dirs::config_dir().unwrap_or_else(|| {
        dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".config")
    });
    base.join("rust-soundpad").join("config.json")
}

pub fn load_config() -> AppConfig {
    let path = config_path();
    if let Ok(content) = fs::read_to_string(&path) {
        if let Ok(mut cfg) = serde_json::from_str::<AppConfig>(&content) {
            cfg.ensure_valid();
            return cfg;
        }
    }
    let mut cfg = AppConfig::default();
    cfg.ensure_valid();
    cfg
}

pub fn save_config(cfg: &AppConfig) -> Result<(), String> {
    let path = config_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("Failed to create config dir: {e}"))?;
    }
    let json =
        serde_json::to_string_pretty(cfg).map_err(|e| format!("Failed to serialize config: {e}"))?;
    fs::write(&path, json).map_err(|e| format!("Failed to write config: {e}"))?;
    Ok(())
}

#[derive(Debug, Clone)]
struct RawSplSound {
    id: Option<String>,
    url: PathBuf,
    tag: String,
    duration_secs: Option<f32>,
    hotkey: Option<Hotkey>,
    play_count: u64,
}

fn parse_duration_attr(val: &str) -> Option<f32> {
    let trimmed = val.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Ok(ms) = trimmed.parse::<f32>() {
        // Soundpad stores duration either in seconds or milliseconds (heuristic: > 36000 likely ms if integer)
        return Some(if ms > 36000.0 { ms / 1000.0 } else { ms });
    }
    if let Some((m, s)) = trimmed.split_once(':') {
        if let (Ok(mins), Ok(secs)) = (m.trim().parse::<f32>(), s.trim().parse::<f32>()) {
            return Some(mins * 60.0 + secs);
        }
    }
    None
}

fn parse_sound_attrs(e: &BytesStart<'_>) -> Option<RawSplSound> {
    let mut id = None;
    let mut url = None;
    let mut title = None;
    let mut duration_secs = None;
    let mut hotkey = None;
    let mut play_count = 0u64;

    for attr in e.attributes().flatten() {
        let key = String::from_utf8_lossy(attr.key.as_ref()).to_ascii_lowercase();
        let val = attr
            .unescape_value()
            .map(|v| v.into_owned())
            .unwrap_or_default();
        match key.as_str() {
            "id" => id = Some(val),
            "url" | "path" | "file" | "filename" => {
                if !val.trim().is_empty() {
                    url = Some(PathBuf::from(val));
                }
            }
            "title" | "tag" | "name" => {
                if !val.trim().is_empty() {
                    title = Some(val);
                }
            }
            "duration" | "length" => duration_secs = parse_duration_attr(&val),
            "hotkey" => hotkey = Hotkey::parse_str(&val),
            "playcount" | "play_count" | "count" => {
                play_count = val.trim().parse::<u64>().unwrap_or(0);
            }
            _ => {}
        }
    }

    let url = url?;
    let tag = title.unwrap_or_else(|| {
        url.file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "Sound".to_string())
    });

    Some(RawSplSound {
        id,
        url,
        tag,
        duration_secs,
        hotkey,
        play_count,
    })
}

/// Imports Soundpad `.spl` XML playlists, supporting both nested `<Category><Sound .../></Category>`
/// and flat `<Soundlist><Sound id="..." url="..."/></Soundlist>` + `<Categories><Category ...><Sound id="..."/></Category></Categories>`.
pub fn import_spl(path: &Path, cfg: &mut AppConfig) -> Result<usize, String> {
    let xml = fs::read_to_string(path).map_err(|e| format!("Failed to read {}: {e}", path.display()))?;
    let mut reader = Reader::from_str(&xml);
    reader.config_mut().trim_text(true);

    let mut sounds_by_id: HashMap<String, RawSplSound> = HashMap::new();
    let mut root_sounds: Vec<RawSplSound> = Vec::new();
    let mut parsed_categories: Vec<(String, Vec<RawSplSound>, Vec<String>)> = Vec::new();
    let mut current_category: Option<(String, Vec<RawSplSound>, Vec<String>)> = None;

    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) => {
                let name = String::from_utf8_lossy(e.name().as_ref()).to_ascii_lowercase();
                match name.as_str() {
                    "category" => {
                        let mut cat_name = "Imported".to_string();
                        for attr in e.attributes().flatten() {
                            let k = String::from_utf8_lossy(attr.key.as_ref()).to_ascii_lowercase();
                            if k == "name" || k == "title" {
                                if let Ok(v) = attr.unescape_value() {
                                    if !v.trim().is_empty() {
                                        cat_name = v.into_owned();
                                    }
                                }
                            }
                        }
                        current_category = Some((cat_name, Vec::new(), Vec::new()));
                    }
                    "sound" => {
                        handle_sound_tag(
                            e,
                            &mut current_category,
                            &mut sounds_by_id,
                            &mut root_sounds,
                        );
                    }
                    _ => {}
                }
            }
            Ok(Event::Empty(ref e)) => {
                let name = String::from_utf8_lossy(e.name().as_ref()).to_ascii_lowercase();
                if name == "sound" {
                    handle_sound_tag(
                        e,
                        &mut current_category,
                        &mut sounds_by_id,
                        &mut root_sounds,
                    );
                } else if name == "category" {
                    let mut cat_name = "Imported".to_string();
                    for attr in e.attributes().flatten() {
                        let k = String::from_utf8_lossy(attr.key.as_ref()).to_ascii_lowercase();
                        if k == "name" || k == "title" {
                            if let Ok(v) = attr.unescape_value() {
                                if !v.trim().is_empty() {
                                    cat_name = v.into_owned();
                                }
                            }
                        }
                    }
                    parsed_categories.push((cat_name, Vec::new(), Vec::new()));
                }
            }
            Ok(Event::End(ref e)) => {
                let name = String::from_utf8_lossy(e.name().as_ref()).to_ascii_lowercase();
                if name == "category" {
                    if let Some(cat) = current_category.take() {
                        parsed_categories.push(cat);
                    }
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => return Err(format!("XML parse error: {e}")),
            _ => {}
        }
        buf.clear();
    }

    let mut imported_count = 0usize;

    if !parsed_categories.is_empty() {
        for (cat_name, inline_sounds, ref_ids) in parsed_categories {
            let mut entries = Vec::new();
            for s in inline_sounds {
                entries.push(to_sound_entry(cfg, s));
            }
            for rid in ref_ids {
                if let Some(s) = sounds_by_id.get(&rid).cloned() {
                    entries.push(to_sound_entry(cfg, s));
                }
            }
            imported_count += entries.len();
            if let Some(existing) = cfg.categories.iter_mut().find(|c| c.name == cat_name) {
                existing.sounds.extend(entries);
            } else {
                let id = cfg.alloc_id();
                cfg.categories.push(Category {
                    id,
                    name: cat_name,
                    icon: CategoryIcon::Folder,
                    sounds: entries,
                });
            }
        }
    } else if !root_sounds.is_empty() {
        let entries: Vec<SoundEntry> = root_sounds
            .into_iter()
            .map(|s| to_sound_entry(cfg, s))
            .collect();
        imported_count = entries.len();
        if let Some(cat) = cfg.selected_category_mut() {
            cat.sounds.extend(entries);
        }
    }

    Ok(imported_count)
}

fn handle_sound_tag(
    e: &BytesStart<'_>,
    current_category: &mut Option<(String, Vec<RawSplSound>, Vec<String>)>,
    sounds_by_id: &mut HashMap<String, RawSplSound>,
    root_sounds: &mut Vec<RawSplSound>,
) {
    if let Some(sound) = parse_sound_attrs(e) {
        if let Some(ref id) = sound.id {
            sounds_by_id.insert(id.clone(), sound.clone());
        }
        if let Some((_, list, _)) = current_category {
            list.push(sound);
        } else {
            root_sounds.push(sound);
        }
    } else if let Some((_, _, ref_ids)) = current_category {
        for attr in e.attributes().flatten() {
            let k = String::from_utf8_lossy(attr.key.as_ref()).to_ascii_lowercase();
            if k == "id" {
                if let Ok(v) = attr.unescape_value() {
                    ref_ids.push(v.into_owned());
                }
            }
        }
    }
}

fn to_sound_entry(cfg: &mut AppConfig, raw: RawSplSound) -> SoundEntry {
    SoundEntry {
        id: cfg.alloc_id(),
        path: raw.url,
        tag: raw.tag,
        duration_secs: raw.duration_secs,
        hotkey: raw.hotkey,
        play_count: raw.play_count,
        norm_gain: None,
    }
}

pub fn export_spl(path: &Path, cfg: &AppConfig) -> Result<(), String> {
    let mut writer = Writer::new_with_indent(Cursor::new(Vec::new()), b' ', 2);
    writer
        .write_event(Event::Decl(BytesDecl::new("1.0", Some("utf-8"), None)))
        .map_err(|e| e.to_string())?;

    writer
        .write_event(Event::Start(BytesStart::new("Soundlist")))
        .map_err(|e| e.to_string())?;
    writer
        .write_event(Event::Start(BytesStart::new("Categories")))
        .map_err(|e| e.to_string())?;

    for cat in &cfg.categories {
        let mut cat_el = BytesStart::new("Category");
        cat_el.push_attribute(("name", cat.name.as_str()));
        writer
            .write_event(Event::Start(cat_el))
            .map_err(|e| e.to_string())?;

        for sound in &cat.sounds {
            let mut s_el = BytesStart::new("Sound");
            let url_str = sound.path.to_string_lossy();
            s_el.push_attribute(("url", url_str.as_ref()));
            s_el.push_attribute(("title", sound.tag.as_str()));
            if let Some(dur) = sound.duration_secs {
                let dur_str = format!("{dur:.3}");
                s_el.push_attribute(("duration", dur_str.as_str()));
            }
            if let Some(ref hk) = sound.hotkey {
                let hk_str = hk.display();
                s_el.push_attribute(("hotkey", hk_str.as_str()));
            }
            let pc_str = sound.play_count.to_string();
            s_el.push_attribute(("playCount", pc_str.as_str()));
            writer
                .write_event(Event::Empty(s_el))
                .map_err(|e| e.to_string())?;
        }

        writer
            .write_event(Event::End(BytesEnd::new("Category")))
            .map_err(|e| e.to_string())?;
    }

    writer
        .write_event(Event::End(BytesEnd::new("Categories")))
        .map_err(|e| e.to_string())?;
    writer
        .write_event(Event::End(BytesEnd::new("Soundlist")))
        .map_err(|e| e.to_string())?;

    let bytes = writer.into_inner().into_inner();
    fs::write(path, bytes).map_err(|e| format!("Failed to write {}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spl_export_and_import_roundtrip() {
        let mut cfg = AppConfig::default();
        let sid = cfg.alloc_id();
        cfg.categories[0].sounds.push(SoundEntry {
            id: sid,
            path: PathBuf::from("/tmp/ba_dum_tss.wav"),
            tag: "ba dum tss".to_string(),
            duration_secs: Some(2.0),
            hotkey: Hotkey::parse_str("Ctrl+1"),
            play_count: 7,
            norm_gain: None,
        });

        let tmp = std::env::temp_dir().join("rust_soundpad_test_roundtrip.spl");
        export_spl(&tmp, &cfg).expect("export spl");

        let mut cfg2 = AppConfig::default();
        let count = import_spl(&tmp, &mut cfg2).expect("import spl");
        let _ = fs::remove_file(&tmp);

        assert_eq!(count, 1);
        assert_eq!(cfg2.categories[0].sounds.len(), 1);
        let imported = &cfg2.categories[0].sounds[0];
        assert_eq!(imported.tag, "ba dum tss");
        assert_eq!(imported.play_count, 7);
        assert_eq!(
            imported.hotkey.as_ref().map(|h| h.display()).as_deref(),
            Some("Ctrl+1")
        );
    }

    #[test]
    fn spl_import_flat_with_category_refs() {
        let xml = r#"<?xml version="1.0" encoding="utf-8"?>
<Soundlist>
  <Sound id="10" url="/tmp/meme.mp3" title="vine boom" playCount="3" hotkey="F9" />
  <Categories>
    <Category name="Memes">
      <Sound id="10" />
    </Category>
  </Categories>
</Soundlist>"#;
        let tmp = std::env::temp_dir().join("rust_soundpad_test_flat.spl");
        fs::write(&tmp, xml).unwrap();

        let mut cfg = AppConfig::default();
        let count = import_spl(&tmp, &mut cfg).expect("import flat spl");
        let _ = fs::remove_file(&tmp);

        assert_eq!(count, 1);
        let memes = cfg.categories.iter().find(|c| c.name == "Memes").unwrap();
        assert_eq!(memes.sounds.len(), 1);
        assert_eq!(memes.sounds[0].tag, "vine boom");
        assert_eq!(memes.sounds[0].play_count, 3);
    }

    #[test]
    fn config_backward_compatibility_and_new_fields() {
        let legacy_json = r#"{
            "speakers_volume": 0.75,
            "mic_volume": 0.9,
            "stop_all_hotkey": { "ctrl": true, "alt": false, "shift": false, "super_key": false, "key": "F10" }
        }"#;
        let mut cfg: AppConfig = serde_json::from_str(legacy_json).expect("parse legacy config");
        cfg.ensure_valid();
        assert!((cfg.master_volume - 1.0).abs() < 1e-6);
        assert_eq!(
            cfg.stop_hotkey.as_ref().map(|h| h.display()).as_deref(),
            Some("Ctrl+F10")
        );
        assert!(cfg.pause_hotkey.is_none());
        assert!(cfg.record_hotkey.is_none());
        assert_eq!(cfg.recording_source, crate::model::RecordingSource::SystemAudio);
        assert!(cfg.recording_dir.ends_with("rust-soundpad/recordings"));

        cfg.master_volume = 1.25;
        cfg.pause_hotkey = Hotkey::parse_str("Ctrl+P");
        cfg.record_hotkey = Hotkey::parse_str("Ctrl+R");
        cfg.recording_source = crate::model::RecordingSource::Both;
        let serialized = serde_json::to_string(&cfg).expect("serialize new config");
        let parsed: AppConfig = serde_json::from_str(&serialized).expect("reparse new config");
        assert!((parsed.master_volume - 1.25).abs() < 1e-6);
        assert_eq!(
            parsed.pause_hotkey.as_ref().map(|h| h.display()).as_deref(),
            Some("Ctrl+P")
        );
        assert_eq!(
            parsed.record_hotkey.as_ref().map(|h| h.display()).as_deref(),
            Some("Ctrl+R")
        );
        assert_eq!(parsed.recording_source, crate::model::RecordingSource::Both);
    }
}
