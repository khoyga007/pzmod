// pzmod — Project Zomboid workshop mod manager (Rust + Tauri 2).
// All routes used by ui.html are native here; pzmod.py remains the CLI twin.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::collections::{hash_map::DefaultHasher, BTreeMap, HashMap, HashSet};
use std::ffi::OsString;
use std::fmt;
use std::fs;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::Serialize;
use serde_json::{json, Map, Value};

const APPID: &str = "108600";
const DETAILS_URL: &str =
    "https://api.steampowered.com/ISteamRemoteStorage/GetPublishedFileDetails/v1/";
const COLLECTION_URL: &str =
    "https://api.steampowered.com/ISteamRemoteStorage/GetCollectionDetails/v1/";
const BROWSE_URL: &str = "https://steamcommunity.com/workshop/browse/?";
const ITEM_URL: &str = "https://steamcommunity.com/sharedfiles/filedetails/?id=";
const UA: &str = "Mozilla/5.0 (pzmod)";
const BROWSE_TTL: Duration = Duration::from_secs(30 * 60);
const REQUIRES_TTL: Duration = Duration::from_secs(7 * 24 * 60 * 60);
const BROWSE_GAP: Duration = Duration::from_secs(2);
const RATE_LIMITED: &str =
    "Steam chặn vì hỏi quá nhiều - chờ 10-30 phút. IP Warp là IP dùng chung nên dễ dính hơn IP nhà";

const SORTS: &[&str] = &[
    "trend",
    "totaluniquesubscriptions",
    "toprated",
    "num_parent_items",
    "mostrecent",
    "textsearch",
];
const SORT_LABELS: &[(&str, &str)] = &[
    ("trend", "Thịnh hành tuần"),
    ("totaluniquesubscriptions", "Nhiều sub nhất"),
    ("toprated", "Đánh giá cao nhất"),
    ("num_parent_items", "Được cần nhiều nhất"),
    ("mostrecent", "Mới nhất"),
    ("textsearch", "Khớp từ khoá"),
];
const STEAMCMD_CANDIDATES: &[&str] = &[
    r"C:\WorkshopDL\steamcmd\steamcmd.exe",
    r"E:\steamcmd\steamcmd.exe",
    r"C:\steamcmd\steamcmd.exe",
];
const TAGS: &[&str] = &[
    "Build 40",
    "Build 41",
    "Build 42",
    "Animals",
    "Audio",
    "Balance",
    "Building",
    "Clothing/Armor",
    "Farming",
    "Food",
    "Framework",
    "Hardmode",
    "Interface",
    "Items",
    "Language/Translation",
    "Literature",
    "Map",
    "Military",
    "Misc",
    "Models",
    "Multiplayer",
    "Pop Culture",
    "Realistic",
    "Silly/Fun",
    "Skills",
    "Textures",
    "Traits",
    "Vehicles",
    "QoL",
    "WIP",
    "Weapons",
];

#[derive(Debug)]
struct Blocked(String);

impl fmt::Display for Blocked {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

fn env_path(key: &str) -> Option<PathBuf> {
    std::env::var_os(key)
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
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

fn work_guard() -> Result<std::sync::MutexGuard<'static, ()>, String> {
    static WORK: OnceLock<Mutex<()>> = OnceLock::new();
    WORK.get_or_init(|| Mutex::new(()))
        .lock()
        .map_err(|_| "Khoá thao tác mod bị lỗi".to_string())
}

fn http_agent() -> &'static ureq::Agent {
    static AGENT: OnceLock<ureq::Agent> = OnceLock::new();
    AGENT.get_or_init(|| {
        ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(60)))
            .build()
            .into()
    })
}

fn url_encode(value: &str) -> String {
    let mut out = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

fn form_body(fields: &[(String, String)]) -> String {
    fields
        .iter()
        .map(|(key, value)| format!("{}={}", url_encode(key), url_encode(value)))
        .collect::<Vec<_>>()
        .join("&")
}

fn post_json(url: &str, fields: Vec<(String, String)>) -> Result<Value, String> {
    let body = form_body(&fields);
    let text = http_agent()
        .post(url)
        .header("User-Agent", UA)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .send(body)
        .map_err(|e| format!("Steam API lỗi: {e}"))?
        .body_mut()
        .read_to_string()
        .map_err(|e| format!("Không đọc được Steam API: {e}"))?;
    serde_json::from_str(&text).map_err(|e| format!("Steam API trả JSON lỗi: {e}"))
}

fn details(ids: &[String]) -> Result<HashMap<String, Value>, String> {
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    let mut fields = vec![("itemcount".into(), ids.len().to_string())];
    fields.extend(
        ids.iter()
            .enumerate()
            .map(|(index, id)| (format!("publishedfileids[{index}]"), id.clone())),
    );
    let value = post_json(DETAILS_URL, fields)?;
    let mut out = HashMap::new();
    if let Some(items) = value
        .get("response")
        .and_then(|v| v.get("publishedfiledetails"))
        .and_then(Value::as_array)
    {
        for item in items {
            if value_u64(item, "result") == 1 {
                let id = value_string(item, "publishedfileid");
                if !id.is_empty() {
                    out.insert(id, item.clone());
                }
            }
        }
    }
    Ok(out)
}

fn collection_children(id: &str) -> Result<Vec<String>, String> {
    let value = post_json(
        COLLECTION_URL,
        vec![
            ("collectioncount".into(), "1".into()),
            ("publishedfileids[0]".into(), id.into()),
        ],
    )?;
    let Some(collections) = value
        .get("response")
        .and_then(|v| v.get("collectiondetails"))
        .and_then(Value::as_array)
    else {
        return Ok(Vec::new());
    };
    for collection in collections {
        if value_u64(collection, "result") != 1 {
            continue;
        }
        return Ok(collection
            .get("children")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .map(|child| value_string(child, "publishedfileid"))
            .filter(|child| !child.is_empty())
            .collect());
    }
    Ok(Vec::new())
}

fn cache_path(url: &str) -> PathBuf {
    let mut hasher = DefaultHasher::new();
    url.hash(&mut hasher);
    paths()
        .1
        .join(".pzmod-cache")
        .join(format!("{:016x}.html", hasher.finish()))
}

fn cached(url: &str, ttl: Option<Duration>) -> Option<String> {
    let path = cache_path(url);
    if let Some(ttl) = ttl {
        let fresh = fs::metadata(&path)
            .and_then(|meta| meta.modified())
            .ok()
            .and_then(|modified| modified.elapsed().ok())
            .is_some_and(|age| age < ttl);
        if !fresh {
            return None;
        }
    }
    let html = fs::read_to_string(path).ok()?;
    html.contains("filedetails/?id=").then_some(html)
}

fn write_cache(url: &str, html: &str) -> Result<(), Blocked> {
    let path = cache_path(url);
    fs::create_dir_all(path.parent().expect("cache file has a parent"))
        .and_then(|_| fs::write(&path, html))
        .map_err(|e| {
            Blocked(format!(
                "Không ghi được cache workshop {}: {e}",
                path.display()
            ))
        })
}

fn community_error(error: ureq::Error) -> String {
    match error {
        ureq::Error::StatusCode(429) => RATE_LIMITED.into(),
        ureq::Error::StatusCode(code) => format!("steamcommunity.com trả HTTP {code}"),
        other => format!("không nối được steamcommunity.com ({other}) - Warp/VPN đã bật chưa?"),
    }
}

fn cached_page(url: &str, ttl: Duration) -> Result<String, Blocked> {
    if let Some(html) = cached(url, Some(ttl)) {
        return Ok(html);
    }

    static LAST_HIT: OnceLock<Mutex<Option<Instant>>> = OnceLock::new();
    let mut last_hit = LAST_HIT
        .get_or_init(|| Mutex::new(None))
        .lock()
        .map_err(|_| Blocked("Khoá nhịp workshop bị lỗi".into()))?;
    if let Some(html) = cached(url, Some(ttl)) {
        return Ok(html);
    }
    if let Some(last) = *last_hit {
        if let Some(wait) = BROWSE_GAP.checked_sub(last.elapsed()) {
            thread::sleep(wait);
        }
    }
    *last_hit = Some(Instant::now());

    let fetched = http_agent()
        .get(url)
        .header("User-Agent", UA)
        .call()
        .map_err(community_error)
        .and_then(|mut response| {
            response
                .body_mut()
                .read_to_string()
                .map_err(|e| e.to_string())
        })
        .and_then(|html| {
            if html.to_lowercase().contains("too many requests") {
                Err(RATE_LIMITED.into())
            } else if !html.contains("filedetails/?id=") {
                Err("Steam không trả trang workshop hợp lệ".into())
            } else {
                Ok(html)
            }
        });

    match fetched {
        Ok(html) => {
            write_cache(url, &html)?;
            Ok(html)
        }
        Err(why) => cached(url, None).ok_or(Blocked(why)),
    }
}

fn ids_after(html: &str, marker: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    let mut rest = html;
    while let Some(index) = rest.find(marker) {
        rest = &rest[index + marker.len()..];
        let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
        if !digits.is_empty() && seen.insert(digits.clone()) {
            out.push(digits);
        }
    }
    out
}

fn extract_listing_ids(html: &str) -> Vec<String> {
    ids_after(html, "sharedfiles/filedetails/?id=")
}

fn required_ids(id: &str) -> Result<Vec<String>, Blocked> {
    let html = cached_page(&format!("{ITEM_URL}{id}"), REQUIRES_TTL)?;
    let Some(start) = html.find("id=\"RequiredItems\"") else {
        return Ok(Vec::new());
    };
    let rest = &html[start..];
    let end = [rest.find("<!--"), rest.find("rightSectionTopTitle")]
        .into_iter()
        .flatten()
        .min()
        .unwrap_or(rest.len());
    Ok(ids_after(&rest[..end], "filedetails/?id=")
        .into_iter()
        .filter(|dependency| dependency != id)
        .take(20)
        .collect())
}

fn maybe_id(value: &str) -> Option<String> {
    let value = value.trim();
    if !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Some(value.into());
    }
    value
        .split(['?', '&'])
        .find_map(|part| part.strip_prefix("id="))
        .filter(|id| !id.is_empty() && id.bytes().all(|byte| byte.is_ascii_digit()))
        .map(str::to_string)
}

