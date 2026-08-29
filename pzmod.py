#!/usr/bin/env python3
"""pzmod - Project Zomboid workshop mod manager, no Steam login required.

Usage:
    pzmod search [text]        browse the workshop (no text = trending)
    pzmod info <id|url>        show one item's details
    pzmod install <id|url>...  download and install (collections are expanded)
    pzmod remove <id>...       remove every managed folder for an item
    pzmod list                 show installed mods and enabled state
    pzmod update               reinstall workshop items with newer files
    pzmod reinstall <id>...    force download and reinstall
    pzmod enable <folder>...   move installed folders into mods
    pzmod disable <folder>...  move installed folders into mods_off
    pzmod bisect <operation>   start/state/bad/good/stop a mod bisect
    pzmod selftest             run internal checks
"""
import contextlib
import hashlib
import io
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
import threading
import time
import urllib.error
import urllib.parse
import urllib.request

APPID = "108600"
GAME = os.environ.get("PZ_GAME", r"D:\ProjectZomboid")
USER = os.environ.get("PZ_USER", os.path.join(os.path.expanduser("~"), "Zomboid"))
MODS = os.path.join(USER, "mods")
OFF = os.path.join(USER, "mods_off")
STATE = os.path.join(USER, ".pzmod.json")

DETAILS = "https://api.steampowered.com/ISteamRemoteStorage/GetPublishedFileDetails/v1/"
COLLECTION = "https://api.steampowered.com/ISteamRemoteStorage/GetCollectionDetails/v1/"
BROWSE = "https://steamcommunity.com/workshop/browse/?"
UA = {"User-Agent": "Mozilla/5.0 (pzmod)"}

SORTS = {"trend": "Thịnh hành tuần", "totaluniquesubscriptions": "Nhiều sub nhất",
         "toprated": "Đánh giá cao nhất", "num_parent_items": "Được cần nhiều nhất",
         "mostrecent": "Mới nhất", "textsearch": "Khớp từ khoá"}

# "Most Required by Other Items" của Workshop không phải chỉ đổi browsesort: nó
# còn cần special_filter=6, thiếu là Steam lặng lẽ trả về danh sách mặc định.
# Số 6 lấy từ hàm onViewAll trong bundle JS của trang chủ workshop.
SPECIAL_FILTER = {"num_parent_items": "6"}

# Steam ghim thông báo "Modding Policy" của Spiffo's Workshop lên đầu MỌI danh
# sách, kiểu sắp xếp nào cũng có, 0 sub. Nó không phải mod, không cài được.
PINNED = {"2872282653"}

# Steam bỏ qua browsesort nếu thiếu days: hỏi "nhiều sub nhất" mà không kèm cửa
# sổ thời gian thì nó trả về đúng danh sách trending, nên bộ lọc trông như chết.
# "toprated" cố tình không có mặt: Steam xếp theo sao và bỏ qua days, gửi kèm
# chỉ tạo ra một nút bấm không làm gì.
SORT_DAYS = {"trend": "7", "totaluniquesubscriptions": "3650"}

# Cửa sổ thời gian, giống hộp chọn của Workshop. "Tất cả" là 3650 ngày chứ không
# phải -1: Steam đọc -1 ra thành một ngày, trả về mod mới toanh vài chục sub.
PERIODS = [("7", "1 tuần"), ("30", "1 tháng"), ("90", "3 tháng"),
           ("180", "6 tháng"), ("365", "1 năm"), ("3650", "Tất cả")]

TAGS = ["Build 40", "Build 41", "Build 42", "Animals", "Audio", "Balance",
        "Building", "Clothing/Armor", "Farming", "Food", "Framework", "Hardmode",
        "Interface", "Items", "Language/Translation", "Literature", "Map", "Military",
        "Misc", "Models", "Multiplayer", "Pop Culture", "Realistic", "Silly/Fun",
        "Skills", "Textures", "Traits", "Vehicles", "QoL", "WIP", "Weapons"]

# Steam Community rate limits affect the whole machine, including the Steam client.
CACHE_DIR = os.path.join(os.path.dirname(os.path.abspath(__file__)), ".cache")
BROWSE_TTL = 30 * 60
BROWSE_GAP = 2.0
_last_hit = [0.0]
_hit_lock = threading.Lock()

RATE_LIMITED = ("Steam chặn vì hỏi quá nhiều - chờ 10-30 phút. "
                "IP Warp là IP dùng chung nên dễ dính hơn IP nhà")


class Blocked(Exception):
    """Steam Community did not return a usable page."""


STEAMCMD_CANDIDATES = [
    r"C:\WorkshopDL\steamcmd\steamcmd.exe",
    r"E:\steamcmd\steamcmd.exe",
    r"C:\steamcmd\steamcmd.exe",
]


