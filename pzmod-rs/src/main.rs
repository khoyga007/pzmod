// pzmod — Project Zomboid workshop mod manager (Rust + Tauri 2).
// All routes used by ui.html are native here; this is the sole runtime lane.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::collections::{hash_map::DefaultHasher, BTreeMap, HashMap, HashSet, VecDeque};
use std::ffi::OsString;
use std::fmt;
use std::fs;
use std::hash::{Hash, Hasher};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex, OnceLock};
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
/// Item pages are ~130 KB against ~680 KB for a listing page, so Steam tolerates a
/// tighter cadence for them. Measured 30/08/2026: the fetch itself is 0.7 s, so
/// 2 s was pure waiting. 0.35 s x 4 workers did run 4.5x faster but tripped
/// Steam's limiter after ~22 pages, so the shipped numbers stay well under that.
const ITEM_GAP: Duration = Duration::from_millis(800);
const ITEM_WORKERS: usize = 3;
/// A 429 anywhere drops every page back to the listing cadence for this long.
const RATE_LIMIT_COOLDOWN: Duration = Duration::from_secs(20 * 60);
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

/// How strongly a folder looks like THE folder the game itself reads.
/// 2 = the game wrote there (options.ini/console.txt/Saves are game-authored and
/// pzmod never creates them). 1 = only a non-empty mods/ folder, which is what a
/// pzmod-managed -cachedir tree looks like before the game has ever run in it.
/// 0 = no. ensure_dirs() creates empty mods/ + mods_off/, so an empty mods/ must
/// never score, or pzmod would keep re-electing a folder it made up itself.
fn pz_user_rank(dir: &Path) -> u8 {
    if GAME_MARKERS.iter().any(|marker| dir.join(marker).exists()) {
        return 2;
    }
    let has_mods = fs::read_dir(dir.join("mods"))
        .map(|mut entries| entries.next().is_some())
        .unwrap_or(false);
    u8::from(has_mods)
}

const GAME_MARKERS: [&str; 3] = ["options.ini", "console.txt", "Saves"];

/// Newest game-written marker in the folder — how recently the game actually ran
/// there. Two folders can both be real (moving the user folder with -cachedir
/// leaves the old one behind, files and all); the fresher one is the live one.
fn pz_user_touched(dir: &Path) -> SystemTime {
    GAME_MARKERS
        .iter()
        .filter_map(|marker| fs::metadata(dir.join(marker)).ok()?.modified().ok())
        .max()
        .unwrap_or(SystemTime::UNIX_EPOCH)
}

/// dev.bat sources pz-paths.bat, so PZ_USER is always set while developing. An
/// installed build launched from the Start Menu has no such wrapper, and the old
/// %USERPROFILE%\Zomboid fallback then pointed at a folder the game never uses:
/// ensure_dirs() created mods/ and mods_off/ there and the app reported "no mods
/// installed" while the real ones sat under a -cachedir path on another drive.
/// So sweep the drives before falling back.
fn detect_user_dir() -> Option<PathBuf> {
    static DETECTED: OnceLock<Option<PathBuf>> = OnceLock::new();
    DETECTED
        .get_or_init(|| {
            let mut candidates = Vec::new();
            if let Some(home) = env_path("USERPROFILE") {
                candidates.push(home.join("Zomboid"));
            }
            for letter in 'A'..='Z' {
                let root = PathBuf::from(format!("{letter}:\\"));
                if !root.is_dir() {
                    continue;
                }
                candidates.push(root.join("ProjectZomboid").join("Zomboid"));
                candidates.push(root.join("Zomboid"));
            }
            let mut ranked: Vec<(u8, SystemTime, PathBuf)> = candidates
                .into_iter()
                .map(|dir| (pz_user_rank(&dir), pz_user_touched(&dir), dir))
                .filter(|(rank, _, _)| *rank > 0)
                .collect();
            // Best rank first, then the folder the game touched most recently;
            // sort_by is stable, so a tie keeps candidate order.
            ranked.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.cmp(&a.1)));
            ranked.into_iter().next().map(|(_, _, dir)| dir)
        })
        .clone()
}