fn value_string(value: &Value, key: &str) -> String {
    value
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

fn value_u64(value: &Value, key: &str) -> u64 {
    value
        .get(key)
        .and_then(|field| field.as_u64().or_else(|| field.as_str()?.parse().ok()))
        .unwrap_or(0)
}

fn strip_bb(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find('[') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        if let Some(end) = after.find(']').filter(|end| *end <= 200) {
            rest = &after[end + 1..];
        } else {
            out.push('[');
            rest = after;
        }
    }
    out.push_str(rest);
    out
}

fn card(detail: &Value) -> Value {
    let description = strip_bb(&value_string(detail, "description")).replace('\r', "");
    let summary: String = description.trim().chars().take(220).collect();
    let tags: Vec<String> = detail
        .get("tags")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|tag| value_string(tag, "tag"))
        .filter(|tag| !tag.is_empty())
        .collect();
    json!({
        "id": value_string(detail, "publishedfileid"),
        "title": value_string(detail, "title"),
        "preview": value_string(detail, "preview_url"),
        "subs": value_u64(detail, "subscriptions"),
        "size": value_u64(detail, "file_size"),
        "updated": value_u64(detail, "time_updated"),
        "tags": tags,
        "summary": summary,
    })
}

fn managed_state(user: &Path) -> Map<String, Value> {
    fs::read_to_string(user.join(".pzmod.json"))
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .and_then(|value| value.as_object().cloned())
        .unwrap_or_default()
}

fn unique_suffix() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
}

fn transaction_dir(user: &Path, label: &str) -> Result<PathBuf, String> {
    fs::create_dir_all(user).map_err(|e| format!("Không tạo được {}: {e}", user.display()))?;
    for attempt in 0..20 {
        let path = user.join(format!(
            ".pzmod-{label}-{}-{}-{attempt}",
            std::process::id(),
            unique_suffix()
        ));
        match fs::create_dir(&path) {
            Ok(()) => return Ok(path),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(format!("Không tạo được transaction: {error}")),
        }
    }
    Err("Không tạo được tên transaction duy nhất".into())
}

fn write_managed_state(user: &Path, state: &Map<String, Value>) -> Result<(), String> {
    fs::create_dir_all(user).map_err(|e| format!("Không tạo được {}: {e}", user.display()))?;
    let path = user.join(".pzmod.json");
    let suffix = format!("{}-{}", std::process::id(), unique_suffix());
    let temporary = user.join(format!(".pzmod.json.tmp-{suffix}"));
    let backup = user.join(format!(".pzmod.json.bak-{suffix}"));
    let body = serde_json::to_string_pretty(state)
        .map_err(|e| format!("Không serialize được .pzmod.json: {e}"))?;
    fs::write(&temporary, body).map_err(|e| format!("Không ghi được state tạm: {e}"))?;

    let had_state = path.is_file();
    if had_state {
        if let Err(error) = fs::rename(&path, &backup) {
            fs::remove_file(&temporary).ok();
            return Err(format!("Không backup được .pzmod.json: {error}"));
        }
    }
    if let Err(error) = fs::rename(&temporary, &path) {
        if had_state {
            fs::rename(&backup, &path).map_err(|rollback| {
                format!(
                    "Không lưu được state ({error}) và không khôi phục được {} ({rollback})",
                    backup.display()
                )
            })?;
        }
        return Err(format!("Không lưu được .pzmod.json: {error}"));
    }
    if had_state {
        fs::remove_file(backup).ok();
    }
    Ok(())
}

fn entry_folders(entry: &Value) -> Vec<String> {
    entry
        .get("folders")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_string)
        .collect()
}

fn entry_complete(entry: &Value, mods: &Path, off: &Path) -> bool {
    let folders = entry_folders(entry);
    !folders.is_empty()
        && folders
            .iter()
            .all(|folder| matches!(folder_status_at(mods, off, folder), "enabled" | "disabled"))
}

fn normalize_required(value: &str) -> String {
    value
        .trim()
        .trim_start_matches(['\\', '/'])
        .trim()
        .to_string()
}

fn missing_modids(state: &Map<String, Value>, mods: &Path, off: &Path) -> Vec<String> {
    let mut supplied = HashSet::new();
    for entry in state.values() {
        if entry_folders(entry)
            .iter()
            .any(|folder| matches!(folder_status_at(mods, off, folder), "enabled" | "disabled"))
        {
            if let Some(modids) = entry.get("modids").and_then(Value::as_array) {
                supplied.extend(
                    modids
                        .iter()
                        .filter_map(Value::as_str)
                        .map(|value| value.to_lowercase()),
                );
            }
        }
    }
    let mut required = BTreeMap::new();
    for value in state
        .values()
        .filter_map(|entry| entry.get("require").and_then(Value::as_array))
        .flatten()
        .filter_map(Value::as_str)
    {
        let value = normalize_required(value);
        if !value.is_empty() {
            required.entry(value.to_lowercase()).or_insert(value);
        }
    }
    required
        .into_iter()
        .filter_map(|(key, value)| (!supplied.contains(&key)).then_some(value))
        .collect()
}

fn copy_tree(source: &Path, destination: &Path) -> Result<(), String> {
    fs::create_dir(destination)
        .map_err(|e| format!("Không tạo được {}: {e}", destination.display()))?;
    let mut entries: Vec<_> = fs::read_dir(source)
        .map_err(|e| format!("Không đọc được {}: {e}", source.display()))?
        .collect::<Result<_, _>>()
        .map_err(|e| format!("Không đọc được {}: {e}", source.display()))?;
    entries.sort_by_key(|entry| entry.file_name().to_string_lossy().to_lowercase());
    for entry in entries {
        let from = entry.path();
        let to = destination.join(entry.file_name());
        let kind = entry
            .file_type()
            .map_err(|e| format!("Không đọc được loại file {}: {e}", from.display()))?;
        if kind.is_dir() {
            copy_tree(&from, &to)?;
        } else if kind.is_file() {
            fs::copy(&from, &to).map_err(|e| format!("Không copy được {}: {e}", from.display()))?;
        } else {
            return Err(format!(
                "Không hỗ trợ symlink trong mod: {}",
                from.display()
            ));
        }
    }
    Ok(())
}

fn rollback_files(installed: &[PathBuf], moved: &[(PathBuf, PathBuf)]) -> Result<(), String> {
    for path in installed.iter().rev() {
        if path.is_dir() {
            fs::remove_dir_all(path)
                .map_err(|e| format!("Không dọn được {}: {e}", path.display()))?;
        } else if path.exists() {
            return Err(format!(
                "Đích rollback không còn là thư mục: {}",
                path.display()
            ));
        }
    }
    for (backup, original) in moved.iter().rev() {
        if backup.is_dir() {
            if original.exists() {
                return Err(format!("Đích rollback đã tồn tại: {}", original.display()));
            }
            fs::create_dir_all(original.parent().expect("folder has a parent"))
                .map_err(|e| format!("Không tạo được thư mục rollback: {e}"))?;
            fs::rename(backup, original)
                .map_err(|e| format!("Không khôi phục được {}: {e}", original.display()))?;
        }
    }
    Ok(())
}

fn steamcmd_exe() -> Result<PathBuf, String> {
    for candidate in STEAMCMD_CANDIDATES {
        let path = PathBuf::from(candidate);
        if path.is_file() {
            return Ok(path);
        }
    }
    if let Some(path) = std::env::var_os("PATH") {
        for directory in std::env::split_paths(&path) {
            for name in ["steamcmd.exe", "steamcmd"] {
                let candidate = directory.join(name);
                if candidate.is_file() {
                    return Ok(candidate);
                }
            }
        }
    }
    Err("Không tìm thấy steamcmd.exe".into())
}

fn steamcmd_args(id: &str) -> Vec<OsString> {
    [
        "+login",
        "anonymous",
        "+workshop_download_item",
        APPID,
        id,
        "+quit",
    ]
    .into_iter()
    .map(OsString::from)
    .collect()
}

