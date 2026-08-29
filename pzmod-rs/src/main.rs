// pzmod — Project Zomboid workshop mod manager (Rust + Tauri 2).
// Port in progress: filesystem lane (list / enable / disable) is live here;
// workshop browse + steamcmd install still live in pzmod.py. See ROUTES.md.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Serialize;

const APPID: &str = "108600";

fn env_path(key: &str) -> Option<PathBuf> {
    std::env::var_os(key).map(PathBuf::from).filter(|p| !p.as_os_str().is_empty())
}

/// (game, user, mods, mods_off) — same env overrides as pzmod.py so both halves
/// of the port can be pointed at the throwaway tree in E:\pztest.
fn paths() -> (PathBuf, PathBuf, PathBuf, PathBuf) {
    let game = env_path("PZ_GAME").unwrap_or_else(|| PathBuf::from(r"D:\ProjectZomboid"));
    let user = env_path("PZ_USER").unwrap_or_else(|| {
        let home = env_path("USERPROFILE").unwrap_or_else(|| PathBuf::from(r"C:\"));
        home.join("Zomboid")
    });
    let mods = user.join("mods");
    let off = user.join("mods_off");
    (game, user, mods, off)
}

/// Trust boundary: `folder` arrives from the webview. Only a bare directory
/// name is ever allowed — no separators, no drive letters, no dot entries.
fn check_folder(folder: &str) -> Result<String, String> {
    let f = folder.trim();
    if f.is_empty() || f == "." || f == ".." {
        return Err(format!("Tên thư mục không hợp lệ: {folder:?}"));
    }
    if f.contains('/') || f.contains('\\') || f.contains(':') {
        return Err(format!("Tên thư mục không được chứa đường dẫn: {folder:?}"));
    }
    if Path::new(f).components().count() != 1 {
        return Err(format!("Tên thư mục không hợp lệ: {folder:?}"));
    }
    Ok(f.to_string())
}

fn is_dir(p: &Path) -> bool {
    // ponytail: mirrors installed_folders() in pzbisect.py, which tests isdir —
    // a stray FILE of the same name must not read as an installed mod.
    p.is_dir()
}

fn ensure_dirs(mods: &Path, off: &Path) -> Result<(), String> {
    for d in [mods, off] {
        fs::create_dir_all(d).map_err(|e| format!("Không tạo được {}: {e}", d.display()))?;
    }
    Ok(())
}

fn dir_names(p: &Path) -> Vec<String> {
    let mut out: Vec<String> = match fs::read_dir(p) {
        Ok(rd) => rd
            .filter_map(|e| e.ok())
            .filter(|e| e.path().is_dir())
            .filter_map(|e| e.file_name().into_string().ok())
            .collect(),
        Err(_) => Vec::new(),
    };
    out.sort_by_key(|s| s.to_lowercase());
    out
}

#[derive(Serialize)]
struct Folder {
    name: String,
    status: String,
    enabled: bool,
}

#[derive(Serialize)]
struct State {
    game: String,
    user: String,
    mods: String,
    off: String,
    appid: &'static str,
    installed: BTreeMap<String, serde_json::Value>,
    loose: Vec<Folder>,
    missing: Vec<String>,
    sorts: Vec<serde_json::Value>,
    tags: Vec<String>,
    bisect_ready: bool,
    /// Routes still served by pzmod.py; the UI greys those tabs out.
    ported: Vec<&'static str>,
}

#[tauri::command]
fn state() -> Result<State, String> {
    let (game, user, mods, off) = paths();
    ensure_dirs(&mods, &off)?;

    let mut folders: Vec<Folder> = Vec::new();
    let on = dir_names(&mods);
    let disabled = dir_names(&off);

    for name in &on {
        let status = if disabled.contains(name) { "collision" } else { "enabled" };
        folders.push(Folder { name: name.clone(), status: status.into(), enabled: true });
    }
    for name in &disabled {
        if on.contains(name) {
            continue; // already reported as a collision above
        }
        folders.push(Folder { name: name.clone(), status: "disabled".into(), enabled: false });
    }
    folders.sort_by_key(|f| f.name.to_lowercase());

    Ok(State {
        game: game.display().to_string(),
        user: user.display().to_string(),
        mods: mods.display().to_string(),
        off: off.display().to_string(),
        appid: APPID,
        // Workshop-owned entries need .pzmod.json + the Steam lane; not ported yet.
        installed: BTreeMap::new(),
        loose: folders,
        missing: Vec::new(),
        sorts: Vec::new(),
        tags: Vec::new(),
        bisect_ready: false,
        ported: vec!["state", "enable", "disable"],
    })
}

#[derive(Serialize)]
struct Done {
    ok: bool,
    log: String,
}

fn move_folder(from: &Path, to: &Path, folder: &str, verb: &str) -> Result<Done, String> {
    let src = from.join(folder);
    let dst = to.join(folder);

    if is_dir(&src) && is_dir(&dst) {
        return Err(format!(
            "'{folder}' có ở cả mods và mods_off — dọn tay một bên trước, pzmod không đoán bản nào đúng."
        ));
    }
    if is_dir(&dst) {
        return Ok(Done { ok: true, log: format!("'{folder}' đã {verb} sẵn.") });
    }
    if !is_dir(&src) {
        return Err(format!("Không tìm thấy thư mục mod '{folder}' trong mods/ hoặc mods_off/"));
    }
    fs::rename(&src, &dst).map_err(|e| format!("Không {verb} được '{folder}': {e}"))?;
    Ok(Done { ok: true, log: format!("Đã {verb} '{folder}'.") })
}

#[tauri::command]
fn enable(folder: String) -> Result<Done, String> {
    let folder = check_folder(&folder)?;
    let (_, _, mods, off) = paths();
    ensure_dirs(&mods, &off)?;
    move_folder(&off, &mods, &folder, "bật")
}

#[tauri::command]
fn disable(folder: String) -> Result<Done, String> {
    let folder = check_folder(&folder)?;
    let (_, _, mods, off) = paths();
    ensure_dirs(&mods, &off)?;
    move_folder(&mods, &off, &folder, "tắt")
}

fn main() {
    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![state, enable, disable])
        .run(tauri::generate_context!())
        .expect("pzmod: tauri failed to start");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folder_names_are_sandboxed() {
        assert!(check_folder("ZBPopFix").is_ok());
        assert!(check_folder("  ZBPopFix  ").is_ok());
        for bad in ["", ".", "..", "../evil", r"..\evil", r"C:\Windows", "a/b"] {
            assert!(check_folder(bad).is_err(), "phải chặn: {bad:?}");
        }
    }

    #[test]
    fn toggle_moves_folder_and_is_idempotent() {
        let base = std::env::temp_dir().join(format!("pzmod-test-{}", std::process::id()));
        let mods = base.join("mods");
        let off = base.join("mods_off");
        ensure_dirs(&mods, &off).unwrap();
        fs::create_dir_all(mods.join("ModA")).unwrap();

        move_folder(&mods, &off, "ModA", "tắt").unwrap();
        assert!(off.join("ModA").is_dir() && !mods.join("ModA").exists());

        // second call must not fail and must not create a second copy
        move_folder(&mods, &off, "ModA", "tắt").unwrap();
        assert!(!mods.join("ModA").exists());

        move_folder(&off, &mods, "ModA", "bật").unwrap();
        assert!(mods.join("ModA").is_dir() && !off.join("ModA").exists());

        // both sides present = refuse, never delete
        fs::create_dir_all(off.join("ModA")).unwrap();
        assert!(move_folder(&mods, &off, "ModA", "tắt").is_err());
        assert!(mods.join("ModA").is_dir() && off.join("ModA").is_dir());

        fs::remove_dir_all(&base).ok();
    }
}