def die(msg):
    print("ERROR: " + msg)
    raise SystemExit(1)


def steamcmd():
    for path in STEAMCMD_CANDIDATES:
        if os.path.exists(path):
            return path
    found = shutil.which("steamcmd") or shutil.which("steamcmd.exe")
    if found:
        return found
    die("không tìm thấy steamcmd.exe; hãy cài SteamCMD hoặc thêm đường dẫn vào STEAMCMD_CANDIDATES")


def post(url, fields):
    body = urllib.parse.urlencode(fields).encode()
    req = urllib.request.Request(url, data=body, headers=UA)
    with urllib.request.urlopen(req, timeout=60) as response:
        return json.load(response)["response"]


def maybe_id(value):
    """Return a workshop id from a bare id or URL, otherwise None."""
    value = value.strip()
    if value.isdigit():
        return value
    match = re.search(r"[?&]id=(\d+)", value)
    return match.group(1) if match else None


def as_id(value):
    """Require a bare workshop id or a workshop URL carrying an id."""
    return maybe_id(value) or die("không phải workshop id hoặc URL: %s" % value)


def details(ids):
    """Return a mapping of workshop id to detail dictionaries."""
    ids = list(ids)
    if not ids:
        return {}
    fields = {"itemcount": len(ids)}
    for index, wid in enumerate(ids):
        fields["publishedfileids[%d]" % index] = wid
    out = {}
    for detail in post(DETAILS, fields).get("publishedfiledetails", []):
        if detail.get("result") == 1:
            out[detail["publishedfileid"]] = detail
    return out


def children(wid):
    """Return child ids when wid is a collection."""
    response = post(COLLECTION, {"collectioncount": 1, "publishedfileids[0]": wid})
    for collection in response.get("collectiondetails", []):
        if collection.get("result") == 1:
            return [child["publishedfileid"] for child in collection.get("children", [])]
    return []


def read_state():
    try:
        with open(STATE, encoding="utf-8") as state_file:
            state = json.load(state_file)
        return state if isinstance(state, dict) else {}
    except (OSError, ValueError):
        return {}


def write_state(state):
    """Atomically replace state so an interrupted write cannot truncate it."""
    os.makedirs(USER, exist_ok=True)
    temp_state = STATE + ".tmp"
    with open(temp_state, "w", encoding="utf-8") as state_file:
        json.dump(state, state_file, indent=2, ensure_ascii=False)
    os.replace(temp_state, STATE)


def _child(base, folder):
    """Resolve one direct child and reject path traversal from corrupted state."""
    if (not isinstance(folder, str) or not folder or folder in (".", "..") or
            "/" in folder or "\\" in folder or os.path.basename(folder) != folder):
        raise ValueError("tên thư mục không an toàn: %r" % folder)
    return os.path.join(base, folder)


def _entry_folders(entry):
    folders = entry.get("folders", []) if isinstance(entry, dict) else []
    return [folder for folder in folders if isinstance(folder, str)]


def folders_of(wid, state=None):
    """Return all folder names recorded for a workshop item."""
    state = read_state() if state is None else state
    return _entry_folders(state.get(wid, {}))


def folder_status(folder):
    """Return enabled, disabled, missing, or collision for a folder name."""
    try:
        enabled = os.path.isdir(_child(MODS, folder))
        disabled = os.path.isdir(_child(OFF, folder))
    except ValueError:
        return "invalid"
    if enabled and disabled:
        return "collision"
    if enabled:
        return "enabled"
    if disabled:
        return "disabled"
    return "missing"


def folder_of(wid):
    """Compatibility helper: return the first present folder for an item."""
    for folder in folders_of(wid):
        if folder_status(folder) in ("enabled", "disabled", "collision"):
            return folder
    return None


def _entry_complete(entry):
    folders = _entry_folders(entry)
    return bool(folders) and all(folder_status(folder) in ("enabled", "disabled")
                                 for folder in folders)


def installed_modids(state=None):
    """Return mod.info ids belonging to workshop items still present on disk."""
    state = read_state() if state is None else state
    out = set()
    for entry in state.values():
        if any(folder_status(folder) in ("enabled", "disabled")
               for folder in _entry_folders(entry)):
            out.update(value for value in entry.get("modids", []) if value)
    return out


def _require_id(value):
    return value.strip().lstrip("\\/").strip()


def _missing_modids(required, supplied):
    supplied = {value.casefold() for value in supplied}
    normalized = {}
    for value in required:
        value = _require_id(value)
        if value:
            normalized.setdefault(value.casefold(), value)
    return sorted((value for key, value in normalized.items() if key not in supplied),
                  key=str.casefold)