fn download_item(id: &str, force: bool) -> Result<PathBuf, String> {
    if id.is_empty() || !id.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err("Workshop id không hợp lệ".into());
    }
    let executable = steamcmd_exe()?;
    let workshop = executable
        .parent()
        .ok_or("steamcmd.exe không có thư mục cha")?
        .join("steamapps")
        .join("workshop");
    let destination = workshop.join("content").join(APPID).join(id);
    if force {
        if destination.is_dir() {
            fs::remove_dir_all(&destination)
                .map_err(|e| format!("Không dọn được cache {}: {e}", destination.display()))?;
        }
        let manifest = workshop.join(format!("appworkshop_{APPID}.acf"));
        if manifest.is_file() {
            fs::remove_file(&manifest)
                .map_err(|e| format!("Không dọn được {}: {e}", manifest.display()))?;
        }
    }
    let output = Command::new(&executable)
        .args(steamcmd_args(id))
        .output()
        .map_err(|e| format!("Không chạy được steamcmd: {e}"))?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    if !stdout.contains("Success. Downloaded item") && !destination.is_dir() {
        let tail = stdout
            .lines()
            .rev()
            .take(4)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<Vec<_>>()
            .join(" | ");
        return Err(format!("steamcmd thất bại với {id}: {tail}"));
    }
    if !destination.is_dir() {
        return Err(format!(
            "steamcmd báo thành công nhưng thiếu {}",
            destination.display()
        ));
    }
    Ok(destination)
}

#[derive(Debug)]
struct ModRoot {
    folder: String,
    root: PathBuf,
    infos: Vec<PathBuf>,
}

fn collect_modinfo(root: &Path, out: &mut Vec<PathBuf>) -> Result<(), String> {
    let mut entries: Vec<_> = fs::read_dir(root)
        .map_err(|e| format!("Không đọc được {}: {e}", root.display()))?
        .collect::<Result<_, _>>()
        .map_err(|e| format!("Không đọc được {}: {e}", root.display()))?;
    entries.sort_by_key(|entry| entry.file_name().to_string_lossy().to_lowercase());
    let mut directories = Vec::new();
    for entry in entries {
        if entry.path().is_dir() {
            directories.push(entry.path());
        } else if entry.file_name() == "mod.info" {
            out.push(entry.path());
        }
    }
    for directory in directories {
        collect_modinfo(&directory, out)?;
    }
    Ok(())
}

fn mod_roots(item: &Path) -> Result<Vec<ModRoot>, String> {
    let mods = item.join("mods");
    if !mods.is_dir() {
        return Ok(Vec::new());
    }
    let mut entries: Vec<_> = fs::read_dir(&mods)
        .map_err(|e| format!("Không đọc được {}: {e}", mods.display()))?
        .collect::<Result<_, _>>()
        .map_err(|e| format!("Không đọc được {}: {e}", mods.display()))?;
    entries.sort_by_key(|entry| entry.file_name().to_string_lossy().to_lowercase());
    let mut roots = Vec::new();
    for entry in entries.into_iter().filter(|entry| entry.path().is_dir()) {
        let folder = entry
            .file_name()
            .into_string()
            .map_err(|_| "Tên thư mục mod không phải Unicode hợp lệ".to_string())?;
        check_folder(&folder)?;
        let mut infos = Vec::new();
        collect_modinfo(&entry.path(), &mut infos)?;
        if !infos.is_empty() {
            roots.push(ModRoot {
                folder,
                root: entry.path(),
                infos,
            });
        }
    }
    Ok(roots)
}

fn parse_modinfo(path: &Path) -> Result<HashMap<String, String>, String> {
    let bytes = fs::read(path).map_err(|e| format!("Không đọc được {}: {e}", path.display()))?;
    let text = String::from_utf8_lossy(&bytes);
    let mut out = HashMap::new();
    for line in text.lines() {
        let line = line.trim().trim_start_matches('\u{feff}');
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((key, value)) = line.split_once('=') {
            let key = key.trim();
            if !key.is_empty() {
                out.insert(key.to_string(), value.trim().to_string());
            }
        }
    }
    Ok(out)
}

fn mod_metadata(roots: &[ModRoot]) -> Result<(Vec<String>, Vec<String>), String> {
    let mut modids = Vec::new();
    let mut required = Vec::new();
    let mut modid_keys = HashSet::new();
    let mut required_keys = HashSet::new();
    for info in roots.iter().flat_map(|root| &root.infos) {
        let values = parse_modinfo(info)?;
        if let Some(modid) = values.get("id") {
            if !modid.is_empty() && modid_keys.insert(modid.to_lowercase()) {
                modids.push(modid.clone());
            }
        }
        for dependency in values
            .get("require")
            .into_iter()
            .flat_map(|value| value.split(','))
        {
            let dependency = normalize_required(dependency);
            if !dependency.is_empty() && required_keys.insert(dependency.to_lowercase()) {
                required.push(dependency);
            }
        }
    }
    Ok((modids, required))
}

fn folder_status_at(mods: &Path, off: &Path, folder: &str) -> &'static str {
    if check_folder(folder).is_err() {
        return "invalid";
    }
    match (mods.join(folder).is_dir(), off.join(folder).is_dir()) {
        (true, true) => "collision",
        (true, false) => "enabled",
        (false, true) => "disabled",
        (false, false) => "missing",
    }
}

/// Steam drops `browsesort` on the floor unless a time window comes with it:
/// ask for "most subscribed" with no `days` and it hands back the trending list,
/// which is why every filter looked dead. `mostrecent` is the one sort that
/// takes no window.
/// `toprated` is deliberately absent: Steam ranks it by stars and ignores
/// `days` entirely — the same list comes back for 7 as for 3650.
fn sort_days(browse_sort: &str) -> Option<&'static str> {
    match browse_sort {
        "trend" => Some("7"),
        "totaluniquesubscriptions" => Some("3650"),
        _ => None,
    }
}

/// The window picker, same choices the Workshop offers. "Tất cả" is 3650 days,
/// not -1: Steam reads -1 as a single day and answers with brand-new mods that
/// have a few dozen subscribers.
const PERIODS: &[(&str, &str)] = &[
    ("7", "1 tuần"),
    ("30", "1 tháng"),
    ("90", "3 tháng"),
    ("180", "6 tháng"),
    ("365", "1 năm"),
    ("3650", "Tất cả"),
];

/// Steam pins its own "Modding Policy" notice to the top of every listing, tag
/// filter or not, so the tags are checked again against the metadata.
fn has_tags(item: &Value, wanted: &[String]) -> bool {
    if wanted.is_empty() {
        return true;
    }
    let on_item: HashSet<&str> = item
        .get("tags")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|tag| tag.get("tag").and_then(Value::as_str))
        .collect();
    wanted.iter().all(|tag| on_item.contains(tag.as_str()))
}

#[tauri::command]
fn browse(
    q: String,
    sort: String,
    page: u32,
    tags: Vec<String>,
    days: Option<String>,
) -> Result<Value, String> {
    let page = page.clamp(1, 50);
    let wanted: Vec<String> = tags
        .iter()
        .filter(|tag| TAGS.contains(&tag.as_str()))
        .cloned()
        .collect();
    let ids = if let Some(id) = maybe_id(&q) {
        if details(std::slice::from_ref(&id))?.contains_key(&id) {
            vec![id]
        } else {
            Vec::new()
        }
    } else {
        let sort = if SORTS.contains(&sort.as_str()) {
            sort.as_str()
        } else {
            "trend"
        };
        let browse_sort = if q.is_empty() { sort } else { "textsearch" };
        let mut fields = vec![
            ("appid".into(), APPID.into()),
            ("section".into(), "readytouseitems".into()),
            ("browsesort".into(), browse_sort.into()),
            ("p".into(), page.to_string()),
        ];
        if !q.is_empty() {
            fields.push(("searchtext".into(), q));
        }
        // "Được cần nhiều nhất" is not just another browsesort: without
        // special_filter=6 Steam quietly answers with the default listing. The
        // 6 comes from the onViewAll handler in the workshop home JS bundle.
        if browse_sort == "num_parent_items" {
            fields.push(("special_filter".into(), "6".into()));
        }
        if let Some(fallback) = sort_days(browse_sort) {
            let window = days
                .as_deref()
                .filter(|asked| PERIODS.iter().any(|(value, _)| value == asked))
                .unwrap_or(fallback);
            fields.push(("days".into(), window.into()));
        }
        fields.extend(
            wanted
                .iter()
                .map(|tag| ("requiredtags[]".into(), tag.clone())),
        );
        let html = cached_page(&(BROWSE_URL.to_string() + &form_body(&fields)), BROWSE_TTL)
            .map_err(|error| error.to_string())?;
        extract_listing_ids(&html)
    };

    let metadata = details(&ids)?;
    let items: Vec<Value> = ids
        .iter()
        .filter_map(|id| metadata.get(id))
        .filter(|item| has_tags(item, &wanted))
        .map(card)
        .collect();
    Ok(json!({"items": items, "page": page}))
}

