#!/usr/bin/env python3
"""pzbisect - Mod enable/disable and binary search bisect tool for Project Zomboid.

Project Zomboid stores its active mod list inside save files. This tool
controls active/inactive mods via filesystem placement between:
  - %USERPROFILE%/Zomboid/mods      (Enabled)
  - %USERPROFILE%/Zomboid/mods_off  (Disabled)

Public API:
  installed_folders(user_dir=None) -> [(folder, enabled: bool)]
  enable(folder, user_dir=None) -> bool
  disable(folder, user_dir=None) -> bool
  bisect_start(names=None, user_dir=None) -> dict
  bisect_mark(bad: bool, user_dir=None) -> dict
  bisect_state(user_dir=None) -> dict
  bisect_stop(user_dir=None) -> dict
"""

import json
import os
import shutil
import sys
import tempfile
from typing import Dict, List, Optional, Set, Tuple

DEFAULT_USER = os.environ.get("PZ_USER", os.path.join(os.path.expanduser("~"), "Zomboid"))
MODS_DIRNAME = "mods"
OFF_DIRNAME = "mods_off"
STATE_FILENAME = ".pzbisect.json"


def get_paths(user_dir: Optional[str] = None) -> Tuple[str, str, str, str]:
    """Return (user, mods_dir, off_dir, state_file)."""
    user = os.path.abspath(user_dir if user_dir else os.environ.get("PZ_USER", DEFAULT_USER))
    mods = os.path.join(user, MODS_DIRNAME)
    off = os.path.join(user, OFF_DIRNAME)
    state = os.path.join(user, STATE_FILENAME)
    return user, mods, off, state


def _check_folder_name(folder: str) -> str:
    """Validate folder name to prevent directory traversal or invalid characters."""
    if not isinstance(folder, str) or not folder.strip():
        raise ValueError(f"Tên thư mục mod không hợp lệ: {folder!r}")
    cleaned = folder.strip()
    if (
        os.path.basename(cleaned) != cleaned
        or "/" in cleaned
        or "\\" in cleaned
        or cleaned in (".", "..")
    ):
        raise ValueError(f"Tên thư mục mod không an toàn: {folder!r}")
    return cleaned


def _ensure_dirs(mods_dir: str, off_dir: str) -> None:
    """Ensure MODS and OFF directories exist."""
    os.makedirs(mods_dir, exist_ok=True)
    os.makedirs(off_dir, exist_ok=True)


def _check_collision(mods_dir: str, off_dir: str, folder: str) -> None:
    """Refuse if a mod folder exists in both MODS and OFF."""
    p_on = os.path.join(mods_dir, folder)
    p_off = os.path.join(off_dir, folder)
    if os.path.isdir(p_on) and os.path.isdir(p_off):
        raise RuntimeError(f"Xung đột: mod '{folder}' tồn tại ở cả mods/ và mods_off/")


def installed_folders(user_dir: Optional[str] = None) -> List[Tuple[str, bool]]:
    """Return a sorted list of [(folder, enabled: bool)].

    Guards:
      - Ignores non-directory files and hidden items.
      - Raises RuntimeError on name collision (folder exists in both mods/ and mods_off/).
    """
    _, mods, off, _ = get_paths(user_dir)
    res: Dict[str, bool] = {}

    on_set: Set[str] = set()
    if os.path.isdir(mods):
        for name in os.listdir(mods):
            if name.startswith("."):
                continue
            p = os.path.join(mods, name)
            if os.path.isdir(p):
                on_set.add(name)

    off_set: Set[str] = set()
    if os.path.isdir(off):
        for name in os.listdir(off):
            if name.startswith("."):
                continue
            p = os.path.join(off, name)
            if os.path.isdir(p):
                off_set.add(name)

    collisions = on_set & off_set
    if collisions:
        collided = ", ".join(sorted(collisions))
        raise RuntimeError(f"Xung đột tên mod: tồn tại đồng thời ở cả mods/ và mods_off/: {collided}")

    for f in on_set:
        res[f] = True
    for f in off_set:
        res[f] = False

    return sorted(res.items(), key=lambda x: x[0].lower())