def missing_requirements(state=None):
    """Return required mod ids not supplied by any installed workshop item."""
    state = read_state() if state is None else state
    required = (value for entry in state.values() for value in entry.get("require", []))
    return _missing_modids(required, installed_modids(state))


def parse_modinfo(path):
    """Parse a BOM/CRLF/bad-byte-tolerant Project Zomboid mod.info file."""
    with open(path, "rb") as info_file:
        text = info_file.read().decode("utf-8-sig", "replace")
    out = {}
    for line in text.splitlines():
        line = line.strip()
        if not line or line.startswith("#") or "=" not in line:
            continue
        key, value = line.split("=", 1)
        key = key.strip()
        if key:
            out[key] = value.strip()
    return out


def _mod_roots(item_dir):
    """Find direct children of item/mods that represent PZ mod roots."""
    mods_dir = os.path.join(item_dir, "mods")
    if not os.path.isdir(mods_dir):
        return []
    roots = []
    for folder in sorted(os.listdir(mods_dir), key=str.lower):
        root = os.path.join(mods_dir, folder)
        if not os.path.isdir(root):
            continue
        infos = []
        for current, dirs, files in os.walk(root):
            dirs.sort(key=str.lower)
            if "mod.info" in files:
                infos.append(os.path.join(current, "mod.info"))
        if infos:
            roots.append((folder, root, infos))
    return roots


def _mod_metadata(roots):
    modids, required = [], []
    modid_keys, required_keys = set(), set()
    for _, _, info_paths in roots:
        for info_path in info_paths:
            info = parse_modinfo(info_path)
            modid = info.get("id")
            if modid and modid.casefold() not in modid_keys:
                modid_keys.add(modid.casefold())
                modids.append(modid)
            for dependency in info.get("require", "").split(","):
                dependency = _require_id(dependency)
                if dependency and dependency.casefold() not in required_keys:
                    required_keys.add(dependency.casefold())
                    required.append(dependency)
    return modids, required


def download(wid, force=False):
    """Download one workshop item through SteamCMD and return its item folder."""
    exe = steamcmd()
    root = os.path.join(os.path.dirname(exe), "steamapps", "workshop")
    dest = os.path.join(root, "content", APPID, wid)
    if force:
        shutil.rmtree(dest, ignore_errors=True)
        manifest = os.path.join(root, "appworkshop_%s.acf" % APPID)
        if os.path.exists(manifest):
            os.remove(manifest)
    command = [exe, "+login", "anonymous", "+workshop_download_item", APPID, wid, "+quit"]
    output = subprocess.run(command, capture_output=True, text=True, errors="replace").stdout
    if "Success. Downloaded item" not in output and not os.path.isdir(dest):
        tail = " | ".join(line.strip() for line in output.splitlines()[-4:] if line.strip())
        die("steamcmd thất bại với %s: %s" % (wid, tail))
    if not os.path.isdir(dest):
        die("steamcmd báo thành công nhưng thiếu thư mục %s" % dest)
    return dest


def _restore_transaction(installed, moved):
    for path in reversed(installed):
        if os.path.isdir(path):
            shutil.rmtree(path)
        elif os.path.exists(path):
            raise OSError("không thể khôi phục vì đích không còn là thư mục: %s" % path)
    for backup, original in reversed(moved):
        if os.path.isdir(backup):
            if os.path.exists(original):
                raise OSError("không thể khôi phục vì đích đã tồn tại: %s" % original)
            os.makedirs(os.path.dirname(original), exist_ok=True)
            shutil.move(backup, original)