#[tauri::command]
fn detail(id: String) -> Result<Value, String> {
    if id.is_empty() || !id.bytes().all(|byte| byte.is_ascii_digit()) {
        return Ok(json!({"error": "bad id"}));
    }
    let Some(workshop) = details(std::slice::from_ref(&id))?.remove(&id) else {
        return Ok(json!({"error": "not found"}));
    };
    let mut out = card(&workshop);
    let object = out.as_object_mut().expect("card is an object");
    object.insert(
        "description".into(),
        Value::String(strip_bb(&value_string(&workshop, "description")).replace('\r', "")),
    );
    object.insert("children".into(), json!(collection_children(&id)?.len()));
    object.insert(
        "created".into(),
        json!(value_u64(&workshop, "time_created")),
    );
    object.insert("views".into(), json!(value_u64(&workshop, "views")));
    object.insert("favorited".into(), json!(value_u64(&workshop, "favorited")));

    let (_, user, mods, off) = paths();
    let state = managed_state(&user);
    let entry = state.get(&id).and_then(Value::as_object);
    let folders: Vec<Value> = entry
        .and_then(|item| item.get("folders"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(|folder| json!({"name": folder, "status": folder_status_at(&mods, &off, folder)}))
        .collect();
    let modids = entry
        .and_then(|item| item.get("modids"))
        .cloned()
        .unwrap_or_else(|| json!([]));
    object.insert("folders".into(), Value::Array(folders));
    object.insert("modids".into(), modids);

    match required_ids(&id) {
        Ok(required) => {
            let metadata = details(&required)?;
            let items: Vec<Value> = required
                .iter()
                .map(|required_id| {
                    json!({
                        "id": required_id,
                        "title": metadata.get(required_id)
                            .map(|item| value_string(item, "title"))
                            .filter(|title| !title.is_empty())
                            .unwrap_or_else(|| required_id.clone()),
                        "installed": state.contains_key(required_id),
                    })
                })
                .collect();
            object.insert("required".into(), Value::Array(items));
        }
        Err(_) => {
            object.insert("required".into(), json!([]));
            object.insert("req_blocked".into(), Value::Bool(true));
        }
    }
    Ok(out)
}

/// Trust boundary: `folder` arrives from the webview. Only a bare directory
/// name is ever allowed — no separators, no drive letters, no dot entries.
fn check_folder(folder: &str) -> Result<String, String> {
    let f = folder.trim();
    if f.is_empty() || f == "." || f == ".." {
        return Err(format!("Tên thư mục không hợp lệ: {folder:?}"));
    }
    // The lookalikes are not separators to Windows, so they cannot escape the
    // directory — they are refused here only so a bad name fails with a clear
    // message instead of an opaque fs::rename error two calls later.
    if f.contains([
        '/', '\\', ':', '\0', '\u{2215}', '\u{FF0F}', '\u{FF3C}', '\u{FF1A}',
    ]) {
        return Err(format!("Tên thư mục không được chứa đường dẫn: {folder:?}"));
    }
    // CON, NUL, COM1… are devices, not files. Creating one succeeds and then
    // behaves like a device; refuse before the rename touches it.
    let stem = f.split('.').next().unwrap_or(f).to_ascii_uppercase();
    let reserved = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || matches!(stem.strip_prefix("COM").or_else(|| stem.strip_prefix("LPT")),
                    Some(n) if matches!(n, "1"|"2"|"3"|"4"|"5"|"6"|"7"|"8"|"9"));
    if reserved {
        return Err(format!(
            "Tên thư mục trùng tên thiết bị Windows: {folder:?}"
        ));
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
struct SortLabels {
    trend: &'static str,
    totaluniquesubscriptions: &'static str,
    toprated: &'static str,
    num_parent_items: &'static str,
    mostrecent: &'static str,
    textsearch: &'static str,
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
    sorts: SortLabels,
    /// [value, label] pairs for the time-window picker; the UI renders them in
    /// order, so a plain array keeps that order.
    periods: Vec<[&'static str; 2]>,
    tags: Vec<String>,
    bisect_ready: bool,
    /// Routes still served by pzmod.py; the UI greys those tabs out.
    ported: Vec<&'static str>,
}

fn state_internal(game: &Path, user: &Path) -> Result<State, String> {
    let mods = user.join("mods");
    let off = user.join("mods_off");
    ensure_dirs(&mods, &off)?;

    let mut folders: Vec<Folder> = Vec::new();
    let on = dir_names(&mods);
    let disabled = dir_names(&off);

    for name in &on {
        let status = if disabled.contains(name) {
            "collision"
        } else {
            "enabled"
        };
        folders.push(Folder {
            name: name.clone(),
            status: status.into(),
            enabled: true,
        });
    }
    for name in &disabled {
        if on.contains(name) {
            continue; // already reported as a collision above
        }
        folders.push(Folder {
            name: name.clone(),
            status: "disabled".into(),
            enabled: false,
        });
    }
    folders.sort_by_key(|f| f.name.to_lowercase());

    let managed = managed_state(user);
    let mut installed = BTreeMap::new();
    let mut known = HashSet::new();
    for (id, entry) in &managed {
        let folder_values: Vec<Value> = entry_folders(entry)
            .into_iter()
            .map(|name| {
                known.insert(name.clone());
                let status = folder_status_at(&mods, &off, &name);
                json!({"name": name, "status": status})
            })
            .collect();
        let present = folder_values.iter().any(|folder| {
            matches!(
                folder["status"].as_str(),
                Some("enabled" | "disabled" | "collision")
            )
        });
        installed.insert(
            id.clone(),
            json!({
                "title": entry.get("title").and_then(Value::as_str).unwrap_or(id),
                "size": value_u64(entry, "size"),
                "updated": value_u64(entry, "updated"),
                "modids": entry.get("modids").cloned().unwrap_or_else(|| json!([])),
                "require": entry.get("require").cloned().unwrap_or_else(|| json!([])),
                "folders": folder_values,
                "present": present,
            }),
        );
    }
    folders.retain(|folder| !known.contains(&folder.name));

    Ok(State {
        game: game.display().to_string(),
        user: user.display().to_string(),
        mods: mods.display().to_string(),
        off: off.display().to_string(),
        appid: APPID,
        installed,
        loose: folders,
        missing: missing_modids(&managed, &mods, &off),
        sorts: SortLabels {
            trend: SORT_LABELS[0].1,
            totaluniquesubscriptions: SORT_LABELS[1].1,
            toprated: SORT_LABELS[2].1,
            num_parent_items: SORT_LABELS[3].1,
            mostrecent: SORT_LABELS[4].1,
            textsearch: SORT_LABELS[5].1,
        },
        periods: PERIODS.iter().map(|(v, label)| [*v, *label]).collect(),
        tags: TAGS.iter().map(|tag| (*tag).to_string()).collect(),
        bisect_ready: true,
        ported: vec![
            "state", "enable", "disable", "browse", "detail", "bisect", "install", "remove",
            "update",
        ],
    })
}

#[tauri::command]
fn state() -> Result<State, String> {
    let (game, user, _, _) = paths();
    let _guard = work_guard()?;
    state_internal(&game, &user)
}

#[derive(Serialize)]
struct Done {
    ok: bool,
    // A LIST, not a string: pzmod_gui.py answers /api/enable with
    // {"log": ["bật X"]} and ui.html calls .map() on it. One string here makes
    // showLog() throw after a toggle that actually succeeded.
    log: Vec<String>,
}

/// Every mover goes through check_folder, not just the two webview commands —
/// bisect feeds names straight out of .pzbisect.json, which a user can edit.
fn move_folder(from: &Path, to: &Path, folder: &str, verb: &str) -> Result<Done, String> {
    let folder = &check_folder(folder)?;
    let src = from.join(folder);
    let dst = to.join(folder);

    if is_dir(&src) && is_dir(&dst) {
        return Err(format!(
            "'{folder}' có ở cả mods và mods_off — dọn tay một bên trước, pzmod không đoán bản nào đúng."
        ));
    }
    if is_dir(&dst) {
        return Ok(Done {
            ok: true,
            log: vec![format!("'{folder}' đã {verb} sẵn.")],
        });
    }
    if !is_dir(&src) {
        return Err(format!(
            "Không tìm thấy thư mục mod '{folder}' trong mods/ hoặc mods_off/"
        ));
    }
    fs::rename(&src, &dst).map_err(|e| format!("Không {verb} được '{folder}': {e}"))?;
    Ok(Done {
        ok: true,
        log: vec![format!("Đã {verb} '{folder}'.")],
    })
}

#[tauri::command]
fn enable(folder: String) -> Result<Done, String> {
    let _guard = work_guard()?;
    let folder = check_folder(&folder)?;
    let (_, _, mods, off) = paths();
    ensure_dirs(&mods, &off)?;
    move_folder(&off, &mods, &folder, "bật")
}

#[tauri::command]
fn disable(folder: String) -> Result<Done, String> {
    let _guard = work_guard()?;
    let folder = check_folder(&folder)?;
    let (_, _, mods, off) = paths();
    ensure_dirs(&mods, &off)?;
    move_folder(&mods, &off, &folder, "tắt")
}

fn install_from_source(
    id: &str,
    detail: &Value,
    source: &Path,
    user: &Path,
) -> Result<Vec<String>, String> {
    let mods = user.join("mods");
    let off = user.join("mods_off");
    ensure_dirs(&mods, &off)?;
    let roots = mod_roots(source)?;
    if roots.is_empty() {
        return Err("không tìm thấy mods/<ModName>/mod.info".into());
    }
    let mut state = managed_state(user);
    let previous = state.get(id).cloned().unwrap_or_else(|| json!({}));
    let mut old_folders = entry_folders(&previous);
    old_folders.sort_by_key(|folder| folder.to_lowercase());
    old_folders.dedup();
    let old_set: HashSet<_> = old_folders.iter().cloned().collect();
    let mut old_locations = HashMap::new();

    for folder in &old_folders {
        let folder = check_folder(folder)?;
        let enabled = mods.join(&folder);
        let disabled = off.join(&folder);
        if enabled.is_dir() && disabled.is_dir() {
            return Err(format!(
                "{folder} có ở cả mods và mods_off; giữ nguyên để người dùng xử lý"
            ));
        }
        if [enabled.as_path(), disabled.as_path()]
            .iter()
            .any(|path| path.exists() && !path.is_dir())
        {
            return Err(format!("{folder} trùng với một file; giữ nguyên"));
        }
        if enabled.is_dir() {
            old_locations.insert(folder, mods.clone());
        } else if disabled.is_dir() {
            old_locations.insert(folder, off.clone());
        }
    }

    for root in &roots {
        if !old_set.contains(&root.folder)
            && [mods.join(&root.folder), off.join(&root.folder)]
                .iter()
                .any(|path| path.exists())
        {
            return Err(format!(
                "thư mục {} đã tồn tại và không thuộc bản cài này",
                root.folder
            ));
        }
    }

    let (modids, required) = mod_metadata(&roots)?;
    let transaction = transaction_dir(user, &format!("install-{id}"))?;
    let staged = transaction.join("new");
    let backup = transaction.join("old");
    if let Err(error) = fs::create_dir(&staged) {
        fs::remove_dir_all(&transaction).ok();
        return Err(format!("Không tạo được staging: {error}"));
    }
    let mut installed = Vec::new();
    let mut moved = Vec::new();
    let mut preserved = Vec::new();

    let result = (|| -> Result<(), String> {
        for root in &roots {
            copy_tree(&root.root, &staged.join(&root.folder))?;
        }
        for folder in &old_folders {
            for (label, base) in [("mods", &mods), ("mods_off", &off)] {
                let original = base.join(folder);
                if original.is_dir() {
                    let saved = backup.join(label).join(folder);
                    fs::create_dir_all(saved.parent().expect("backup has parent"))
                        .map_err(|e| format!("Không tạo được backup: {e}"))?;
                    fs::rename(&original, &saved)
                        .map_err(|e| format!("Không backup được {}: {e}", original.display()))?;
                    moved.push((saved, original));
                }
            }
        }

        let new_folders: HashSet<_> = roots.iter().map(|root| root.folder.clone()).collect();
        for root in &roots {
            let base = old_locations.get(&root.folder).unwrap_or(&mods);
            let destination = base.join(&root.folder);
            fs::rename(staged.join(&root.folder), &destination)
                .map_err(|e| format!("Không cài được thư mục {}: {e}", root.folder))?;
            installed.push(destination);
        }
        for folder in old_set.difference(&new_folders) {
            let Some(base) = old_locations.get(folder) else {
                continue;
            };
            let label = if base == &mods { "mods" } else { "mods_off" };
            let saved = backup.join(label).join(folder);
            let destination = off.join(folder);
            installed.push(destination.clone());
            copy_tree(&saved, &destination)?;
            preserved.push(folder.clone());
        }

        let title = value_string(detail, "title");
        let title = if title.is_empty() {
            id.to_string()
        } else {
            title
        };
        state.insert(
            id.to_string(),
            json!({
                "title": title,
                "updated": value_u64(detail, "time_updated"),
                "size": value_u64(detail, "file_size"),
                "folders": roots.iter().map(|root| root.folder.clone()).collect::<Vec<_>>(),
                "modids": modids,
                "require": required,
            }),
        );
        write_managed_state(user, &state)
    })();

    if let Err(error) = result {
        match rollback_files(&installed, &moved) {
            Ok(()) => {
                fs::remove_dir_all(&transaction).ok();
                return Err(error);
            }
            Err(rollback) => {
                return Err(format!(
                    "{error}; KHÔNG KHÔI PHỤC ĐƯỢC, bản sao còn tại {} ({rollback})",
                    transaction.display()
                ));
            }
        }
    }

    let mut log = preserved
        .into_iter()
        .map(|folder| format!("! {folder} không còn trong bản mới, đã chuyển sang mods_off"))
        .collect::<Vec<_>>();
    log.push(format!(
        "+ {} -> {} thư mục ({:.1} KB)",
        value_string(detail, "title"),
        roots.len(),
        value_u64(detail, "file_size") as f64 / 1024.0
    ));
    if let Err(error) = fs::remove_dir_all(&transaction) {
        log.push(format!(
            "! Không dọn được transaction {}: {error}",
            transaction.display()
        ));
    }
    Ok(log)
}

fn install_one(id: &str, detail: &Value, force: bool, user: &Path) -> Result<Vec<String>, String> {
    if value_u64(detail, "consumer_app_id") != 108600 {
        return Err(format!(
            "thuộc app {}, không phải Project Zomboid",
            value_u64(detail, "consumer_app_id")
        ));
    }
    let state = managed_state(user);
    let previous = state.get(id);
    let mods = user.join("mods");
    let off = user.join("mods_off");
    if previous.is_some_and(|entry| {
        !force
            && value_u64(entry, "updated") == value_u64(detail, "time_updated")
            && entry_complete(entry, &mods, &off)
    }) {
        return Ok(vec![format!(
            "= {} đã là bản mới nhất",
            value_string(detail, "title")
        )]);
    }
    let source = download_item(id, previous.is_some() || force)?;
    install_from_source(id, detail, &source, user)
}

fn walk_dependencies(
    id: &str,
    depth: usize,
    seen: &mut HashSet<String>,
    order: &mut Vec<String>,
    log: &mut Vec<String>,
) {
    if !seen.insert(id.to_string()) {
        return;
    }
    if depth > 0 {
        match required_ids(id) {
            Ok(required) => {
                for dependency in required {
                    walk_dependencies(&dependency, depth - 1, seen, order, log);
                }
            }
            Err(error) => log.push(format!("! {id}: không dò được mod bắt buộc ({error})")),
        }
    }
    order.push(id.to_string());
}

fn install_internal(id: &str, force: bool, user: &Path) -> Result<Done, String> {
    if id.is_empty() || !id.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err("bad id".into());
    }
    let original_meta = details(&[id.to_string()])?;
    let mut todo = collection_children(id)?;
    let mut log = Vec::new();
    if todo.is_empty() {
        todo.push(id.to_string());
    } else {
        log.push(format!(
            "collection {}: {} mod",
            original_meta
                .get(id)
                .map(|value| value_string(value, "title"))
                .filter(|title| !title.is_empty())
                .unwrap_or_else(|| id.to_string()),
            todo.len()
        ));
    }
    let requested: HashSet<_> = todo.iter().cloned().collect();
    let mut seen = HashSet::new();
    let mut order = Vec::new();
    for item in todo {
        walk_dependencies(&item, 4, &mut seen, &mut order, &mut log);
    }
    let extra: Vec<_> = order
        .iter()
        .filter(|item| !requested.contains(*item))
        .cloned()
        .collect();
    if !extra.is_empty() {
        let names = details(&extra)?;
        log.push(format!(
            "kèm {} mod bắt buộc: {}",
            extra.len(),
            extra
                .iter()
                .map(|item| {
                    names
                        .get(item)
                        .map(|value| value_string(value, "title"))
                        .filter(|title| !title.is_empty())
                        .unwrap_or_else(|| item.clone())
                })
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }

    let metadata = details(&order)?;
    let mut ok = true;
    for item in order {
        match metadata.get(&item) {
            Some(detail) => match install_one(&item, detail, force, user) {
                Ok(lines) => log.extend(lines),
                Err(error) => {
                    ok = false;
                    log.push(format!("! {item}: {error}"));
                }
            },
            None => {
                ok = false;
                log.push(format!("! {item}: mod đã bị xoá hoặc đặt riêng tư"));
            }
        }
    }
    let state = managed_state(user);
    let missing = missing_modids(&state, &user.join("mods"), &user.join("mods_off"));
    if !missing.is_empty() {
        ok = false;
        log.push(format!("! thiếu mod bắt buộc: {}", missing.join(", ")));
    }
    if log.iter().any(|line| line.trim_start().starts_with('!')) {
        ok = false;
    }
    Ok(Done { ok, log })
}

fn remove_internal(id: &str, user: &Path) -> Result<Done, String> {
    if id.is_empty() || !id.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err("bad id".into());
    }
    let mods = user.join("mods");
    let off = user.join("mods_off");
    let mut state = managed_state(user);
    let Some(entry) = state.get(id).cloned() else {
        return Ok(Done {
            ok: true,
            log: vec![format!("? {id} chưa được pzmod cài")],
        });
    };
    let mut paths = Vec::new();
    for folder in entry_folders(&entry) {
        let folder = check_folder(&folder)?;
        for path in [mods.join(&folder), off.join(&folder)] {
            if path.exists() && !path.is_dir() {
                return Err(format!(
                    "Từ chối xoá vì không phải thư mục: {}",
                    path.display()
                ));
            }
            if path.is_dir() && !paths.contains(&path) {
                paths.push(path);
            }
        }
    }

    let transaction = transaction_dir(user, &format!("remove-{id}"))?;
    let mut moved = Vec::new();
    let result = (|| -> Result<(), String> {
        for path in &paths {
            let label = if path.parent() == Some(mods.as_path()) {
                "mods"
            } else {
                "mods_off"
            };
            let saved =
                transaction
                    .join(label)
                    .join(path.file_name().ok_or_else(|| {
                        format!("Đường dẫn mod không hợp lệ: {}", path.display())
                    })?);
            fs::create_dir_all(saved.parent().expect("saved has parent"))
                .map_err(|e| format!("Không tạo được transaction: {e}"))?;
            fs::rename(path, &saved)
                .map_err(|e| format!("Không staging được {}: {e}", path.display()))?;
            moved.push((saved, path.clone()));
        }
        state.remove(id);
        write_managed_state(user, &state)
    })();

    if let Err(error) = result {
        match rollback_files(&[], &moved) {
            Ok(()) => {
                fs::remove_dir_all(&transaction).ok();
                return Err(error);
            }
            Err(rollback) => {
                return Err(format!(
                    "{error}; KHÔNG KHÔI PHỤC ĐƯỢC, bản sao còn tại {} ({rollback})",
                    transaction.display()
                ));
            }
        }
    }
    let mut log = vec![format!(
        "- {} đã xoá",
        entry.get("title").and_then(Value::as_str).unwrap_or(id)
    )];
    let mut ok = true;
    if let Err(error) = fs::remove_dir_all(&transaction) {
        ok = false;
        log.push(format!(
            "! Đã gỡ state nhưng không dọn được {}: {error}",
            transaction.display()
        ));
    }
    Ok(Done { ok, log })
}

fn update_internal(user: &Path) -> Result<Done, String> {
    let state = managed_state(user);
    if state.is_empty() {
        return Ok(Done {
            ok: true,
            log: vec!["không có mod để cập nhật".into()],
        });
    }
    let ids: Vec<_> = state.keys().cloned().collect();
    let metadata = details(&ids)?;
    let mut log = Vec::new();
    let mut ok = true;
    for id in ids {
        let entry = &state[&id];
        let title = entry.get("title").and_then(Value::as_str).unwrap_or(&id);
        match metadata.get(&id) {
            None => {
                ok = false;
                log.push(format!(
                    "! {title}: đã biến mất khỏi workshop, giữ nguyên bản cục bộ"
                ));
            }
            Some(detail)
                if value_u64(detail, "time_updated") == value_u64(entry, "updated")
                    && entry_complete(entry, &user.join("mods"), &user.join("mods_off")) =>
            {
                log.push(format!("= {title}"));
            }
            Some(detail) => match install_one(&id, detail, true, user) {
                Ok(lines) => log.extend(lines),
                Err(error) => {
                    ok = false;
                    log.push(format!("! {title}: {error}"));
                }
            },
        }
    }
    let refreshed = managed_state(user);
    let missing = missing_modids(&refreshed, &user.join("mods"), &user.join("mods_off"));
    if !missing.is_empty() {
        ok = false;
        log.push(format!("! thiếu mod bắt buộc: {}", missing.join(", ")));
    }
    if log.iter().any(|line| line.trim_start().starts_with('!')) {
        ok = false;
    }
    Ok(Done { ok, log })
}

#[tauri::command]
fn install(id: String, force: Option<bool>) -> Result<Done, String> {
    let _guard = work_guard()?;
    let (_, user, _, _) = paths();
    install_internal(&id, force.unwrap_or(false), &user)
}

#[tauri::command]
fn remove(id: String) -> Result<Done, String> {
    let _guard = work_guard()?;
    let (_, user, _, _) = paths();
    remove_internal(&id, &user)
}

#[tauri::command]
fn update() -> Result<Done, String> {
    let _guard = work_guard()?;
    let (_, user, _, _) = paths();
    update_internal(&user)
}

fn installed_folders_rs(user: &Path) -> Result<Vec<(String, bool)>, String> {
    let mods = user.join("mods");
    let off = user.join("mods_off");
    ensure_dirs(&mods, &off)?;

    let on = dir_names(&mods);
    let off_list = dir_names(&off);

    let on_set: HashSet<_> = on.into_iter().collect();
    let off_set: HashSet<_> = off_list.into_iter().collect();

    let collisions: Vec<_> = on_set.intersection(&off_set).cloned().collect();
    if !collisions.is_empty() {
        return Err(format!(
            "Xung đột tên mod: tồn tại đồng thời ở cả mods/ và mods_off/: {}",
            collisions.join(", ")
        ));
    }

    let mut res: Vec<(String, bool)> = Vec::new();
    for f in &on_set {
        res.push((f.clone(), true));
    }
    for f in &off_set {
        res.push((f.clone(), false));
    }
    res.sort_by_key(|a| a.0.to_lowercase());
    Ok(res)
}

fn bisect_state_internal(user: &Path) -> Result<Value, String> {
    let state_file = user.join(".pzbisect.json");
    let installed = installed_folders_rs(user)?;
    let enabled_now: Vec<String> = installed
        .iter()
        .filter(|(_, en)| *en)
        .map(|(f, _)| f.clone())
        .collect();

    if !state_file.is_file() {
        return Ok(json!({
            "round": 0,
            "candidates": json!([]),
            "enabled_now": enabled_now,
            "suspect": Value::Null,
            "done": false
        }));
    }

    let text = fs::read_to_string(&state_file)
        .map_err(|e| format!("Không đọc được file .pzbisect.json: {e}"))?;
    let st: Value = serde_json::from_str(&text)
        .map_err(|e| format!("File .pzbisect.json chứa JSON không hợp lệ: {e}"))?;

    Ok(json!({
        "round": st.get("round").and_then(Value::as_u64).unwrap_or(0),
        "candidates": st.get("candidates").cloned().unwrap_or(json!([])),
        "enabled_now": enabled_now,
        "suspect": st.get("suspect").cloned().unwrap_or(Value::Null),
        "done": st.get("done").and_then(Value::as_bool).unwrap_or(false)
    }))
}

fn bisect_view_internal(user: &Path) -> Result<Value, String> {
    let st = bisect_state_internal(user)?;
    let enabled_set: HashSet<String> = st["enabled_now"]
        .as_array()
        .unwrap_or(&vec![])
        .iter()
        .filter_map(|v| v.as_str().map(String::from))
        .collect();
    let cands: Vec<String> = st["candidates"]
        .as_array()
        .unwrap_or(&vec![])
        .iter()
        .filter_map(|v| v.as_str().map(String::from))
        .collect();
    let tested: Vec<String> = cands
        .iter()
        .filter(|c| enabled_set.contains(*c))
        .cloned()
        .collect();
    let untested: Vec<String> = cands
        .iter()
        .filter(|c| !enabled_set.contains(*c))
        .cloned()
        .collect();
    let done = st["done"].as_bool().unwrap_or(false);
    let running = !cands.is_empty() && !done;

    Ok(json!({
        "ready": true,
        "running": running,
        "round": st["round"],
        "candidates": cands,
        "tested": tested,
        "untested": untested,
        "suspect": st["suspect"],
        "enabled_now": st["enabled_now"],
        "done": done
    }))
}

fn bisect_start_internal(names: Option<Vec<String>>, user: &Path) -> Result<Value, String> {
    let state_file = user.join(".pzbisect.json");
    if state_file.is_file() {
        if let Ok(text) = fs::read_to_string(&state_file) {
            if let Ok(val) = serde_json::from_str::<Value>(&text) {
                if !val.get("done").and_then(Value::as_bool).unwrap_or(false) {
                    return Err("Đang có phiên bisect hoạt động. Hãy chạy 'pzbisect stop' trước khi bắt đầu phiên mới.".into());
                }
            }
        }
    }

    let mods = user.join("mods");
    let off = user.join("mods_off");
    ensure_dirs(&mods, &off)?;

    let installed = installed_folders_rs(user)?;
    let installed_map: HashMap<String, bool> = installed.into_iter().collect();
    let mut original_enabled: Vec<String> = installed_map
        .iter()
        .filter(|(_, &en)| en)
        .map(|(k, _)| k.clone())
        .collect();
    original_enabled.sort_by_key(|a| a.to_lowercase());

    let mut candidates: Vec<String> = match names {
        None => original_enabled.clone(),
        Some(list) => {
            let mut cleaned = Vec::new();
            for n in list {
                let cn = check_folder(&n)?;
                if !installed_map.contains_key(&cn) {
                    return Err(format!("Mod '{cn}' không tồn tại trong danh sách cài đặt."));
                }
                cleaned.push(cn);
            }
            let set: HashSet<_> = cleaned.into_iter().collect();
            set.into_iter().collect()
        }
    };

    if candidates.len() < 2 {
        return Err(format!(
            "Bisect cần tối thiểu 2 candidates để bắt đầu (hiện có {}).",
            candidates.len()
        ));
    }

    for f in &candidates {
        check_folder(f)?;
    }

    candidates.sort_by_key(|a| a.to_lowercase());
    let mid = candidates.len().div_ceil(2);
    let left = candidates[..mid].to_vec();
    let right = candidates[mid..].to_vec();

    for f in &left {
        move_folder(&off, &mods, f, "bật")?;
    }
    for f in &right {
        move_folder(&mods, &off, f, "tắt")?;
    }

    let st = json!({
        "round": 1,
        "candidates": candidates,
        "current_tested": left,
        "current_untested": right,
        "original_enabled": original_enabled,
        "suspect": Value::Null,
        "done": false
    });

    let tmp = user.join(".pzbisect.json.tmp");
    let json_text =
        serde_json::to_string_pretty(&st).map_err(|e| format!("Không serialize được JSON: {e}"))?;
    fs::write(&tmp, json_text).map_err(|e| format!("Không ghi được file tạm: {e}"))?;
    fs::rename(&tmp, &state_file).map_err(|e| format!("Không lưu được file trạng thái: {e}"))?;

    bisect_view_internal(user)
}

fn bisect_mark_internal(bad: bool, user: &Path) -> Result<Value, String> {
    let state_file = user.join(".pzbisect.json");
    if !state_file.is_file() {
        return Err("Không có phiên bisect nào đang hoạt động.".into());
    }

    let text = fs::read_to_string(&state_file)
        .map_err(|e| format!("Không đọc được .pzbisect.json: {e}"))?;
    let mut st: Value =
        serde_json::from_str(&text).map_err(|e| format!("JSON không hợp lệ: {e}"))?;

    if st.get("done").and_then(Value::as_bool).unwrap_or(false) {
        return bisect_view_internal(user);
    }

    let current_tested: Vec<String> = st
        .get("current_tested")
        .and_then(Value::as_array)
        .unwrap_or(&vec![])
        .iter()
        .filter_map(|v| v.as_str().map(String::from))
        .collect();
    let current_untested: Vec<String> = st
        .get("current_untested")
        .and_then(Value::as_array)
        .unwrap_or(&vec![])
        .iter()
        .filter_map(|v| v.as_str().map(String::from))
        .collect();

    let mut new_candidates = if bad {
        current_tested
    } else {
        current_untested
    };

    if new_candidates.is_empty() {
        return Err("Lỗi logic bisect: danh sách candidates bị rỗng.".into());
    }

    for f in &new_candidates {
        check_folder(f)?;
    }

    let mods = user.join("mods");
    let off = user.join("mods_off");

    if new_candidates.len() == 1 {
        let suspect = new_candidates.remove(0);
        st["candidates"] = json!([suspect]);
        st["current_tested"] = json!([]);
        st["current_untested"] = json!([]);
        st["suspect"] = json!(suspect);
        st["done"] = json!(true);
    } else {
        let round = st.get("round").and_then(Value::as_u64).unwrap_or(1) + 1;
        new_candidates.sort_by_key(|a| a.to_lowercase());
        let mid = new_candidates.len().div_ceil(2);
        let left = new_candidates[..mid].to_vec();
        let right = new_candidates[mid..].to_vec();

        for f in &left {
            move_folder(&off, &mods, f, "bật")?;
        }
        for f in &right {
            move_folder(&mods, &off, f, "tắt")?;
        }

        st["round"] = json!(round);
        st["candidates"] = json!(new_candidates);
        st["current_tested"] = json!(left);
        st["current_untested"] = json!(right);
    }

    let tmp = user.join(".pzbisect.json.tmp");
    let json_text =
        serde_json::to_string_pretty(&st).map_err(|e| format!("Không serialize được JSON: {e}"))?;
    fs::write(&tmp, json_text).map_err(|e| format!("Không ghi được file tạm: {e}"))?;
    fs::rename(&tmp, &state_file).map_err(|e| format!("Không lưu được file trạng thái: {e}"))?;

    bisect_view_internal(user)
}

fn bisect_stop_internal(user: &Path) -> Result<Value, String> {
    let state_file = user.join(".pzbisect.json");
    if !state_file.is_file() {
        return Ok(json!({
            "stopped": false,
            "message": "Không có phiên bisect đang chạy",
            "done": false
        }));
    }

    let text = fs::read_to_string(&state_file)
        .map_err(|e| format!("Không đọc được .pzbisect.json: {e}"))?;
    let st: Value = serde_json::from_str(&text).map_err(|e| format!("JSON không hợp lệ: {e}"))?;

    let orig_arr = match st.get("original_enabled").and_then(Value::as_array) {
        Some(arr) => arr,
        None => {
            return Err("File trạng thái .pzbisect.json bị hỏng (thiếu trường 'original_enabled'). Dừng khôi phục để tránh tắt nhầm mod. Hãy khôi phục thủ công.".into());
        }
    };

    let orig_set: HashSet<String> = orig_arr
        .iter()
        .filter_map(|v| v.as_str().map(String::from))
        .collect();

    let mut snapshot_pool: HashSet<String> = orig_set.clone();
    for key in &["candidates", "current_tested", "current_untested"] {
        if let Some(arr) = st.get(key).and_then(Value::as_array) {
            for v in arr {
                if let Some(s) = v.as_str() {
                    snapshot_pool.insert(s.to_string());
                }
            }
        }
    }

    for f in &orig_set {
        check_folder(f)?;
    }
    for f in &snapshot_pool {
        check_folder(f)?;
    }

    let mods = user.join("mods");
    let off = user.join("mods_off");
    let installed = installed_folders_rs(user)?;

    for (folder, is_enabled) in installed {
        if orig_set.contains(&folder) && !is_enabled {
            move_folder(&off, &mods, &folder, "bật")?;
        } else if snapshot_pool.contains(&folder) && !orig_set.contains(&folder) && is_enabled {
            move_folder(&mods, &off, &folder, "tắt")?;
        }
    }

    fs::remove_file(&state_file).ok();
    let restored: Vec<String> = orig_set.into_iter().collect();
    Ok(json!({
        "stopped": true,
        "restored": restored,
        "done": true
    }))
}

#[tauri::command]
fn bisect(op: Option<String>, names: Option<Vec<String>>) -> Result<Value, String> {
    let _guard = work_guard()?;
    let (_, user, _, _) = paths();
    match op.as_deref().unwrap_or("view") {
        "view" | "" => bisect_view_internal(&user),
        "start" => {
            let state = bisect_start_internal(names, &user)?;
            Ok(json!({ "ok": true, "state": state }))
        }
        "bad" => {
            let state = bisect_mark_internal(true, &user)?;
            Ok(json!({ "ok": true, "state": state }))
        }
        "good" => {
            let state = bisect_mark_internal(false, &user)?;
            Ok(json!({ "ok": true, "state": state }))
        }
        "stop" => {
            bisect_stop_internal(&user)?;
            let state = bisect_view_internal(&user)?;
            Ok(json!({ "ok": true, "state": state }))
        }
        other => Err(format!("Lệnh bisect không hợp lệ: {other}")),
    }
}

fn main() {
    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![
            state, enable, disable, browse, detail, bisect, install, remove, update
        ])
        .run(tauri::generate_context!())
        .expect("pzmod: tauri failed to start");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_root(label: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "pzmod-rust-{label}-{}-{}",
            std::process::id(),
            unique_suffix()
        ));
        fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn steamcmd_argv_matches_python_lane() {
        let args: Vec<_> = steamcmd_args("123")
            .into_iter()
            .map(|arg| arg.into_string().unwrap())
            .collect();
        assert_eq!(
            args,
            [
                "+login",
                "anonymous",
                "+workshop_download_item",
                "108600",
                "123",
                "+quit"
            ]
        );
    }

    #[test]
    fn state_reads_the_python_schema() {
        let user = test_root("state");
        let mods = user.join("mods");
        let off = user.join("mods_off");
        ensure_dirs(&mods, &off).unwrap();
        fs::create_dir(mods.join("Owned")).unwrap();
        fs::create_dir(off.join("Loose")).unwrap();
        let state = Map::from_iter([(
            "10".into(),
            json!({
                "title": "Owned item", "updated": 2, "size": 3,
                "folders": ["Owned"], "modids": ["OwnedID"], "require": ["MissingID"]
            }),
        )]);
        write_managed_state(&user, &state).unwrap();

        let snapshot = state_internal(Path::new(r"D:\ProjectZomboid"), &user).unwrap();
        assert_eq!(snapshot.installed["10"]["title"], "Owned item");
        assert_eq!(snapshot.installed["10"]["folders"][0]["status"], "enabled");
        assert_eq!(snapshot.loose.len(), 1);
        assert_eq!(snapshot.loose[0].name, "Loose");
        assert_eq!(snapshot.missing, ["MissingID"]);
        let json = serde_json::to_value(&snapshot).unwrap();
        assert_eq!(json["sorts"]["trend"], "Thịnh hành tuần");
        assert!(json["tags"].as_array().unwrap().len() > 20);
        fs::remove_dir_all(user).ok();
    }

    #[test]
    fn install_preserves_disabled_and_retired_folders() {
        let user = test_root("install");
        let mods = user.join("mods");
        let off = user.join("mods_off");
        ensure_dirs(&mods, &off).unwrap();
        fs::create_dir(off.join("Keep")).unwrap();
        fs::write(off.join("Keep").join("old.txt"), "old").unwrap();
        fs::create_dir(mods.join("Retired")).unwrap();
        fs::write(mods.join("Retired").join("retired.txt"), "retired").unwrap();
        write_managed_state(
            &user,
            &Map::from_iter([(
                "10".into(),
                json!({
                    "title": "Old", "updated": 1, "size": 1,
                    "folders": ["Keep", "Retired"], "modids": ["Old"], "require": []
                }),
            )]),
        )
        .unwrap();

        let source = user.join("source");
        fs::create_dir_all(source.join("mods").join("Keep").join("42.20")).unwrap();
        fs::write(
            source.join("mods").join("Keep").join("mod.info"),
            "id=KeepID\nrequire=\\BaseID\n",
        )
        .unwrap();
        fs::write(
            source
                .join("mods")
                .join("Keep")
                .join("42.20")
                .join("mod.info"),
            "id=keepid\nrequire=/OtherID\n",
        )
        .unwrap();
        fs::write(source.join("mods").join("Keep").join("new.txt"), "new").unwrap();
        let log = install_from_source(
            "10",
            &json!({
                "title": "New", "time_updated": 2, "file_size": 100,
                "consumer_app_id": 108600
            }),
            &source,
            &user,
        )
        .unwrap();

        assert!(off.join("Keep").join("new.txt").is_file());
        assert!(!mods.join("Keep").exists());
        assert!(off.join("Retired").join("retired.txt").is_file());
        assert!(log.iter().any(|line| line.contains("Retired không còn")));
        let state = managed_state(&user);
        assert_eq!(state["10"]["folders"], json!(["Keep"]));
        assert_eq!(state["10"]["modids"], json!(["KeepID"]));
        assert_eq!(state["10"]["require"], json!(["BaseID", "OtherID"]));

        fs::write(mods.join("Blocker"), "do not delete").unwrap();
        let mut state = managed_state(&user);
        state.get_mut("10").unwrap()["folders"] = json!(["Keep", "Blocker"]);
        write_managed_state(&user, &state).unwrap();
        assert!(remove_internal("10", &user).is_err());
        assert!(off.join("Keep").is_dir());
        assert!(managed_state(&user).contains_key("10"));
        fs::remove_file(mods.join("Blocker")).unwrap();
        remove_internal("10", &user).unwrap();
        assert!(!off.join("Keep").exists());
        assert!(!managed_state(&user).contains_key("10"));
        assert!(
            off.join("Retired").is_dir(),
            "retired folder is loose, not deleted"
        );
        fs::remove_dir_all(user).ok();
    }

    #[test]
    fn every_sort_but_mostrecent_carries_a_time_window() {
        assert_eq!(sort_days("trend"), Some("7"));
        assert_eq!(sort_days("totaluniquesubscriptions"), Some("3650"));
        assert_eq!(sort_days("mostrecent"), None);
        // the picker offers exactly the windows Steam ranks over
        let values: Vec<&str> = PERIODS.iter().map(|(value, _)| *value).collect();
        assert_eq!(values, ["7", "30", "90", "180", "365", "3650"]);
    }

    #[test]
    fn tag_filter_drops_the_pinned_notice() {
        let pinned = json!({"tags": [{"tag": "Modding or Configuration"}]});
        let car = json!({"tags": [{"tag": "Build 42"}, {"tag": "Vehicles"}]});
        let wanted = vec!["Vehicles".to_string()];
        assert!(!has_tags(&pinned, &wanted));
        assert!(has_tags(&car, &wanted));
        assert!(has_tags(&pinned, &[]));
        // every requested tag has to be present, not just one of them
        assert!(!has_tags(&car, &["Vehicles".into(), "Map".into()]));
    }

    #[test]
    fn parses_cached_workshop_listing_fixture() {
        let html = include_str!("../tests/fixtures/workshop_browse.html");
        assert_eq!(extract_listing_ids(html), ["111", "222"]);
    }

    #[test]
    fn parses_required_block_without_leaking_other_links() {
        let html = r#"<div id="RequiredItems">
            <a href="filedetails/?id=10">self</a>
            <a href="filedetails/?id=20">dependency</a>
            <a href="filedetails/?id=20">duplicate</a>
            <!-- next panel --><a href="filedetails/?id=30">unrelated</a>"#;
        let start = html.find("id=\"RequiredItems\"").unwrap();
        let block = &html[start..html[start..].find("<!--").unwrap() + start];
        let ids: Vec<_> = ids_after(block, "filedetails/?id=")
            .into_iter()
            .filter(|id| id != "10")
            .collect();
        assert_eq!(ids, ["20"]);
    }

    #[test]
    fn strips_long_bounded_bbcode() {
        let tag = format!("[url=https://example.invalid/{}]", "x".repeat(80));
        assert_eq!(strip_bb(&format!("a {tag}link[/url] b")), "a link b");
        assert_eq!(strip_bb("keep [ unmatched text"), "keep [ unmatched text");
    }

    #[test]
    #[ignore = "live Steam API/community check"]
    fn live_workshop_commands_match_ui_shape() {
        let listing = browse("498441420".into(), "trend".into(), 1, Vec::new(), None).unwrap();
        let item = &listing["items"][0];
        assert_eq!(item["id"], "498441420");
        assert!(item["preview"].as_str().unwrap().starts_with("https://"));

        let item = detail("498441420".into()).unwrap();
        assert_eq!(item["id"], "498441420");
        assert!(item["description"].is_string());
        assert!(item["required"].is_array());
    }

    #[test]
    fn folder_names_are_sandboxed() {
        assert!(check_folder("ZBPopFix").is_ok());
        assert!(check_folder("  ZBPopFix  ").is_ok());
        for bad in [
            "",
            ".",
            "..",
            "../evil",
            r"..\evil",
            r"C:\Windows",
            "a/b",
            "CON",
            "nul",
            "Com1.txt",
            "LPT9",
            "ab\0cd",
            "a\u{2215}b",
            "a\u{FF3C}b",
        ] {
            assert!(check_folder(bad).is_err(), "phải chặn: {bad:?}");
        }
        // COM10 is not a device — only COM1..COM9 are.
        assert!(check_folder("COM10").is_ok());
    }

    #[test]
    fn move_folder_rejects_names_from_a_hand_edited_state_file() {
        let base = std::env::temp_dir().join(format!("pzmod-state-{}", std::process::id()));
        let mods = base.join("mods");
        let off = base.join("mods_off");
        ensure_dirs(&mods, &off).unwrap();
        assert!(move_folder(&mods, &off, r"..\evil", "tắt").is_err());
        assert!(move_folder(&mods, &off, "sub/ModA", "tắt").is_err());
        fs::remove_dir_all(&base).ok();
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

    #[test]
    fn test_bisect_start_refusal_when_active() {
        let base = std::env::temp_dir().join(format!("pzmod-bisect-test1-{}", std::process::id()));
        let mods = base.join("mods");
        let off = base.join("mods_off");
        ensure_dirs(&mods, &off).unwrap();
        fs::create_dir_all(mods.join("ModA")).unwrap();
        fs::create_dir_all(mods.join("ModB")).unwrap();

        let st = bisect_start_internal(None, &base).unwrap();
        assert_eq!(st["round"], 1);

        // Calling start again while active must fail
        assert!(bisect_start_internal(None, &base).is_err());

        bisect_stop_internal(&base).unwrap();
        fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn test_bisect_stop_refusal_on_corrupted_state() {
        let base = std::env::temp_dir().join(format!("pzmod-bisect-test2-{}", std::process::id()));
        let mods = base.join("mods");
        let off = base.join("mods_off");
        ensure_dirs(&mods, &off).unwrap();
        fs::create_dir_all(mods.join("ModA")).unwrap();
        fs::create_dir_all(mods.join("ModB")).unwrap();

        let state_file = base.join(".pzbisect.json");
        fs::write(
            &state_file,
            r#"{"round": 1, "candidates": ["ModA", "ModB"]}"#,
        )
        .unwrap();

        // Must refuse to stop and not touch filesystem
        assert!(bisect_stop_internal(&base).is_err());
        assert!(mods.join("ModA").is_dir() && mods.join("ModB").is_dir());

        fs::remove_file(&state_file).ok();
        fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn test_bisect_stop_preserves_newly_installed_mod() {
        let base = std::env::temp_dir().join(format!("pzmod-bisect-test3-{}", std::process::id()));
        let mods = base.join("mods");
        let off = base.join("mods_off");
        ensure_dirs(&mods, &off).unwrap();
        fs::create_dir_all(mods.join("ModA")).unwrap();
        fs::create_dir_all(mods.join("ModB")).unwrap();
        fs::create_dir_all(off.join("ModC")).unwrap();

        bisect_start_internal(None, &base).unwrap();

        // Install new mod mid-way
        fs::create_dir_all(mods.join("NewMod")).unwrap();

        bisect_stop_internal(&base).unwrap();

        assert!(mods.join("ModA").is_dir());
        assert!(mods.join("ModB").is_dir());
        assert!(off.join("ModC").is_dir());
        assert!(mods.join("NewMod").is_dir()); // Not disabled

        fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn test_bisect_hand_edited_dirty_name_in_state_fails_cleanly_without_partial_move() {
        let base = std::env::temp_dir().join(format!("pzmod-bisect-test4-{}", std::process::id()));
        let mods = base.join("mods");
        let off = base.join("mods_off");
        ensure_dirs(&mods, &off).unwrap();
        fs::create_dir_all(mods.join("ModA")).unwrap();
        fs::create_dir_all(mods.join("ModB")).unwrap();

        let state_file = base.join(".pzbisect.json");
        let dirty_state = json!({
            "round": 1,
            "candidates": ["ModA", "../evil_traversal"],
            "current_tested": ["ModA"],
            "current_untested": ["../evil_traversal"],
            "original_enabled": ["ModA", "ModB"],
            "suspect": Value::Null,
            "done": false
        });
        fs::write(
            &state_file,
            serde_json::to_string_pretty(&dirty_state).unwrap(),
        )
        .unwrap();

        // Must fail upfront and not move ModA
        assert!(bisect_mark_internal(false, &base).is_err());
        assert!(
            mods.join("ModA").is_dir(),
            "ModA không được dời dở dang khi gặp tên bẩn"
        );

        fs::remove_file(&state_file).ok();
        fs::remove_dir_all(&base).ok();
    }
}
