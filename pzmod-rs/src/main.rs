// pzmod — Project Zomboid workshop mod manager (Rust + Tauri 2).
// Port in progress: filesystem lane (list / enable / disable) is live here;
// workshop browse + steamcmd install still live in pzmod.py. See ROUTES.md.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::collections::{hash_map::DefaultHasher, BTreeMap, HashMap, HashSet};
use std::fmt;
use std::fs;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

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
    "mostrecent",
    "textsearch",
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

#[tauri::command]
fn browse(q: String, sort: String, page: u32, tags: Vec<String>) -> Result<Value, String> {
    let page = page.clamp(1, 50);
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
        if sort == "trend" {
            fields.push(("days".into(), "7".into()));
        }
        fields.extend(
            tags.into_iter()
                .filter(|tag| TAGS.contains(&tag.as_str()))
                .map(|tag| ("requiredtags[]".into(), tag)),
        );
        let html = cached_page(&(BROWSE_URL.to_string() + &form_body(&fields)), BROWSE_TTL)
            .map_err(|error| error.to_string())?;
        extract_listing_ids(&html)
    };

    let metadata = details(&ids)?;
    let items: Vec<Value> = ids
        .iter()
        .filter_map(|id| metadata.get(id).map(card))
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
        ported: vec!["state", "enable", "disable", "browse", "detail"],
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
        return Ok(Done {
            ok: true,
            log: format!("'{folder}' đã {verb} sẵn."),
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
        log: format!("Đã {verb} '{folder}'."),
    })
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
        .invoke_handler(tauri::generate_handler![
            state, enable, disable, browse, detail
        ])
        .run(tauri::generate_context!())
        .expect("pzmod: tauri failed to start");
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let listing = browse("498441420".into(), "trend".into(), 1, Vec::new()).unwrap();
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