def install_one(wid, detail=None, force=False):
    detail = detail or details([wid]).get(wid)
    if not detail:
        print("  ! %s: mod đã bị xoá hoặc đặt riêng tư" % wid)
        return False
    if str(detail.get("consumer_app_id")) != APPID:
        print("  ! %s: thuộc app %s, không phải Project Zomboid" %
              (wid, detail.get("consumer_app_id")))
        return False

    title = detail.get("title") or wid
    state = read_state()
    previous = state.get(wid, {})
    if (previous and not force and previous.get("updated") == detail.get("time_updated")
            and _entry_complete(previous)):
        print("  = %s đã là bản mới nhất" % title)
        return True

    source = download(wid, force=bool(previous) or force)
    roots = _mod_roots(source)
    if not roots:
        print("  ! %s: không tìm thấy mods/<ModName>/mod.info" % title)
        return False

    old_folders = set(_entry_folders(previous))
    old_locations = {}
    for folder in old_folders:
        try:
            enabled = _child(MODS, folder)
            disabled = _child(OFF, folder)
        except ValueError as error:
            print("  ! %s: %s" % (title, error))
            return False
        if os.path.isdir(enabled) and os.path.isdir(disabled):
            print("  ! %s: %s có ở cả mods và mods_off; giữ nguyên để người dùng xử lý" %
                  (title, folder))
            return False
        if any(os.path.exists(path) and not os.path.isdir(path)
               for path in (enabled, disabled)):
            print("  ! %s: %s trùng với một file; giữ nguyên" % (title, folder))
            return False
        if os.path.isdir(enabled):
            old_locations[folder] = MODS
        elif os.path.isdir(disabled):
            old_locations[folder] = OFF

    for folder, _, _ in roots:
        try:
            destinations = (_child(MODS, folder), _child(OFF, folder))
        except ValueError as error:
            print("  ! %s: %s" % (title, error))
            return False
        if folder not in old_folders and any(os.path.exists(path) for path in destinations):
            print("  ! %s: thư mục %s đã tồn tại và không thuộc bản cài này" % (title, folder))
            return False

    modids, required = _mod_metadata(roots)

    os.makedirs(USER, exist_ok=True)
    os.makedirs(MODS, exist_ok=True)
    installed, moved = [], []
    transaction = tempfile.mkdtemp(prefix=".pzmod-%s-" % wid, dir=USER)
    keep_transaction = False
    try:
        staged = os.path.join(transaction, "new")
        backup = os.path.join(transaction, "old")
        os.makedirs(staged)
        for folder, root, _ in roots:
            shutil.copytree(root, _child(staged, folder))

        for folder in old_folders:
            for label, base in (("mods", MODS), ("mods_off", OFF)):
                original = _child(base, folder)
                if os.path.isdir(original):
                    saved = os.path.join(backup, label, folder)
                    os.makedirs(os.path.dirname(saved), exist_ok=True)
                    shutil.move(original, saved)
                    moved.append((saved, original))

        new_folders = {folder for folder, _, _ in roots}
        for folder, _, _ in roots:
            destination = _child(old_locations.get(folder, MODS), folder)
            shutil.move(_child(staged, folder), destination)
            installed.append(destination)

        preserved = []
        for folder in sorted(old_folders - new_folders, key=str.lower):
            base = old_locations.get(folder)
            if not base:
                continue
            saved = os.path.join(backup, "mods" if base == MODS else "mods_off", folder)
            destination = _child(OFF, folder)
            installed.append(destination)
            shutil.copytree(saved, destination)
            preserved.append(folder)

        state[wid] = {
            "title": title,
            "updated": detail.get("time_updated"),
            "size": int(detail.get("file_size") or 0),
            "folders": [folder for folder, _, _ in roots],
            "modids": modids,
            "require": required,
        }
        write_state(state)
    except Exception as error:
        try:
            _restore_transaction(installed, moved)
        except Exception as rollback_error:
            keep_transaction = True
            print("  ! KHÔNG KHÔI PHỤC ĐƯỢC; bản sao cũ còn tại %s (%s)" %
                  (transaction, rollback_error))
        print("  ! %s: cài đặt thất bại (%s)" % (title, error))
        return False
    finally:
        if not keep_transaction:
            shutil.rmtree(transaction, ignore_errors=True)

    for folder in preserved:
        print("  ! %s không còn trong bản mới, đã chuyển sang mods_off" % folder)
    print("  + %s -> %d thư mục (%.1f KB)" %
          (title, len(roots), int(detail.get("file_size") or 0) / 1024))
    return True


def with_deps(ids, depth=4):
    """Return workshop ids in dependency-first post-order."""
    order, seen = [], set()

    def walk(wid, left):
        if wid in seen:
            return
        seen.add(wid)
        dependencies = []
        if left > 0:
            try:
                dependencies = requires(wid)
            except Blocked as error:
                print("  ! %s: không dò được mod bắt buộc (%s)" % (wid, error))
        for dependency in dependencies:
            walk(dependency, left - 1)
        order.append(wid)

    for wid in ids:
        walk(wid, depth)
    return order


def cmd_install(args, force=False, deps=True):
    if not args:
        die("install cần workshop id hoặc URL")
    ids = [as_id(arg) for arg in args]
    metadata = details(ids)
    todo = []
    for wid in ids:
        kids = children(wid)
        if kids:
            print("collection %s: %d mod" % (metadata.get(wid, {}).get("title", wid), len(kids)))
            todo.extend(kids)
        else:
            todo.append(wid)
    order = list(dict.fromkeys(todo))
    if deps:
        requested = set(order)
        order = with_deps(order)
        extra = [wid for wid in order if wid not in requested]
        if extra:
            names = details(extra)
            print("kèm %d mod bắt buộc: %s" % (len(extra), ", ".join(
                names.get(wid, {}).get("title", wid) for wid in extra)))
    metadata = details(order)
    for wid in order:
        install_one(wid, metadata.get(wid), force=force)
    missing = missing_requirements()
    if missing:
        print("  ! thiếu mod bắt buộc: %s" % ", ".join(missing))