def enable(folder: str, user_dir: Optional[str] = None) -> bool:
    """Enable a mod by moving it from mods_off/ to mods/.

    Idempotent: returns True if already enabled or moved successfully.
    """
    folder = _check_folder_name(folder)
    _, mods, off, _ = get_paths(user_dir)
    _ensure_dirs(mods, off)
    _check_collision(mods, off, folder)

    p_on = os.path.join(mods, folder)
    p_off = os.path.join(off, folder)

    if os.path.isdir(p_on):
        return True

    if not os.path.isdir(p_off):
        raise FileNotFoundError(f"Không tìm thấy thư mục mod '{folder}' trong mods/ hoặc mods_off/")

    shutil.move(p_off, p_on)
    return True


def disable(folder: str, user_dir: Optional[str] = None) -> bool:
    """Disable a mod by moving it from mods/ to mods_off/.

    Idempotent: returns True if already disabled or moved successfully.
    """
    folder = _check_folder_name(folder)
    _, mods, off, _ = get_paths(user_dir)
    _ensure_dirs(mods, off)
    _check_collision(mods, off, folder)

    p_on = os.path.join(mods, folder)
    p_off = os.path.join(off, folder)

    if os.path.isdir(p_off):
        return True

    if not os.path.isdir(p_on):
        raise FileNotFoundError(f"Không tìm thấy thư mục mod '{folder}' trong mods/ hoặc mods_off/")

    shutil.move(p_on, p_off)
    return True


def _read_bisect_file(state_file: str) -> Optional[dict]:
    if not os.path.exists(state_file):
        return None
    try:
        with open(state_file, "r", encoding="utf-8") as f:
            return json.load(f)
    except Exception:
        return None


def _write_bisect_file(state_file: str, state: dict) -> None:
    os.makedirs(os.path.dirname(state_file), exist_ok=True)
    tmp = state_file + ".tmp"
    with open(tmp, "w", encoding="utf-8") as f:
        json.dump(state, f, indent=2, ensure_ascii=False)
    os.replace(tmp, state_file)


def bisect_state(user_dir: Optional[str] = None) -> dict:
    """Return current bisect state:
    {round, candidates, enabled_now, suspect, done}
    """
    _, _, _, state_file = get_paths(user_dir)
    st = _read_bisect_file(state_file)
    installed = installed_folders(user_dir)
    enabled_now = [f for f, en in installed if en]

    if not st:
        return {
            "round": 0,
            "candidates": [],
            "enabled_now": enabled_now,
            "suspect": None,
            "done": False,
        }

    return {
        "round": st.get("round", 0),
        "candidates": st.get("candidates", []),
        "enabled_now": enabled_now,
        "suspect": st.get("suspect"),
        "done": st.get("done", False),
    }


def bisect_start(names: Optional[List[str]] = None, user_dir: Optional[str] = None) -> dict:
    """Start a new bisect session.

    If names is None, candidates = all currently enabled mods.
    Snapshot the current enabled set as original_enabled.
    Requires >= 2 candidates.
    Splits candidates into two halves, enables left half, disables right half.
    Refuses to start if an existing session is currently active (not done).
    """
    _, mods, off, state_file = get_paths(user_dir)

    existing_st = _read_bisect_file(state_file)
    if existing_st and not existing_st.get("done"):
        raise RuntimeError(
            "Đang có phiên bisect hoạt động. Hãy chạy 'pzbisect stop' trước khi bắt đầu phiên mới."
        )

    _ensure_dirs(mods, off)

    installed = installed_folders(user_dir)
    installed_dict = dict(installed)
    original_enabled = [f for f, en in installed if en]

    if names is None:
        candidates = list(original_enabled)
    else:
        cleaned_names = []
        for n in names:
            cn = _check_folder_name(n)
            if cn not in installed_dict:
                raise FileNotFoundError(f"Mod '{cn}' không tồn tại trong danh sách cài đặt.")
            cleaned_names.append(cn)
        candidates = sorted(list(set(cleaned_names)))

    if len(candidates) < 2:
        raise ValueError(f"Bisect cần tối thiểu 2 candidates để bắt đầu (hiện có {len(candidates)}).")

    candidates = sorted(candidates)
    mid = (len(candidates) + 1) // 2
    left = candidates[:mid]
    right = candidates[mid:]

    for f in left:
        enable(f, user_dir)
    for f in right:
        disable(f, user_dir)

    st = {
        "round": 1,
        "candidates": candidates,
        "current_tested": left,
        "current_untested": right,
        "original_enabled": original_enabled,
        "suspect": None,
        "done": False,
    }
    _write_bisect_file(state_file, st)
    return bisect_state(user_dir)