/// (game, user, mods, mods_off) — env overrides keep dev and tests pointed at
/// a throwaway tree instead of the live game profile.
fn paths() -> (PathBuf, PathBuf, PathBuf, PathBuf) {
    let game = env_path("PZ_GAME").unwrap_or_else(|| PathBuf::from(r"D:\ProjectZomboid"));
    let user = env_path("PZ_USER")
        .or_else(detect_user_dir)
        .unwrap_or_else(|| {
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

const PROGRESS_CAP: usize = 500;

#[derive(Default)]
struct ProgressStore {
    seq: usize,
    lines: VecDeque<String>,
    active: bool,
}

#[derive(Debug, Serialize)]
struct ProgressReply {
    seq: usize,
    lines: Vec<String>,
    active: bool,
}

impl ProgressStore {
    fn push(&mut self, line: String) {
        self.seq = self.seq.saturating_add(1);
        if self.lines.len() == PROGRESS_CAP {
            self.lines.pop_front();
        }
        self.lines.push_back(line);
    }

    fn snapshot(&self, since: usize) -> ProgressReply {
        let first = self.seq.saturating_sub(self.lines.len());
        let offset = since.saturating_sub(first).min(self.lines.len());
        ProgressReply {
            seq: self.seq,
            lines: self.lines.iter().skip(offset).cloned().collect(),
            active: self.active,
        }
    }
}

fn progress_store() -> &'static Mutex<ProgressStore> {
    static STORE: OnceLock<Mutex<ProgressStore>> = OnceLock::new();
    STORE.get_or_init(|| Mutex::new(ProgressStore::default()))
}

fn progress_push(line: impl Into<String>) {
    if let Ok(mut store) = progress_store().lock() {
        store.push(line.into());
    }
}

fn progress_lines(lines: &[String]) {
    for line in lines {
        progress_push(line.clone());
    }
}

struct ProgressJob;

impl ProgressJob {
    fn start() -> Self {
        if let Ok(mut store) = progress_store().lock() {
            store.lines.clear();
            store.active = true;
        }
        Self
    }
}

impl Drop for ProgressJob {
    fn drop(&mut self) {
        if let Ok(mut store) = progress_store().lock() {
            store.active = false;
        }
    }
}

async fn blocking_work<T, F>(track_progress: bool, work: F) -> Result<T, String>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, String> + Send + 'static,
{
    tauri::async_runtime::spawn_blocking(move || {
        let _work_guard = work_guard()?;
        let _progress_guard = track_progress.then(ProgressJob::start);
        work()
    })
    .await
    .map_err(|error| format!("Tác vụ nền bị lỗi: {error}"))?
}

#[tauri::command]
fn progress(since: usize) -> ProgressReply {
    progress_store()
        .lock()
        .map(|store| store.snapshot(since))
        .unwrap_or(ProgressReply {
            seq: 0,
            lines: Vec::new(),
            active: false,
        })
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

static FOREGROUND_FETCHING: AtomicUsize = AtomicUsize::new(0);

struct ForegroundGuard;
impl ForegroundGuard {
    fn new() -> Self {
        FOREGROUND_FETCHING.fetch_add(1, Ordering::SeqCst);
        Self
    }
}
impl Drop for ForegroundGuard {
    fn drop(&mut self) {
        FOREGROUND_FETCHING.fetch_sub(1, Ordering::SeqCst);
    }
}

static GAP_FLOOR_UNTIL: OnceLock<Mutex<Option<Instant>>> = OnceLock::new();

fn note_rate_limit() {
    let cell = GAP_FLOOR_UNTIL.get_or_init(|| Mutex::new(None));
    *cell.lock().unwrap() = Some(Instant::now() + RATE_LIMIT_COOLDOWN);
}

/// Gap to honour right now, letting a rate-limit penalty expire on its own.
fn effective_gap(gap: Duration) -> Duration {
    let cell = GAP_FLOOR_UNTIL.get_or_init(|| Mutex::new(None));
    let mut until = cell.lock().unwrap();
    match *until {
        Some(deadline) if Instant::now() < deadline => gap.max(BROWSE_GAP),
        Some(_) => {
            *until = None;
            gap
        }
        None => gap,
    }
}

fn rate_limit_gap(is_foreground: bool, gap: Duration) {
    loop {
        let current_gap = effective_gap(gap);
        if !is_foreground && FOREGROUND_FETCHING.load(Ordering::SeqCst) > 0 {
            thread::sleep(Duration::from_millis(100));
            continue;
        }
        let wait = {
            static LAST_HIT: OnceLock<Mutex<Option<Instant>>> = OnceLock::new();
            let mut last = LAST_HIT.get_or_init(|| Mutex::new(None)).lock().unwrap();
            if let Some(t) = *last {
                if let Some(w) = current_gap.checked_sub(t.elapsed()) {
                    Some(w)
                } else {
                    *last = Some(Instant::now());
                    None
                }
            } else {
                *last = Some(Instant::now());
                None
            }
        };
        match wait {
            Some(w) => {
                if !is_foreground {
                    let start = Instant::now();
                    while start.elapsed() < w {
                        if FOREGROUND_FETCHING.load(Ordering::SeqCst) > 0 {
                            break;
                        }
                        thread::sleep(
                            Duration::from_millis(50).min(w.saturating_sub(start.elapsed())),
                        );
                    }
                } else {
                    thread::sleep(w);
                }
            }
            None => break,
        }
    }
}

fn is_valid_workshop_html(html: &str) -> bool {
    html.contains("filedetails/?id=") && !html.to_lowercase().contains("too many requests")
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
    is_valid_workshop_html(&html).then_some(html)
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

fn fetch_page(url: &str, is_foreground: bool, gap: Duration) -> Result<String, Blocked> {
    rate_limit_gap(is_foreground, gap);

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
            } else if !is_valid_workshop_html(&html) {
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
        Err(why) => {
            if why == RATE_LIMITED {
                note_rate_limit();
            }
            cached(url, None).ok_or(Blocked(why))
        }
    }
}

static REVALIDATING: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();

fn spawn_revalidate(url: String) {
    let revalidating = REVALIDATING.get_or_init(|| Mutex::new(HashSet::new()));
    {
        let mut set = revalidating.lock().unwrap();
        if set.contains(&url) {
            return;
        }
        set.insert(url.clone());
    }
    thread::spawn(move || {
        let _ = fetch_page(&url, false, BROWSE_GAP);
        if let Some(set_lock) = REVALIDATING.get() {
            if let Ok(mut set) = set_lock.lock() {
                set.remove(&url);
            }
        }
    });
}

fn cached_listing(url: &str) -> Result<String, Blocked> {
    if let Some(html) = cached(url, Some(BROWSE_TTL)) {
        return Ok(html);
    }
    if let Some(html) = cached(url, None) {
        spawn_revalidate(url.to_string());
        return Ok(html);
    }
    let _guard = ForegroundGuard::new();
    fetch_page(url, true, BROWSE_GAP)
}

fn cached_page(url: &str, ttl: Duration, gap: Duration) -> Result<String, Blocked> {
    if let Some(html) = cached(url, Some(ttl)) {
        return Ok(html);
    }
    let _guard = ForegroundGuard::new();
    // Steam chặn -> dùng lại bản cache cũ thay vì bó tay. Mục "Required Items"
    // của một mod gần như không đổi, nên bản cũ vẫn dò được phụ thuộc; hết cách
    // mới trả lỗi.
    fetch_page(url, true, gap).or_else(|blocked| cached(url, None).ok_or(blocked))
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

/// Steam pins Spiffo's Workshop "Modding Policy" notice to the top of every
/// listing, under every sort, with zero subscribers. It is not a mod and cannot
/// be installed, so it never reaches the grid.
const PINNED: &[&str] = &["2872282653"];

fn extract_listing_ids(html: &str) -> Vec<String> {
    ids_after(html, "sharedfiles/filedetails/?id=")
        .into_iter()
        .filter(|id| !PINNED.contains(&id.as_str()))
        .collect()
}

fn required_ids(id: &str) -> Result<Vec<String>, Blocked> {
    let html = cached_page(&format!("{ITEM_URL}{id}"), REQUIRES_TTL, ITEM_GAP)?;
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

fn required_ids_background(id: &str) -> Result<Vec<String>, Blocked> {
    let url = format!("{ITEM_URL}{id}");
    if cached(&url, Some(REQUIRES_TTL)).is_none() {
        fetch_page(&url, false, ITEM_GAP)?;
    }
    required_ids(id)
}

static PREFETCH_STATE: OnceLock<(Mutex<VecDeque<String>>, Condvar)> = OnceLock::new();
static PREFETCH_WORKER_INIT: OnceLock<()> = OnceLock::new();

fn queue_prefetch(ids: Vec<String>) {
    let pair = PREFETCH_STATE.get_or_init(|| (Mutex::new(VecDeque::new()), Condvar::new()));
    {
        let mut queue = pair.0.lock().unwrap();
        for id in ids {
            let id = id.trim();
            if !id.is_empty()
                && id.bytes().all(|b| b.is_ascii_digit())
                && !queue.contains(&id.to_string())
            {
                queue.push_back(id.to_string());
            }
        }
    }
    pair.1.notify_one();

    PREFETCH_WORKER_INIT.get_or_init(|| {
        thread::spawn(|| {
            let pair = PREFETCH_STATE.get_or_init(|| (Mutex::new(VecDeque::new()), Condvar::new()));
            loop {
                let id = {
                    let mut queue = pair.0.lock().unwrap();
                    while queue.is_empty() {
                        queue = pair.1.wait(queue).unwrap();
                    }
                    queue.pop_front()
                };
                if let Some(id) = id {
                    let _ = required_ids_background(&id);
                }
            }
        });
    });
}

#[tauri::command]
fn prefetch(ids: Vec<String>) -> Result<Value, String> {
    queue_prefetch(ids);
    Ok(json!({ "ok": true }))
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

fn steamcmd_args(ids: &[String]) -> Vec<OsString> {
    let mut args = vec![OsString::from("+login"), OsString::from("anonymous")];
    for id in ids {
        args.extend([
            OsString::from("+workshop_download_item"),
            OsString::from(APPID),
            OsString::from(id),
        ]);
    }
    args.push(OsString::from("+quit"));
    args
}

fn steamcmd_success_id(line: &str) -> Option<&str> {
    line.split_once("Success. Downloaded item ")?
        .1
        .split_whitespace()
        .next()
}

fn download_items(ids: &[String], force: bool) -> Result<HashMap<String, PathBuf>, String> {
    download_items_labeled(ids, force, None)
}

fn download_items_labeled(
    ids: &[String],
    force: bool,
    labels: Option<&HashMap<String, String>>,
) -> Result<HashMap<String, PathBuf>, String> {
    if ids
        .iter()
        .any(|id| id.is_empty() || !id.bytes().all(|byte| byte.is_ascii_digit()))
    {
        return Err("Workshop id không hợp lệ".into());
    }
    let executable = steamcmd_exe()?;
    let workshop = executable
        .parent()
        .ok_or("steamcmd.exe không có thư mục cha")?
        .join("steamapps")
        .join("workshop");
    if force {
        for id in ids {
            let destination = workshop.join("content").join(APPID).join(id);
            if destination.is_dir() {
                fs::remove_dir_all(&destination)
                    .map_err(|e| format!("Không dọn được cache {}: {e}", destination.display()))?;
            }
        }
        let manifest = workshop.join(format!("appworkshop_{APPID}.acf"));
        if manifest.is_file() {
            fs::remove_file(&manifest)
                .map_err(|e| format!("Không dọn được {}: {e}", manifest.display()))?;
        }
    }
    let mut child = Command::new(&executable)
        .args(steamcmd_args(ids))
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|e| format!("Không chạy được steamcmd: {e}"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or("Không đọc được stdout steamcmd")?;
    let requested: HashSet<_> = ids.iter().map(String::as_str).collect();
    let mut completed = HashSet::new();
    let mut tail = VecDeque::with_capacity(4);
    for line in BufReader::new(stdout).lines() {
        let line = line.map_err(|e| format!("Không đọc được stdout steamcmd: {e}"))?;
        if let Some(id) = steamcmd_success_id(&line).filter(|id| requested.contains(id)) {
            let fresh = completed.insert(id.to_string());
            eprintln!("steamcmd: tải xong {id}");
            if fresh {
                progress_push(format!("steamcmd: tải xong {id}"));
                let label = labels
                    .and_then(|labels| labels.get(id))
                    .map(String::as_str)
                    .unwrap_or(id);
                progress_push(format!(
                    "tải xong {label} ({}/{})",
                    completed.len(),
                    ids.len()
                ));
            }
        }
        if !line.trim().is_empty() {
            if tail.len() == 4 {
                tail.pop_front();
            }
            tail.push_back(line);
        }
    }
    child
        .wait()
        .map_err(|e| format!("Không chờ được steamcmd: {e}"))?;

    let mut downloaded = HashMap::new();
    for id in ids {
        let destination = workshop.join("content").join(APPID).join(id);
        if (completed.contains(id) || destination.is_dir()) && destination.is_dir() {
            downloaded.insert(id.clone(), destination);
        } else {
            progress_push(format!("! {id}: steamcmd không tải được mục này"));
        }
    }
    if downloaded.is_empty() && !ids.is_empty() {
        let line = format!(
            "! steamcmd không tải được mục nào: {}",
            tail.into_iter().collect::<Vec<_>>().join(" | ")
        );
        eprintln!("{line}");
        progress_push(line);
    }
    Ok(downloaded)
}

fn download_item(id: &str, force: bool) -> Result<PathBuf, String> {
    download_item_labeled(id, force, None)
}

fn download_item_labeled(id: &str, force: bool, label: Option<String>) -> Result<PathBuf, String> {
    let label = label
        .filter(|label| !label.is_empty())
        .unwrap_or_else(|| id.to_string());
    let labels = HashMap::from([(id.to_string(), label)]);
    download_items_labeled(&[id.to_string()], force, Some(&labels))?
        .remove(id)
        .ok_or_else(|| format!("steamcmd thất bại với {id}"))
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
        let folder = match entry.file_name().into_string() {
            Ok(folder) => folder,
            Err(_) => {
                let warning = format!(
                    "! bỏ qua thư mục mod có tên không phải Unicode hợp lệ: {}",
                    entry.path().display()
                );
                eprintln!("{warning}");
                progress_push(warning);
                continue;
            }
        };
        if let Err(error) = check_folder(&folder) {
            let warning = format!("! bỏ qua thư mục mod {}: {error}", entry.path().display());
            eprintln!("{warning}");
            progress_push(warning);
            continue;
        }
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

fn source_metadata(source: &Path) -> Result<(Vec<String>, Vec<String>), String> {
    mod_metadata(&mod_roots(source)?)
}

fn source_provides_modid(source: &Path, wanted: &str) -> Result<bool, String> {
    Ok(source_metadata(source)?
        .0
        .iter()
        .any(|modid| modid == wanted))
}

fn matching_candidate(
    wanted: &str,
    candidates: &[String],
    sources: &HashMap<String, PathBuf>,
) -> Result<Option<String>, String> {
    for id in candidates.iter().take(3) {
        if sources
            .get(id)
            .is_some_and(|source| source_provides_modid(source, wanted).unwrap_or(false))
        {
            return Ok(Some(id.clone()));
        }
    }
    Ok(None)
}

type ResolutionCache = HashMap<String, Option<String>>;

fn resolution_cache(state: &Map<String, Value>) -> ResolutionCache {
    let mut cache = HashMap::new();
    for resolved in state
        .values()
        .filter_map(|entry| entry.get("resolved").and_then(Value::as_object))
    {
        for (modid, value) in resolved {
            let key = normalize_required(modid).to_lowercase();
            if key.is_empty() {
                continue;
            }
            if value.is_null() {
                cache.entry(key).or_insert(None);
            } else if let Some(id) = value
                .as_str()
                .filter(|id| !id.is_empty() && id.bytes().all(|byte| byte.is_ascii_digit()))
            {
                cache.insert(key, Some(id.to_string()));
            }
        }
    }
    cache
}

fn persist_resolutions(user: &Path, cache: &ResolutionCache) -> Result<(), String> {
    let mut state = managed_state(user);
    let mut changed = false;
    for entry in state.values_mut() {
        if !entry.is_object() {
            continue;
        }
        let mut resolved = Map::new();
        for modid in entry
            .get("require")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
        {
            if let Some(id) = cache.get(&normalize_required(modid).to_lowercase()) {
                resolved.insert(
                    modid.to_string(),
                    id.as_ref()
                        .map_or(Value::Null, |id| Value::String(id.clone())),
                );
            }
        }
        let value = Value::Object(resolved);
        if entry.get("resolved") != Some(&value) {
            entry
                .as_object_mut()
                .unwrap()
                .insert("resolved".into(), value);
            changed = true;
        }
    }
    if changed {
        write_managed_state(user, &state)?;
    }
    Ok(())
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
        let html = cached_listing(&(BROWSE_URL.to_string() + &form_body(&fields)))
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
    // A stray file of the same name must not read as an installed mod.
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
    /// Native routes advertised to the bridge; the UI greys out anything omitted.
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
                "resolved": entry.get("resolved").cloned().unwrap_or_else(|| json!({})),
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
            "update", "prefetch", "progress",
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
    // A LIST, not a string: ui.html calls .map() on command logs. One string
    // here makes showLog() throw after a command that actually succeeded.
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
        let done = Done {
            ok: true,
            log: vec![format!("'{folder}' đã {verb} sẵn.")],
        };
        progress_lines(&done.log);
        return Ok(done);
    }
    if !is_dir(&src) {
        return Err(format!(
            "Không tìm thấy thư mục mod '{folder}' trong mods/ hoặc mods_off/"
        ));
    }
    fs::rename(&src, &dst).map_err(|e| format!("Không {verb} được '{folder}': {e}"))?;
    let done = Done {
        ok: true,
        log: vec![format!("Đã {verb} '{folder}'.")],
    };
    progress_lines(&done.log);
    Ok(done)
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
                "resolved": previous.get("resolved").cloned().unwrap_or_else(|| json!({})),
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
    install_one_with_source(id, detail, force, user, None, false)
}

fn install_one_with_source(
    id: &str,
    detail: &Value,
    force: bool,
    user: &Path,
    source: Option<&Path>,
    download_attempted: bool,
) -> Result<Vec<String>, String> {
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
    let downloaded;
    let source = match source {
        Some(source) => source,
        None if download_attempted => return Err("steamcmd không tải được mục này".into()),
        None => {
            let title = value_string(detail, "title");
            downloaded = if title.is_empty() {
                download_item(id, previous.is_some() || force)?
            } else {
                download_item_labeled(id, previous.is_some() || force, Some(title))?
            };
            &downloaded
        }
    };
    install_from_source(id, detail, source, user)
}

fn dedup_ids(ids: &[String]) -> Vec<String> {
    let mut seen = HashSet::new();
    ids.iter()
        .filter(|id| seen.insert((*id).clone()))
        .cloned()
        .collect()
}

/// Scrape one whole level of Required Items at once. A blocked id drops out of
/// the map instead of sinking the batch, same as the single-page path.
fn required_ids_many(ids: &[String], log: &mut Vec<String>) -> HashMap<String, Vec<String>> {
    let mut out = HashMap::new();
    for chunk in ids.chunks(ITEM_WORKERS) {
        let handles: Vec<_> = chunk
            .iter()
            .map(|id| {
                let id = id.clone();
                thread::spawn(move || {
                    let found = required_ids(&id);
                    (id, found)
                })
            })
            .collect();
        for handle in handles {
            let Ok((id, found)) = handle.join() else {
                continue;
            };
            match found {
                Ok(required) => {
                    out.insert(id, required);
                }
                Err(error) => log.push(format!(
                    "~ {id}: chưa kiểm được mod bắt buộc ({error}) - vẫn cài tiếp"
                )),
            }
        }
    }
    out
}

fn post_order(
    id: &str,
    graph: &HashMap<String, Vec<String>>,
    seen: &mut HashSet<String>,
    order: &mut Vec<String>,
) {
    if !seen.insert(id.to_string()) {
        return;
    }
    for dependency in graph.get(id).map(Vec::as_slice).unwrap_or(&[]) {
        post_order(dependency, graph, seen, order);
    }
    order.push(id.to_string());
}

/// Workshop ids in dependency-first post-order, scraped level by level rather
/// than depth-first so each level goes out in parallel.
fn dependency_order(ids: &[String], depth: usize, log: &mut Vec<String>) -> Vec<String> {
    let ids = dedup_ids(ids);
    let mut graph: HashMap<String, Vec<String>> = HashMap::new();
    let mut level = ids.clone();
    let mut left = depth;
    while !level.is_empty() && left > 0 {
        let batch: Vec<String> = level
            .iter()
            .filter(|id| !graph.contains_key(*id))
            .cloned()
            .collect();
        if batch.is_empty() {
            break;
        }
        let found = required_ids_many(&batch, log);
        let mut next = Vec::new();
        for id in &batch {
            let required = found.get(id).cloned().unwrap_or_default();
            next.extend(
                required
                    .iter()
                    .filter(|dependency| !graph.contains_key(*dependency))
                    .cloned(),
            );
            graph.insert(id.clone(), required);
        }
        level = dedup_ids(&next);
        left -= 1;
    }
    let mut seen = HashSet::new();
    let mut order = Vec::new();
    for id in &ids {
        post_order(id, &graph, &mut seen, &mut order);
    }
    order
}

fn search_ids(query: &str) -> Result<Vec<String>, String> {
    let fields = vec![
        ("appid".into(), APPID.into()),
        ("section".into(), "readytouseitems".into()),
        ("browsesort".into(), "textsearch".into()),
        ("p".into(), "1".into()),
        ("searchtext".into(), query.into()),
    ];
    let html = cached_listing(&(BROWSE_URL.to_string() + &form_body(&fields)))
        .map_err(|error| error.to_string())?;
    Ok(extract_listing_ids(&html).into_iter().take(3).collect())
}

fn search_modid_candidates(modid: &str) -> Result<Vec<String>, String> {
    let spaced = modid.replace('_', " ");
    let candidates = search_ids(&spaced)?;
    if candidates.is_empty() && spaced != modid {
        search_ids(modid)
    } else {
        Ok(candidates)
    }
}

/// Serve items already sitting in the steamcmd content folder without starting
/// steamcmd at all - a session costs ~5 s before it downloads a single byte.
/// Only for callers that just read mod.info; the copy an install lands comes
/// from a normal download.
fn download_items_reuse(ids: &[String]) -> Result<HashMap<String, PathBuf>, String> {
    let executable = steamcmd_exe()?;
    let content = executable
        .parent()
        .ok_or("steamcmd.exe không có thư mục cha")?
        .join("steamapps")
        .join("workshop")
        .join("content")
        .join(APPID);
    let mut have = HashMap::new();
    let mut missing = Vec::new();
    for id in dedup_ids(ids) {
        let destination = content.join(&id);
        if destination.is_dir() {
            have.insert(id, destination);
        } else {
            missing.push(id);
        }
    }
    if !missing.is_empty() {
        have.extend(download_items(&missing, false)?);
    }
    Ok(have)
}

/// Resolve several mod ids at once, sharing steamcmd sessions: two for the whole
/// batch instead of two per mod id. Returned in input order.
fn resolve_modids(
    modids: &[String],
    cache: &mut ResolutionCache,
) -> Result<Vec<(String, String, PathBuf)>, String> {
    let mut wanted: Vec<(String, String)> = Vec::new();
    for modid in modids {
        let key = normalize_required(modid).to_lowercase();
        if !key.is_empty() && !wanted.iter().any(|(seen, _)| seen == &key) {
            wanted.push((key, modid.clone()));
        }
    }
    if wanted.is_empty() {
        return Ok(Vec::new());
    }
    progress_push(format!(
        "đang dò mod bắt buộc: {}",
        wanted
            .iter()
            .map(|(_, modid)| modid.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    ));

    let remembered: Vec<String> = wanted
        .iter()
        .filter_map(|(key, _)| cache.get(key).cloned().flatten())
        .collect();
    let verified = if remembered.is_empty() {
        HashMap::new()
    } else {
        download_items_reuse(&remembered)?
    };

    let mut found: Vec<(String, String, PathBuf)> = Vec::new();
    let mut unresolved: Vec<(String, String)> = Vec::new();
    for (key, modid) in &wanted {
        match cache.get(key) {
            Some(None) => {
                progress_push(format!("không tìm thấy workshop id cho {modid}"));
                continue;
            }
            Some(Some(id)) => {
                let id = id.clone();
                if verified
                    .get(&id)
                    .is_some_and(|source| source_provides_modid(source, modid).unwrap_or(false))
                {
                    progress_push(format!("đã tìm thấy {modid} ({id})"));
                    found.push((modid.clone(), id.clone(), verified[&id].clone()));
                    continue;
                }
            }
            None => {}
        }
        cache.remove(key);
        unresolved.push((key.clone(), modid.clone()));
    }

    if !unresolved.is_empty() {
        let mut searched = Vec::new();
        for (key, modid) in unresolved {
            let candidates = search_modid_candidates(&modid)?;
            searched.push((key, modid, candidates));
        }
        let pool: Vec<String> = searched
            .iter()
            .flat_map(|(_, _, candidates)| candidates.iter().cloned())
            .collect();
        let sources = if pool.is_empty() {
            HashMap::new()
        } else {
            download_items_reuse(&pool)?
        };
        for (key, modid, candidates) in searched {
            let matched = matching_candidate(&modid, &candidates, &sources)?;
            cache.insert(key, matched.clone());
            match matched.and_then(|id| sources.get(&id).map(|source| (id, source.clone()))) {
                Some((id, source)) => {
                    progress_push(format!("đã tìm thấy {modid} ({id})"));
                    found.push((modid, id, source));
                }
                None => progress_push(format!("không tìm thấy workshop id cho {modid}")),
            }
        }
    }

    let mut ordered = Vec::new();
    for (_, modid) in &wanted {
        if let Some(index) = found.iter().position(|(name, _, _)| name == modid) {
            ordered.push(found.remove(index));
        }
    }
    Ok(ordered)
}

fn index_sources(sources: &HashMap<String, PathBuf>, providers: &mut HashMap<String, String>) {
    for (id, source) in sources {
        if let Ok((modids, _)) = source_metadata(source) {
            for modid in modids {
                providers.entry(modid.to_lowercase()).or_insert(id.clone());
            }
        }
    }
}

fn installed_providers(user: &Path, state: &Map<String, Value>) -> HashMap<String, String> {
    let mods = user.join("mods");
    let off = user.join("mods_off");
    let mut providers = HashMap::new();
    for (id, entry) in state {
        if !entry_complete(entry, &mods, &off) {
            continue;
        }
        for modid in entry
            .get("modids")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
        {
            providers.entry(modid.to_lowercase()).or_insert(id.clone());
        }
    }
    providers
}

#[allow(clippy::too_many_arguments)]
fn walk_mod_dependencies(
    id: &str,
    depth: usize,
    sources: &mut HashMap<String, PathBuf>,
    providers: &mut HashMap<String, String>,
    cache: &mut ResolutionCache,
    seen: &mut HashSet<String>,
    order: &mut Vec<String>,
    log: &mut Vec<String>,
) -> Result<(), String> {
    if !seen.insert(id.to_string()) {
        return Ok(());
    }
    let required = match sources.get(id).map(|source| source_metadata(source)) {
        Some(Ok((_, required))) => required,
        Some(Err(error)) => {
            log.push(format!("! {id}: không đọc được mod.info ({error})"));
            Vec::new()
        }
        None => Vec::new(),
    };
    let mut pending = Vec::new();
    for modid in required {
        let key = modid.to_lowercase();
        if let Some(provider) = providers.get(&key).cloned() {
            cache.insert(key, Some(provider.clone()));
            if sources.contains_key(&provider) {
                walk_mod_dependencies(
                    &provider,
                    depth.saturating_sub(1),
                    sources,
                    providers,
                    cache,
                    seen,
                    order,
                    log,
                )?;
            }
            continue;
        }
        if depth > 0 {
            pending.push(modid);
        }
    }
    // every unknown mod id of this node resolves in one pass, so the whole node
    // costs two steamcmd sessions instead of two per mod id
    if !pending.is_empty() {
        let resolved = match resolve_modids(&pending, cache) {
            Ok(resolved) => resolved,
            Err(error) => {
                log.push(format!(
                    "! {}: không dò được workshop id ({error})",
                    pending.join(", ")
                ));
                Vec::new()
            }
        };
        if !resolved.is_empty() {
            // the resolver may have read a stale copy off disk just to see which
            // mod ids it provides; the install copy comes from the batched
            // download below, never from that
            let added: Vec<String> = resolved.into_iter().map(|(_, id, _)| id).collect();
            let titles = details(&added).unwrap_or_default();
            for provider in &added {
                let title = titles
                    .get(provider)
                    .map(|item| value_string(item, "title"))
                    .filter(|title| !title.is_empty())
                    .unwrap_or_else(|| provider.clone());
                log.push(format!("kèm mod bắt buộc: {title} ({provider})"));
            }
            let workshop_order = dependency_order(&added, depth - 1, log);
            let needed: Vec<_> = workshop_order
                .iter()
                .filter(|item| !sources.contains_key(*item))
                .cloned()
                .collect();
            if !needed.is_empty() {
                sources.extend(download_items(&needed, false)?);
                index_sources(sources, providers);
            }
            for item in workshop_order {
                walk_mod_dependencies(
                    &item,
                    depth - 1,
                    sources,
                    providers,
                    cache,
                    seen,
                    order,
                    log,
                )?;
            }
        }
    }
    order.push(id.to_string());
    Ok(())
}

fn expand_mod_dependencies(
    initial: Vec<String>,
    user: &Path,
    sources: &mut HashMap<String, PathBuf>,
    log: &mut Vec<String>,
) -> Result<(Vec<String>, ResolutionCache), String> {
    let state = managed_state(user);
    let mut cache = resolution_cache(&state);
    let mut providers = installed_providers(user, &state);
    index_sources(sources, &mut providers);
    let mut seen = HashSet::new();
    let mut order = Vec::new();
    for id in initial {
        walk_mod_dependencies(
            &id,
            4,
            sources,
            &mut providers,
            &mut cache,
            &mut seen,
            &mut order,
            log,
        )?;
    }
    Ok((order, cache))
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
        let line = format!(
            "collection {}: {} mod",
            original_meta
                .get(id)
                .map(|value| value_string(value, "title"))
                .filter(|title| !title.is_empty())
                .unwrap_or_else(|| id.to_string()),
            todo.len()
        );
        log.push(line);
    }
    let requested: HashSet<_> = todo.iter().cloned().collect();
    let order = dependency_order(&todo, 4, &mut log);
    progress_lines(&log);
    let extra: Vec<_> = order
        .iter()
        .filter(|item| !requested.contains(*item))
        .cloned()
        .collect();
    if !extra.is_empty() {
        let names = details(&extra)?;
        let line = format!(
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
        );
        progress_push(line.clone());
        log.push(line);
    }

    let state = managed_state(user);
    let refresh_cache = force || order.iter().any(|item| state.contains_key(item));
    let mut metadata = details(&order)?;
    let labels: HashMap<_, _> = order
        .iter()
        .map(|item| {
            let title = metadata
                .get(item)
                .map(|detail| value_string(detail, "title"))
                .filter(|title| !title.is_empty())
                .unwrap_or_else(|| item.clone());
            (item.clone(), title)
        })
        .collect();
    let mut downloaded = download_items_labeled(&order, refresh_cache, Some(&labels))?;
    let logged = log.len();
    let (order, resolutions) = expand_mod_dependencies(order, user, &mut downloaded, &mut log)?;
    progress_lines(&log[logged..]);
    let missing_metadata: Vec<_> = order
        .iter()
        .filter(|item| !metadata.contains_key(*item))
        .cloned()
        .collect();
    if !missing_metadata.is_empty() {
        metadata.extend(details(&missing_metadata)?);
    }
    let mut ok = true;
    for item in order {
        match metadata.get(&item) {
            Some(detail) => match install_one_with_source(
                &item,
                detail,
                force,
                user,
                downloaded.get(&item).map(PathBuf::as_path),
                true,
            ) {
                Ok(lines) => {
                    progress_lines(&lines);
                    log.extend(lines);
                }
                Err(error) => {
                    ok = false;
                    let line = format!("! {item}: {error}");
                    progress_push(line.clone());
                    log.push(line);
                }
            },
            None => {
                ok = false;
                let line = format!("! {item}: mod đã bị xoá hoặc đặt riêng tư");
                progress_push(line.clone());
                log.push(line);
            }
        }
    }
    if let Err(error) = persist_resolutions(user, &resolutions) {
        ok = false;
        let line = format!("! không lưu được ánh xạ mod id ({error})");
        progress_push(line.clone());
        log.push(line);
    }
    let state = managed_state(user);
    let missing = missing_modids(&state, &user.join("mods"), &user.join("mods_off"));
    if !missing.is_empty() {
        ok = false;
        let line = format!("! thiếu mod bắt buộc: {}", missing.join(", "));
        progress_push(line.clone());
        log.push(line);
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
    progress_lines(&log);
    Ok(Done { ok, log })
}

fn update_internal(user: &Path) -> Result<Done, String> {
    let state = managed_state(user);
    if state.is_empty() {
        progress_push("không có mod để cập nhật");
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
                let line = format!("! {title}: đã biến mất khỏi workshop, giữ nguyên bản cục bộ");
                progress_push(line.clone());
                log.push(line);
            }
            Some(detail)
                if value_u64(detail, "time_updated") == value_u64(entry, "updated")
                    && entry_complete(entry, &user.join("mods"), &user.join("mods_off")) =>
            {
                let line = format!("= {title}");
                progress_push(line.clone());
                log.push(line);
            }
            Some(detail) => match install_one(&id, detail, true, user) {
                Ok(lines) => {
                    progress_lines(&lines);
                    log.extend(lines);
                }
                Err(error) => {
                    ok = false;
                    let line = format!("! {title}: {error}");
                    progress_push(line.clone());
                    log.push(line);
                }
            },
        }
    }
    let refreshed = managed_state(user);
    let missing = missing_modids(&refreshed, &user.join("mods"), &user.join("mods_off"));
    if !missing.is_empty() {
        ok = false;
        let line = format!("! thiếu mod bắt buộc: {}", missing.join(", "));
        progress_push(line.clone());
        log.push(line);
    }
    if log.iter().any(|line| line.trim_start().starts_with('!')) {
        ok = false;
    }
    Ok(Done { ok, log })
}

/// Start the game through the launcher .bat sitting next to the install.
///
/// `PZ-D.bat` self-elevates: steam.exe carries a `RUNASADMIN` compat flag, so a
/// normal-integrity process cannot `OpenProcess` it, `SteamAPI_IsSteamRunning`
/// comes back false and the OnlineFix layer pops "Steam is not launched". The
/// spawn returns as soon as cmd hands the shim off — the UAC prompt and the game
/// both outlive this call, so it never blocks the work lock.
#[tauri::command]
fn launch(nosteam: Option<bool>) -> Result<Value, String> {
    let (game, _, _, _) = paths();
    let name = if nosteam.unwrap_or(false) {
        "PZ-D-nosteam.bat"
    } else {
        "PZ-D.bat"
    };
    let bat = game.join(name);
    if !bat.is_file() {
        return Err(format!("không thấy {name} trong {}", game.display()));
    }
    Command::new("cmd")
        .args(["/c", "start", "", &bat.to_string_lossy()])
        .current_dir(&game)
        .spawn()
        .map_err(|e| format!("không chạy được {name}: {e}"))?;
    Ok(json!({ "ok": true, "log": format!("đang mở game bằng {name}") }))
}

#[tauri::command]
async fn install(id: String, force: Option<bool>) -> Result<Done, String> {
    let (_, user, _, _) = paths();
    blocking_work(true, move || {
        progress_push(format!("đang cài {id}"));
        let result = install_internal(&id, force.unwrap_or(false), &user);
        if let Err(error) = &result {
            progress_push(format!("! {id}: {error}"));
        }
        result
    })
    .await
}

#[tauri::command]
async fn remove(id: String) -> Result<Done, String> {
    let (_, user, _, _) = paths();
    blocking_work(true, move || {
        progress_push(format!("đang gỡ {id}"));
        let result = remove_internal(&id, &user);
        if let Err(error) = &result {
            progress_push(format!("! {id}: {error}"));
        }
        result
    })
    .await
}

#[tauri::command]
async fn update() -> Result<Done, String> {
    let (_, user, _, _) = paths();
    blocking_work(true, move || {
        progress_push("đang cập nhật tất cả");
        let result = update_internal(&user);
        if let Err(error) = &result {
            progress_push(format!("! cập nhật thất bại: {error}"));
        }
        result
    })
    .await
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
                    return Err("Đang có phiên bisect hoạt động. Hãy bấm 'Dừng' trong tab Dò mod lỗi trước khi bắt đầu phiên mới.".into());
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
async fn bisect(op: Option<String>, names: Option<Vec<String>>) -> Result<Value, String> {
    let (_, user, _, _) = paths();
    let action = op.as_deref().unwrap_or("view").to_string();
    let track = !matches!(action.as_str(), "view" | "");
    blocking_work(track, move || {
        if track {
            progress_push(format!("bisect: {action}"));
        }
        let result = match action.as_str() {
            "view" | "" => bisect_view_internal(&user),
            "start" => bisect_start_internal(names, &user)
                .map(|state| json!({ "ok": true, "state": state })),
            "bad" => {
                bisect_mark_internal(true, &user).map(|state| json!({ "ok": true, "state": state }))
            }
            "good" => bisect_mark_internal(false, &user)
                .map(|state| json!({ "ok": true, "state": state })),
            "stop" => bisect_stop_internal(&user)
                .and_then(|_| bisect_view_internal(&user))
                .map(|state| json!({ "ok": true, "state": state })),
            other => Err(format!("Lệnh bisect không hợp lệ: {other}")),
        };
        if track {
            match &result {
                Ok(_) => progress_push(format!("bisect {action}: xong")),
                Err(error) => progress_push(format!("! bisect {action}: {error}")),
            }
        }
        result
    })
    .await
}

fn main() {
    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![
            state, enable, disable, browse, detail, bisect, install, remove, update, prefetch,
            progress, launch
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
    fn steamcmd_argv_batches_requested_items() {
        let args: Vec<_> = steamcmd_args(&["123".into(), "456".into()])
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
                "+workshop_download_item",
                "108600",
                "456",
                "+quit"
            ]
        );
        assert_eq!(
            steamcmd_success_id("Success. Downloaded item 456 to x"),
            Some("456")
        );
        assert_eq!(steamcmd_success_id("ERROR! Download item 456 failed"), None);
    }

    #[test]
    fn pz_user_rank_prefers_the_folder_the_game_wrote() {
        let root = test_root("user-rank");
        let game_written = root.join("real");
        let managed = root.join("cachedir");
        let fresh = root.join("made-up");
        fs::create_dir_all(game_written.join("mods")).unwrap();
        fs::write(game_written.join("options.ini"), "x").unwrap();
        fs::create_dir_all(managed.join("mods").join("SomeMod")).unwrap();
        // What ensure_dirs() leaves behind: mods/ and mods_off/, both empty.
        fs::create_dir_all(fresh.join("mods")).unwrap();
        fs::create_dir_all(fresh.join("mods_off")).unwrap();

        assert_eq!(pz_user_rank(&game_written), 2);
        assert!(pz_user_touched(&game_written) > pz_user_touched(&managed));
        assert_eq!(pz_user_rank(&managed), 1);
        assert_eq!(pz_user_rank(&fresh), 0);
        assert_eq!(pz_user_rank(&root.join("nope")), 0);
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn progress_store_is_capped_and_incremental() {
        let mut store = ProgressStore {
            active: true,
            ..ProgressStore::default()
        };
        for index in 0..505 {
            store.push(format!("line {index}"));
        }
        let all = store.snapshot(0);
        assert_eq!(all.seq, 505);
        assert_eq!(all.lines.len(), PROGRESS_CAP);
        assert_eq!(all.lines[0], "line 5");
        assert!(all.active);
        assert_eq!(store.snapshot(504).lines, ["line 504"]);
    }

    #[test]
    fn dependencies_come_out_deepest_first_without_revisiting() {
        let graph: HashMap<String, Vec<String>> = [
            ("1", vec!["2", "3"]),
            ("2", vec!["4"]),
            ("3", vec!["4"]),
            ("4", vec![]),
        ]
        .into_iter()
        .map(|(id, deps)| {
            (
                id.to_string(),
                deps.into_iter().map(str::to_string).collect(),
            )
        })
        .collect();
        let mut seen = HashSet::new();
        let mut order = Vec::new();
        post_order("1", &graph, &mut seen, &mut order);
        assert_eq!(order, ["4", "2", "3", "1"]);
        assert_eq!(dedup_ids(&["1".into(), "2".into(), "1".into()]), ["1", "2"]);
    }

    #[test]
    fn a_rate_limit_slows_every_page_down_then_expires() {
        let cell = GAP_FLOOR_UNTIL.get_or_init(|| Mutex::new(None));
        let keep = *cell.lock().unwrap();
        note_rate_limit();
        assert_eq!(effective_gap(ITEM_GAP), BROWSE_GAP);
        *cell.lock().unwrap() = Some(Instant::now() - Duration::from_secs(1));
        assert_eq!(effective_gap(ITEM_GAP), ITEM_GAP);
        assert!(cell.lock().unwrap().is_none(), "penalty must clear itself");
        *cell.lock().unwrap() = keep;
    }

    #[test]
    fn modid_resolver_checks_only_three_exact_candidates() {
        let root = test_root("modid-resolver");
        let html = ["11", "22", "33", "44"]
            .map(|id| format!("<a href=\"sharedfiles/filedetails/?id={id}\">x</a>"))
            .join("");
        let candidates = extract_listing_ids(&html);
        let mut sources = HashMap::new();
        for (id, modid) in [
            ("11", "Wrong"),
            ("22", "NeatUI_Framework"),
            ("33", "AlsoWrong"),
            ("44", "OnlyFourth"),
        ] {
            let source = root.join(id);
            let folder = source.join("mods").join("Fixture");
            fs::create_dir_all(&folder).unwrap();
            fs::write(folder.join("mod.info"), format!("id={modid}\n")).unwrap();
            sources.insert(id.to_string(), source);
        }
        assert_eq!(
            matching_candidate("NeatUI_Framework", &candidates, &sources).unwrap(),
            Some("22".into())
        );
        assert_eq!(
            matching_candidate("neatui_framework", &candidates, &sources).unwrap(),
            None,
            "mod.info id matching must be exact"
        );
        assert_eq!(
            matching_candidate("OnlyFourth", &candidates, &sources).unwrap(),
            None,
            "rank four must never be installed"
        );
        let equipment = root.join("55");
        let folder = equipment.join("mods").join("Equipment");
        fs::create_dir_all(&folder).unwrap();
        fs::write(
            folder.join("mod.info"),
            "id=Equipment\nrequire=NeatUI_Framework\n",
        )
        .unwrap();
        sources.insert("55".into(), equipment);
        let (order, cache) =
            expand_mod_dependencies(vec!["55".into()], &root, &mut sources, &mut Vec::new())
                .unwrap();
        assert_eq!(order, ["22", "55"]);
        assert_eq!(cache["neatui_framework"], Some("22".into()));
        fs::remove_dir_all(root).ok();
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
        persist_resolutions(&user, &HashMap::from([("missingid".into(), None)])).unwrap();

        let snapshot = state_internal(Path::new(r"D:\ProjectZomboid"), &user).unwrap();
        assert_eq!(snapshot.installed["10"]["title"], "Owned item");
        assert_eq!(snapshot.installed["10"]["folders"][0]["status"], "enabled");
        assert_eq!(snapshot.loose.len(), 1);
        assert_eq!(snapshot.loose[0].name, "Loose");
        assert_eq!(snapshot.missing, ["MissingID"]);
        assert!(snapshot.installed["10"]["resolved"]["MissingID"].is_null());
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
    fn the_pinned_policy_notice_never_reaches_the_grid() {
        let html = r#"<a href="sharedfiles/filedetails/?id=2872282653">policy</a>
            <a href="sharedfiles/filedetails/?id=111">a real mod</a>"#;
        assert_eq!(extract_listing_ids(html), ["111"]);
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
    fn mod_roots_skips_invalid_folder_without_aborting_the_item() {
        let item = test_root("invalid-workshop-folder");
        let good = item.join("mods").join("GoodMod");
        let bad = item.join("mods").join("Bad\u{FF0F}Mod");
        fs::create_dir_all(&good).unwrap();
        fs::create_dir_all(&bad).unwrap();
        fs::write(good.join("mod.info"), "id=GoodMod\n").unwrap();
        fs::write(bad.join("mod.info"), "id=BadMod\n").unwrap();

        let roots = mod_roots(&item).unwrap();
        assert_eq!(
            roots
                .iter()
                .map(|root| root.folder.as_str())
                .collect::<Vec<_>>(),
            ["GoodMod"]
        );
        fs::remove_dir_all(&item).ok();
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

    #[test]
    fn cached_listing_serves_fresh_and_stale_and_rejects_block_page() {
        let user = test_root("cache-test");
        let prev_user = std::env::var_os("PZ_USER");
        std::env::set_var("PZ_USER", &user);

        // Cổng 1 trên loopback: refuse ngay, không chờ DNS timeout như
        // một host .invalid (test case 5 cần một lần fetch hỏng thật).
        let url = "http://127.0.0.1:1/listing_test";
        let cpath = cache_path(url);
        fs::create_dir_all(cpath.parent().unwrap()).unwrap();

        // 1. Fresh valid listing
        let good_html = "sharedfiles/filedetails/?id=111 sharedfiles/filedetails/?id=222";
        fs::write(&cpath, good_html).unwrap();
        assert_eq!(cached(url, Some(BROWSE_TTL)).as_deref(), Some(good_html));
        assert_eq!(cached_listing(url).unwrap(), good_html);

        // 2. Block page in cache -> must NOT be served
        fs::write(&cpath, "You've made too many requests recently").unwrap();
        assert!(cached(url, None).is_none());

        // 3. Page without required marker -> must NOT be served
        fs::write(&cpath, "<html>random text</html>").unwrap();
        assert!(cached(url, None).is_none());

        // 4. Valid listing in stale cache -> cached_listing serves it (SWR)
        fs::write(&cpath, good_html).unwrap();
        assert_eq!(cached(url, None).as_deref(), Some(good_html));

        // 5. TTL expired + refresh impossible (nothing listening) ->
        //    cached_page still serves the stale copy instead of failing.
        assert_eq!(
            cached_page(url, Duration::ZERO, ITEM_GAP).unwrap(),
            good_html
        );

        if let Some(prev) = prev_user {
            std::env::set_var("PZ_USER", prev);
        } else {
            std::env::remove_var("PZ_USER");
        }
        fs::remove_dir_all(&user).ok();
    }

    #[test]
    fn prefetch_command_accepts_ids() {
        let res = prefetch(vec!["123".into(), "456".into(), "invalid".into()]);
        assert!(res.is_ok());
        assert_eq!(res.unwrap()["ok"], true);
    }
}