def cached_page(url, ttl=BROWSE_TTL):
    """Return cached listing HTML, serving stale data when Steam blocks a refresh."""
    path = os.path.join(CACHE_DIR, hashlib.sha1(url.encode()).hexdigest() + ".html")

    def cached(require_fresh):
        if not os.path.exists(path):
            return None
        if require_fresh and time.time() - os.path.getmtime(path) >= ttl:
            return None
        with open(path, encoding="utf-8") as cache_file:
            html = cache_file.read()
        return html if "filedetails/?id=" in html else None

    fresh = cached(True)
    if fresh:
        return fresh
    with _hit_lock:
        gap = BROWSE_GAP - (time.time() - _last_hit[0])
        if gap > 0:
            time.sleep(gap)
        _last_hit[0] = time.time()
        try:
            request = urllib.request.Request(url, headers=UA)
            with urllib.request.urlopen(request, timeout=60) as response:
                html = response.read().decode("utf-8", "replace")
        except urllib.error.HTTPError as error:
            html = None
            why = RATE_LIMITED if error.code == 429 else "steamcommunity.com trả HTTP %s" % error.code
        except (urllib.error.URLError, OSError) as error:
            html = None
            why = "không nối được steamcommunity.com (%s) - Warp/VPN đã bật chưa?" % error
        else:
            why = None
            if "too many requests" in html.lower():
                html, why = None, RATE_LIMITED
    if html is None:
        stale = cached(False)
        if stale:
            return stale
        raise Blocked(why)
    os.makedirs(CACHE_DIR, exist_ok=True)
    with open(path, "w", encoding="utf-8") as cache_file:
        cache_file.write(html)
    return html


ITEM_URL = "https://steamcommunity.com/sharedfiles/filedetails/?id=%s"
REQUIRES_TTL = 7 * 24 * 3600
REQ_BLOCK = re.compile(r'id="RequiredItems".*?(?:<!--|rightSectionTopTitle)', re.S)


def requires(wid):
    """Scrape the workshop Required Items block for workshop ids."""
    match = REQ_BLOCK.search(cached_page(ITEM_URL % wid, ttl=REQUIRES_TTL))
    if not match:
        return []
    out, seen = [], set()
    for dependency in re.findall(r"filedetails/\?id=(\d+)", match.group(0)):
        if dependency != wid and dependency not in seen:
            seen.add(dependency)
            out.append(dependency)
    return out[:20]


def browse(query="", sort="trend", page=1, tags=(), days=None):
    """Scrape workshop listing ids; the keyless details API supplies metadata."""
    wid = maybe_id(query)
    if wid:
        return [wid] if details([wid]) else []
    browse_sort = "textsearch" if query else sort
    fields = [("appid", APPID), ("section", "readytouseitems"),
              ("browsesort", browse_sort), ("p", str(page))]
    if query:
        fields.append(("searchtext", query))
    if browse_sort in SORT_DAYS:
        window = days if days in dict(PERIODS) else SORT_DAYS[browse_sort]
        fields.append(("days", window))
    if browse_sort in SPECIAL_FILTER:
        fields.append(("special_filter", SPECIAL_FILTER[browse_sort]))
    fields.extend(("requiredtags[]", tag) for tag in tags)
    html = cached_page(BROWSE + urllib.parse.urlencode(fields))
    ids, seen = [], set()
    for match in re.finditer(r"sharedfiles/filedetails/\?id=(\d+)", html):
        if match.group(1) not in seen and match.group(1) not in PINNED:
            seen.add(match.group(1))
            ids.append(match.group(1))
    return ids


def cmd_search(args):
    try:
        ids = browse(" ".join(args))[:30]
    except Blocked as error:
        die(str(error))
    if not ids:
        print("không tìm thấy")
        return
    metadata = details(ids)
    installed = read_state()
    for wid in ids:
        detail = metadata.get(wid)
        if not detail:
            continue
        tags = ",".join(tag["tag"] for tag in detail.get("tags", []))[:28]
        print("  %s%-12s %7s subs  %-30s %s" % (
            "*" if wid in installed else " ", wid, detail.get("subscriptions", 0),
            tags, (detail.get("title") or "")[:52]))
    print("(* = đã cài)  cài bằng: pzmod install <id>")


def strip_bb(text):
    return re.sub(r"\[[^\]]{0,200}\]", "", text or "")