def bisect_mark(bad: bool, user_dir: Optional[str] = None) -> dict:
    """Mark the current split:
    bad=True  -> Bug reproduces -> culprit is in currently tested (enabled) half.
    bad=False -> Bug does not reproduce -> culprit is in currently untested (disabled) half.
    """
    _, _, _, state_file = get_paths(user_dir)
    st = _read_bisect_file(state_file)
    if not st:
        raise RuntimeError("Không có phiên bisect nào đang hoạt động.")
    if st.get("done"):
        return bisect_state(user_dir)

    current_tested = st.get("current_tested", [])
    current_untested = st.get("current_untested", [])

    new_candidates = list(current_tested if bad else current_untested)

    if not new_candidates:
        raise RuntimeError("Lỗi logic bisect: danh sách candidates bị rỗng.")

    if len(new_candidates) == 1:
        suspect = new_candidates[0]
        st["round"] = st.get("round", 1)
        st["candidates"] = [suspect]
        st["current_tested"] = []
        st["current_untested"] = []
        st["suspect"] = suspect
        st["done"] = True
        _write_bisect_file(state_file, st)
        return bisect_state(user_dir)

    st["round"] = st.get("round", 1) + 1
    st["candidates"] = sorted(new_candidates)
    mid = (len(new_candidates) + 1) // 2
    left = st["candidates"][:mid]
    right = st["candidates"][mid:]
    st["current_tested"] = left
    st["current_untested"] = right

    for f in left:
        enable(f, user_dir)
    for f in right:
        disable(f, user_dir)

    _write_bisect_file(state_file, st)
    return bisect_state(user_dir)


def bisect_stop(user_dir: Optional[str] = None) -> dict:
    """Stop the bisect session and restore the exact original layout.

    Guards:
      - If state file is corrupted or missing original_enabled, raises RuntimeError and touches nothing.
      - Only restores/disables folders in the bisect snapshot (original_enabled + candidates),
        leaving newly installed mods untouched.
    """
    _, _, _, state_file = get_paths(user_dir)
    st = _read_bisect_file(state_file)

    if not st:
        return {"stopped": False, "message": "Không có phiên bisect đang chạy", "done": False}

    if "original_enabled" not in st or not isinstance(st.get("original_enabled"), list):
        raise RuntimeError(
            "File trạng thái .pzbisect.json bị hỏng (thiếu trường 'original_enabled'). "
            "Dừng khôi phục để tránh tắt nhầm mod. Hãy khôi phục thủ công."
        )

    orig = set(st["original_enabled"])
    snapshot_pool = (
        orig
        | set(st.get("candidates", []))
        | set(st.get("current_tested", []))
        | set(st.get("current_untested", []))
    )

    installed = installed_folders(user_dir)

    for folder, is_enabled in installed:
        if folder in orig and not is_enabled:
            enable(folder, user_dir)
        elif folder in snapshot_pool and folder not in orig and is_enabled:
            disable(folder, user_dir)

    if os.path.exists(state_file):
        try:
            os.remove(state_file)
        except OSError:
            pass

    return {
        "stopped": True,
        "restored": sorted(list(orig)),
        "done": True,
    }