def cmd_info(args):
    if not args:
        die("info cần workshop id hoặc URL")
    state = read_state()
    for wid in [as_id(arg) for arg in args]:
        detail = details([wid]).get(wid)
        if not detail:
            print("%s: không tìm thấy" % wid)
            continue
        kids = children(wid)
        print("%s - %s" % (wid, detail.get("title")))
        print("  %s subs | %.1f MB | tags: %s" % (
            detail.get("subscriptions", 0), int(detail.get("file_size") or 0) / 1048576,
            ", ".join(tag["tag"] for tag in detail.get("tags", [])) or "không có"))
        if kids:
            print("  COLLECTION gồm %d mod" % len(kids))
        print("  " + strip_bb(detail.get("description")).replace("\r", "").replace("\n", " ")[:400])
        entry = state.get(wid, {})
        folders = ["%s (%s)" % (folder, folder_status(folder))
                   for folder in _entry_folders(entry)]
        print("  thư mục đã cài: %s" % (", ".join(folders) or "chưa cài"))
        if entry.get("modids"):
            print("  mod ids: %s" % ", ".join(entry["modids"]))
        if entry.get("require"):
            print("  require: %s" % ", ".join(entry["require"]))


def cmd_list(args):
    state = read_state()
    if not state:
        print("chưa có mod nào được pzmod quản lý (mods: %s)" % MODS)
    for wid, entry in sorted(state.items(), key=lambda pair: pair[1].get("title", "").lower()):
        print("  %-12s %-52s %.1f KB" %
              (wid, entry.get("title", wid)[:52], entry.get("size", 0) / 1024))
        for folder in _entry_folders(entry):
            labels = {"enabled": "bật", "disabled": "tắt", "missing": "MẤT",
                      "collision": "XUNG ĐỘT", "invalid": "KHÔNG AN TOÀN"}
            status = folder_status(folder)
            print("      [%s] %s" % (labels[status], folder))
    managed = {folder for entry in state.values() for folder in _entry_folders(entry)}
    extras = []
    for label, base in (("mods", MODS), ("mods_off", OFF)):
        if os.path.isdir(base):
            extras.extend("%s/%s" % (label, name) for name in os.listdir(base)
                          if os.path.isdir(os.path.join(base, name)) and name not in managed)
    if extras:
        print("  không do pzmod quản lý: %s" % ", ".join(sorted(extras, key=str.lower)))
    missing = missing_requirements(state)
    if missing:
        print("  ! thiếu mod bắt buộc: %s" % ", ".join(missing))


def cmd_remove(args):
    if not args:
        die("remove cần workshop id hoặc URL")
    state = read_state()
    for wid in [as_id(arg) for arg in args]:
        entry = state.get(wid)
        if not entry:
            print("  ? %s chưa được pzmod cài" % wid)
            continue
        paths = []
        safe = True
        for folder in _entry_folders(entry):
            try:
                folder_paths = (_child(MODS, folder), _child(OFF, folder))
            except ValueError as error:
                print("  ! %s: %s" % (wid, error))
                safe = False
                continue
            for path in folder_paths:
                if os.path.exists(path) and not os.path.isdir(path):
                    print("  ! từ chối xoá vì không phải thư mục: %s" % path)
                    safe = False
                elif os.path.isdir(path):
                    paths.append(path)
        if not safe:
            continue

        os.makedirs(USER, exist_ok=True)
        transaction = tempfile.mkdtemp(prefix=".pzmod-remove-%s-" % wid, dir=USER)
        moved, keep_transaction = [], False
        try:
            for path in paths:
                label = "mods" if os.path.dirname(path) == MODS else "mods_off"
                saved = os.path.join(transaction, label, os.path.basename(path))
                os.makedirs(os.path.dirname(saved), exist_ok=True)
                shutil.move(path, saved)
                moved.append((saved, path))
            state.pop(wid, None)
            write_state(state)
        except Exception as error:
            state[wid] = entry
            try:
                _restore_transaction([], moved)
            except Exception as rollback_error:
                keep_transaction = True
                print("  ! KHÔNG KHÔI PHỤC ĐƯỢC; bản sao còn tại %s (%s)" %
                      (transaction, rollback_error))
            print("  ! %s: xoá thất bại (%s)" % (entry.get("title", wid), error))
            continue
        finally:
            if not keep_transaction:
                shutil.rmtree(transaction, ignore_errors=True)
        print("  - %s đã xoá" % entry.get("title", wid))


def cmd_update(args):
    state = read_state()
    if not state:
        print("không có mod để cập nhật")
        return
    metadata = details(state.keys())
    for wid, entry in sorted(state.items(), key=lambda pair: pair[1].get("title", "").lower()):
        detail = metadata.get(wid)
        if not detail:
            print("  ! %s: đã biến mất khỏi workshop, giữ nguyên bản cục bộ" % entry.get("title", wid))
        elif detail.get("time_updated") == entry.get("updated") and _entry_complete(entry):
            print("  = %s" % entry.get("title", wid))
        else:
            install_one(wid, detail, force=True)
    missing = missing_requirements()
    if missing:
        print("  ! thiếu mod bắt buộc: %s" % ", ".join(missing))


def _pzbisect():
    try:
        import pzbisect
        return pzbisect
    except ImportError:
        die("thiếu pzbisect.py")


def cmd_enable(args):
    if not args:
        die("enable cần tên thư mục mod")
    module = _pzbisect()
    for folder in args:
        module.enable(folder)
        print("  + đã bật %s" % folder)


def cmd_disable(args):
    if not args:
        die("disable cần tên thư mục mod")
    module = _pzbisect()
    for folder in args:
        module.disable(folder)
        print("  - đã tắt %s" % folder)


def cmd_bisect(args):
    module = _pzbisect()
    operation = args[0].lower() if args else "state"
    if operation == "start":
        result = module.bisect_start(args[1:] or None)
    elif operation == "bad":
        result = module.bisect_mark(True)
    elif operation == "good":
        result = module.bisect_mark(False)
    elif operation == "stop":
        result = module.bisect_stop()
    elif operation == "state":
        result = module.bisect_state()
    else:
        die("bisect dùng: start [folder...] | state | bad | good | stop")
    print(json.dumps(result, indent=2, ensure_ascii=False))


def check_cache():
    """Exercise fresh, stale, and invalid cache paths without real Steam traffic."""
    global CACHE_DIR
    keep, url = CACHE_DIR, "https://example.invalid/listing"
    CACHE_DIR = tempfile.mkdtemp(prefix="pzmod-test-")
    path = os.path.join(CACHE_DIR, hashlib.sha1(url.encode()).hexdigest() + ".html")
    try:
        good = "a sharedfiles/filedetails/?id=111 b sharedfiles/filedetails/?id=222"
        with open(path, "w", encoding="utf-8") as cache_file:
            cache_file.write(good)
        assert cached_page(url) == good
        os.utime(path, (0, 0))
        assert cached_page(url) == good
        with open(path, "w", encoding="utf-8") as cache_file:
            cache_file.write("You've made too many requests recently")
        os.utime(path, (0, 0))
        try:
            cached_page(url)
            raise AssertionError("invalid listing cache must not be served")
        except Blocked:
            pass
    finally:
        shutil.rmtree(CACHE_DIR, ignore_errors=True)
        CACHE_DIR = keep


def check_mod_transactions():
    """Exercise update/remove safety against a throwaway PZ user tree."""
    global USER, MODS, OFF, STATE, download, write_state
    keep_paths = USER, MODS, OFF, STATE
    real_download = download
    real_write_state = write_state
    try:
        with tempfile.TemporaryDirectory(prefix="pzmod-files-") as root:
            USER = root
            MODS = os.path.join(root, "mods")
            OFF = os.path.join(root, "mods_off")
            STATE = os.path.join(root, ".pzmod.json")
            os.makedirs(os.path.join(OFF, "Keep"))
            os.makedirs(os.path.join(MODS, "Retired"))
            with open(os.path.join(OFF, "Keep", "old.txt"), "w") as old_file:
                old_file.write("old")
            with open(os.path.join(MODS, "Retired", "retired.txt"), "w") as old_file:
                old_file.write("retired")
            write_state({"1": {"title": "Test", "updated": 1, "size": 1,
                               "folders": ["Keep", "Retired"],
                               "modids": ["Test"], "require": []}})

            source = os.path.join(root, "source")
            os.makedirs(os.path.join(source, "mods", "Keep"))
            with open(os.path.join(source, "mods", "Keep", "mod.info"), "w") as info_file:
                info_file.write("id=Test\n")
            with open(os.path.join(source, "mods", "Keep", "new.txt"), "w") as new_file:
                new_file.write("new")
            download = lambda wid, force=False: source
            detail = {"consumer_app_id": 108600, "title": "Test",
                      "time_updated": 2, "file_size": 2}
            output = io.StringIO()
            with contextlib.redirect_stdout(output):
                assert install_one("1", detail, force=True)
            assert os.path.isfile(os.path.join(OFF, "Keep", "new.txt"))
            assert not os.path.exists(os.path.join(MODS, "Keep"))
            assert os.path.isfile(os.path.join(OFF, "Retired", "retired.txt"))
            assert read_state()["1"]["folders"] == ["Keep"]
            assert "! Retired không còn trong bản mới, đã chuyển sang mods_off" in output.getvalue()

            blocker = os.path.join(MODS, "Blocker")
            with open(blocker, "w") as blocker_file:
                blocker_file.write("do not delete")
            state = read_state()
            state["1"]["folders"].append("Blocker")
            write_state(state)
            with contextlib.redirect_stdout(io.StringIO()):
                cmd_remove(["1"])
            assert os.path.isdir(os.path.join(OFF, "Keep"))
            assert "1" in read_state(), "remove must preflight every path before moving any"
            os.remove(blocker)
            write_state = lambda state: (_ for _ in ()).throw(OSError("forced state failure"))
            with contextlib.redirect_stdout(io.StringIO()):
                cmd_remove(["1"])
            assert os.path.isdir(os.path.join(OFF, "Keep"))
            assert "1" in read_state(), "remove must roll files back when state write fails"
            write_state = real_write_state
            with contextlib.redirect_stdout(io.StringIO()):
                cmd_remove(["1"])
            assert "1" not in read_state()
            assert not os.path.exists(os.path.join(OFF, "Keep"))
    finally:
        USER, MODS, OFF, STATE = keep_paths
        download = real_download
        write_state = real_write_state