def selftest() -> bool:
    """Comprehensive offline self-test for pzbisect."""
    print("=== Chạy selftest pzbisect ===")
    with tempfile.TemporaryDirectory() as tmp:
        user = tmp
        mods_dir = os.path.join(user, MODS_DIRNAME)
        off_dir = os.path.join(user, OFF_DIRNAME)
        os.makedirs(mods_dir, exist_ok=True)
        os.makedirs(off_dir, exist_ok=True)

        # 1. Test path guards
        print("[1/9] Kiểm tra path traversal guard...")
        for bad in ["../evil", "foo/bar", "foo\\bar", "..", "."]:
            try:
                _check_folder_name(bad)
                raise AssertionError(f"Lỗi: Không bắt được path traversal {bad!r}")
            except ValueError:
                pass
        print("  -> PASS")

        # 2. Test name collision guard
        print("[2/9] Kiểm tra name collision guard...")
        os.makedirs(os.path.join(mods_dir, "DupMod"), exist_ok=True)
        os.makedirs(os.path.join(off_dir, "DupMod"), exist_ok=True)
        try:
            installed_folders(user)
            raise AssertionError("Lỗi: Không bắt được collision giữa mods/ và mods_off/")
        except RuntimeError:
            pass
        os.rmdir(os.path.join(off_dir, "DupMod"))
        os.rmdir(os.path.join(mods_dir, "DupMod"))
        print("  -> PASS")

        # 3. Test enable/disable idempotency + non-dir junk file guard
        print("[3/9] Kiểm tra enable/disable idempotency và non-dir junk file...")
        os.makedirs(os.path.join(mods_dir, "ModA"), exist_ok=True)
        os.makedirs(os.path.join(off_dir, "ModB"), exist_ok=True)

        # Create a non-dir file in mods
        with open(os.path.join(mods_dir, "junk.txt"), "w") as f:
            f.write("junk")

        assert installed_folders(user) == [("ModA", True), ("ModB", False)]

        enable("ModB", user)
        assert os.path.isdir(os.path.join(mods_dir, "ModB"))
        assert not os.path.exists(os.path.join(off_dir, "ModB"))
        enable("ModB", user)

        disable("ModA", user)
        assert os.path.isdir(os.path.join(off_dir, "ModA"))
        assert not os.path.exists(os.path.join(mods_dir, "ModA"))
        disable("ModA", user)

        assert installed_folders(user) == [("ModA", False), ("ModB", True)]

        # Trying to enable/disable non-dir file must raise FileNotFoundError
        try:
            enable("junk.txt", user)
            raise AssertionError("Lỗi: Cho phép enable file thông thường")
        except FileNotFoundError:
            pass

        print("  -> PASS")

        shutil.rmtree(mods_dir)
        shutil.rmtree(off_dir)
        os.makedirs(mods_dir, exist_ok=True)
        os.makedirs(off_dir, exist_ok=True)

        # 4. Test bisect min candidates guard
        print("[4/9] Kiểm tra bisect candidate count guard...")
        os.makedirs(os.path.join(mods_dir, "SoloMod"), exist_ok=True)
        try:
            bisect_start(user_dir=user)
            raise AssertionError("Lỗi: Cho phép bisect với < 2 candidates")
        except ValueError:
            pass
        os.rmdir(os.path.join(mods_dir, "SoloMod"))
        print("  -> PASS")

        # 5. Full bisect simulation with 7 mods, culprit is Mod_4
        print("[5/9] Giả lập toàn diện binary search bisect (7 mods, culprit = Mod_4)...")
        mod_names = [f"Mod_{i}" for i in range(7)]
        for m in mod_names:
            os.makedirs(os.path.join(mods_dir, m), exist_ok=True)

        culprit = "Mod_4"

        st = bisect_start(user_dir=user)
        assert st["round"] == 1
        assert st["candidates"] == mod_names
        assert not st["done"]

        max_steps = 10
        steps = 0
        while not st["done"] and steps < max_steps:
            steps += 1
            curr_enabled = st["enabled_now"]
            is_bad = culprit in curr_enabled
            st = bisect_mark(is_bad, user_dir=user)

        assert st["done"] is True
        assert st["suspect"] == culprit
        print(f"  -> Tìm thấy chính xác suspect '{culprit}' sau {st['round']} vòng.")

        stop_res = bisect_stop(user_dir=user)
        assert stop_res["stopped"] is True
        assert set(stop_res["restored"]) == set(mod_names)
        assert all(en for _, en in installed_folders(user))
        print("  -> PASS")

        # 6. Test bisect_stop mid-round layout restoration with partial enabled set
        print("[6/9] Kiểm tra khôi phục layout gốc giữa chừng (original mix on/off)...")
        for m in mod_names[:4]:
            enable(m, user)
        for m in mod_names[4:]:
            disable(m, user)

        expected_on = set(mod_names[:4])
        st = bisect_start(user_dir=user)
        st = bisect_mark(True, user_dir=user)
        bisect_stop(user_dir=user)

        now_on = {f for f, en in installed_folders(user) if en}
        assert now_on == expected_on, f"Layout khôi phục sai: {now_on} != {expected_on}"
        print("  -> PASS")

        # 7. Test bisect_start refusal when session already active (Fix 1)
        print("[7/9] Kiểm tra từ chối bisect_start khi đang có phiên chạy (Fix 1)...")
        st = bisect_start(user_dir=user)
        try:
            bisect_start(user_dir=user)
            raise AssertionError("Lỗi: Cho phép bisect_start đè phiên đang chạy")
        except RuntimeError:
            pass
        bisect_stop(user_dir=user)
        print("  -> PASS")

        # 8. Test bisect_stop fail-safe on corrupted state file (Fix 2)
        print("[8/9] Kiểm tra bisect_stop an toàn khi state file thiếu original_enabled (Fix 2)...")
        # Ensure some mods are enabled
        for m in mod_names[:3]:
            enable(m, user)
        state_file = os.path.join(user, STATE_FILENAME)
        # Write corrupted state file
        with open(state_file, "w", encoding="utf-8") as f:
            json.dump({"round": 1, "candidates": ["Mod_0", "Mod_1"]}, f)

        try:
            bisect_stop(user_dir=user)
            raise AssertionError("Lỗi: bisect_stop không chặn khi state thiếu original_enabled")
        except RuntimeError:
            pass
        # Ensure enabled mods were NOT disabled
        assert set(mod_names[:3]).issubset({f for f, en in installed_folders(user) if en})
        os.remove(state_file)
        print("  -> PASS")

        # 9. Test bisect_stop preserves newly installed mod during bisect session (Fix 3)
        print("[9/9] Kiểm tra bisect_stop giữ nguyên mod cài mới giữa phiên bisect (Fix 3)...")
        # Mod_0..Mod_2 ON, Mod_3..Mod_6 OFF
        for m in mod_names[:3]:
            enable(m, user)
        for m in mod_names[3:]:
            disable(m, user)

        st = bisect_start(user_dir=user)
        # User installs NewMod during bisect session into mods/
        os.makedirs(os.path.join(mods_dir, "NewMod_MidWay"), exist_ok=True)

        bisect_stop(user_dir=user)

        # NewMod_MidWay MUST still be in mods/ (enabled)
        assert os.path.isdir(os.path.join(mods_dir, "NewMod_MidWay")), "Lỗi: NewMod_MidWay bị tắt/xóa!"
        now_enabled = {f for f, en in installed_folders(user) if en}
        assert "NewMod_MidWay" in now_enabled
        assert set(mod_names[:3]).issubset(now_enabled)
        print("  -> PASS")

    print("=== TẤT CẢ TESTCASE ĐỀU PASS HOÀN TOÀN ===")
    return True


def main():
    args = sys.argv[1:]
    if not args:
        print(__doc__)
        return

    cmd = args[0].lower()

    if cmd == "selftest":
        ok = selftest()
        sys.exit(0 if ok else 1)

    elif cmd == "list":
        try:
            mods = installed_folders()
            if not mods:
                print("(không có mod nào được cài)")
                return
            print(f"Tổng cộng {len(mods)} mod:")
            for name, en in mods:
                status = "[BẬT]" if en else "[TẮT]"
                print(f"  {status:6s} {name}")
        except Exception as e:
            print(f"! Lỗi: {e}")
            sys.exit(1)

    elif cmd == "enable":
        if len(args) < 2:
            print("Cách dùng: pzbisect enable <folder_name> [...]")
            sys.exit(1)
        for f in args[1:]:
            try:
                enable(f)
                print(f"+ Đã bật: {f}")
            except Exception as e:
                print(f"! Lỗi khi bật {f}: {e}")
                sys.exit(1)

    elif cmd == "disable":
        if len(args) < 2:
            print("Cách dùng: pzbisect disable <folder_name> [...]")
            sys.exit(1)
        for f in args[1:]:
            try:
                disable(f)
                print(f"- Đã tắt: {f}")
            except Exception as e:
                print(f"! Lỗi khi tắt {f}: {e}")
                sys.exit(1)

    elif cmd == "start":
        names = args[1:] if len(args) > 1 else None
        try:
            st = bisect_start(names)
            print(f"=== BẮT ĐẦU BISECT (Vòng {st['round']}) ===")
            print(f"Candidates còn lại ({len(st['candidates'])}): {', '.join(st['candidates'])}")
            print(f"Đang bật để test ({len(st['enabled_now'])}): {', '.join(st['enabled_now'])}")
            print("-> Hãy mở game và kiểm tra xem lỗi còn xuất hiện không.")
            print("-> Sau đó chạy: 'pzbisect mark bad' (vẫn lỗi) hoặc 'pzbisect mark good' (hết lỗi).")
        except Exception as e:
            print(f"! Lỗi: {e}")
            sys.exit(1)

    elif cmd == "mark":
        if len(args) < 2:
            print("Cách dùng: pzbisect mark <bad|good>")
            sys.exit(1)
        val = args[1].lower()
        if val in ("bad", "true", "1", "loi", "error"):
            bad = True
        elif val in ("good", "false", "0", "ok", "hetloi"):
            bad = False
        else:
            print("! Tham số không hợp lệ. Dùng 'bad' (vẫn lỗi) hoặc 'good' (hết lỗi).")
            sys.exit(1)

        try:
            st = bisect_mark(bad)
            if st.get("done"):
                print("==================================================")
                print(f"🎯 ĐÃ TÌM THẤY MOD GÂY LỖI: {st.get('suspect')}")
                print("==================================================")
                print("-> Chạy 'pzbisect stop' để khôi phục lại toàn bộ mod ban đầu.")
            else:
                print(f"=== TIẾP TỤC BISECT (Vòng {st['round']}) ===")
                print(f"Candidates còn lại ({len(st['candidates'])}): {', '.join(st['candidates'])}")
                print(f"Đang bật để test ({len(st['enabled_now'])}): {', '.join(st['enabled_now'])}")
                print("-> Hãy mở game và kiểm tra lại.")
        except Exception as e:
            print(f"! Lỗi: {e}")
            sys.exit(1)

    elif cmd == "state":
        try:
            st = bisect_state()
            print(json.dumps(st, indent=2, ensure_ascii=False))
        except Exception as e:
            print(f"! Lỗi: {e}")
            sys.exit(1)

    elif cmd == "stop":
        try:
            res = bisect_stop()
            if res.get("stopped"):
                print("Đã dừng bisect và khôi phục trạng thái ban đầu.")
            else:
                print(res.get("message", "Không có phiên bisect đang chạy."))
        except Exception as e:
            print(f"! Lỗi: {e}")
            sys.exit(1)

    else:
        print(f"Lệnh không hợp lệ: {cmd}")
        print("Các lệnh hỗ trợ: list, enable, disable, start, mark, state, stop, selftest")
        sys.exit(1)


if __name__ == "__main__":
    main()