def cmd_selftest(args):
    assert as_id("498441420") == "498441420"
    assert as_id("https://steamcommunity.com/sharedfiles/filedetails/?id=123&searchtext=x") == "123"
    assert as_id(" 456 ") == "456"
    assert strip_bb("a [url=x]link[/url] b") == "a link b"
    long_tag = "[url=https://example.invalid/" + "x" * 80 + "]"
    assert strip_bb("a " + long_tag + "link[/url] b") == "a link b"
    with tempfile.TemporaryDirectory(prefix="pzmod-logic-") as root:
        mods = os.path.join(root, "mods")
        os.makedirs(os.path.join(mods, "Direct"))
        os.makedirs(os.path.join(mods, "Builds", "42.20"))
        direct = os.path.join(mods, "Direct", "mod.info")
        build = os.path.join(mods, "Builds", "42.20", "mod.info")
        with open(direct, "wb") as info_file:
            info_file.write(b"\xef\xbb\xbf# ignored\r\nname=old\r\nname=New\r\nid=DirectID\r\nbad=\xff\r\nrequire=A, B\r\n")
        with open(build, "wb") as info_file:
            info_file.write(b"id=BuildID\nrequire=\\DirectID, /Other\n")
        parsed = parse_modinfo(direct)
        assert parsed["name"] == "New", "last key must win"
        assert parsed["require"] == "A, B"
        assert _require_id("  \\/ZombieBuddy ") == "ZombieBuddy"
        assert _missing_modids(["\\ZombieBuddy", "/Other"],
                               ["zombiebuddy"]) == ["Other"]
        found = _mod_roots(root)
        assert [entry[0] for entry in found] == ["Builds", "Direct"]
        modids, required = _mod_metadata(found)
        assert modids == ["BuildID", "DirectID"]
        assert required == ["DirectID", "Other", "A", "B"]
        assert _missing_modids(required, modids) == ["A", "B", "Other"]
    check_mod_transactions()
    check_cache()
    try:
        assert len(browse(sort="trend")) > 10, "workshop browse layout changed"
    except Blocked as error:
        print("  (bỏ qua kiểm tra browse: %s)" % error)
    try:
        detail = details(["498441420"])["498441420"]
        assert str(detail["consumer_app_id"]) == APPID, "keyless details API changed"
    except (urllib.error.URLError, OSError) as error:
        print("  (bỏ qua kiểm tra details: %s)" % error)
    print("selftest OK (steamcmd: %s)" % steamcmd())


CMDS = {"search": cmd_search, "info": cmd_info, "install": cmd_install,
        "remove": cmd_remove, "list": cmd_list, "update": cmd_update,
        "reinstall": lambda args: cmd_install(args, force=True),
        "enable": cmd_enable, "disable": cmd_disable, "bisect": cmd_bisect,
        "selftest": cmd_selftest}

NO_DEPS = "--no-deps"


if __name__ == "__main__":
    if len(sys.argv) < 2 or sys.argv[1] not in CMDS:
        print(__doc__)
        print("game: %s\nuser: %s\nmods: %s" % (GAME, USER, MODS))
        raise SystemExit(0)
    command = sys.argv[1]
    if command in ("install", "update", "reinstall") and not os.path.isdir(GAME):
        die("không tìm thấy thư mục game: %s (đặt PZ_GAME để đổi)" % GAME)
    argv = sys.argv[2:]
    if NO_DEPS in argv:
        argv = [arg for arg in argv if arg != NO_DEPS]
        if command not in ("install", "reinstall"):
            die("%s chỉ dùng với install/reinstall" % NO_DEPS)
        cmd_install(argv, force=command == "reinstall", deps=False)
    else:
        CMDS[command](argv)
