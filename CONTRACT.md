# pzmod — Project Zomboid workshop mod manager (CCAS team build)

Base: clone of `D:\Tools\DDMods` (ddmod). Read `ddmod.py` + `ddmod_gui.py` + `ui.html` FIRST — 80% reusable, only PZ deltas below are new work.

## Verified facts (Claire, 2026-08-29 — do not re-test)

- steamcmd anon download for PZ works. appid **108600**.
  `E:\steamcmd\steamcmd.exe +force_install_dir <dir> +login anonymous +workshop_download_item 108600 <id> +quit`
  → `Success. Downloaded item ... (46912811 bytes)`; verified item 498441420 Hydrocraft, 53M on disk.
- Keyless `ISteamRemoteStorage/GetPublishedFileDetails` + `steamcommunity.com/workshop/browse` both reachable from Yang machine now (no Warp needed at this moment; ddmod cache/rate-limit rules still apply — keep them).
- Game: `D:\ProjectZomboid` — cracked/OnlineFix build, **B41** (no `media/lua/shared/Definitions/AnimalDefinitions.lua`), git rev `b0bbce05d5`.
- Workshop item layout: `<content>/108600/<wid>/mods/<ModName>/mod.info` (one item MAY carry several `<ModName>` dirs). B42 items may add a build subdir: `mods/<ModName>/<41|42>/mod.info`.
- `mod.info` keys seen: `name=`, `id=`, `poster=`, `description=`, `pack=`, plus `require=` (comma list of **mod ids**, not workshop ids), `url=`.
- Workshop tag vocabulary for PZ = `D:\ProjectZomboid\media\WorkshopTags.txt` (Build 40/41/42, Animals, Audio, Balance, Building, Clothing/Armor, Farming, Food, Framework, Hardmode, Interface, Items, Language/Translation, Literature, Map, Military, Misc, Models, Multiplayer, Pop Culture, Realistic, Silly/Fun, Skills, Textures, Traits, Vehicles, QoL, WIP, Weapons).
- `C:\Users\Asus1\Zomboid` does NOT exist yet (game never launched). Create it on demand.

## Paths (single source of truth, defined in pzmod.py)

```python
APPID = "108600"
GAME  = os.environ.get("PZ_GAME", r"D:\ProjectZomboid")
USER  = os.environ.get("PZ_USER", os.path.join(os.path.expanduser("~"), "Zomboid"))
MODS  = os.path.join(USER, "mods")        # install target, created if missing
OFF   = os.path.join(USER, "mods_off")    # disabled mods parked here (bisect)
STATE = os.path.join(USER, ".pzmod.json")
```

## State file format (`.pzmod.json`)

```json
{"<workshop_id>": {"title": "...", "updated": 1690000000, "size": 46912811,
                   "folders": ["Hydrocraft"], "modids": ["Hydrocraft"],
                   "require": ["OtherModId"],
                   "resolved": {"OtherModId": "<workshop_id>"}}}
```
`folders` = dir names created under MODS (one item can install several). `modids` = `id=` from each mod.info. `require` = union of `require=` values. `resolved` caches each mod id as a verified workshop id, or `null` when the top three search candidates did not provide an exact `id=` match.

## Work split

One lane only. Yang retired the Python lane on 30/08/2026: the product is the
Tauri GUI, and a parallel CLI twin doubled dev, review and test while producing
lane-divergence bugs of its own. `pzmod.py`, `pzmod_gui.py` and `pzbisect.py` are
gone; `main.rs` is the whole app. Cost accepted knowingly: there is no terminal
surface any more, and `pzmod.py::selftest` (with its live `details(498441420)`
check) died with it — `cargo test --bins` is the only suite left.

| who | files | scope |
|---|---|---|
| Claire | CONTRACT.md, git, integration, final review | this doc; merges; reports to Yang; measures and diagnoses, then hands the fix out |
| Celine | `pzmod-rs/src/main.rs` | core: search/info/install/remove/list/update/deps + cargo tests |
| Ariel | `main.rs` bisect block (`bisect_*_internal`, ~2726-3031) | enable/disable + binary-search culprit finder; plus scrape-budget and rate-limit audit |
| Selica | `ui.html` | web UI: detail sheet, batch install, enable/disable toggles, bisect panel, launch button |

Celine and Ariel share `main.rs`. Split by block, not by file: Ariel owns the
bisect functions and their tests, Celine owns the rest. Touching the other's
block needs a word on the bridge first, same rule as a separate file.

`pzmod-rs/ui/` IS A BUILD ARTIFACT. `sync-ui.py` generates `ui/index.html` from
`ui.html` and `dev.bat` runs it before every `cargo run`. Never hand-edit
anything under `pzmod-rs/ui/` — the next build overwrites it. `ui.html` at the
repo root is the only UI source. (Claire got this wrong on 30/08/2026 and told
Selica to port a fix into the generated file; it is written down here so nobody
repeats it.)

`sync-ui.py` and `fetch-fonts.py` stay Python. They are build tooling, not the
retired lane.

**Nobody edits someone else's file without that owner saying so on the bridge.**
Claire diagnosing is not Claire implementing: a measurement goes to the bridge
with the numbers attached, and the owner writes the fix. Claire may commit
directly only for CONTRACT.md, merges, and one-to-two-line obvious repairs.

Copies of ddmod GUI already staged in this folder as `pzmod_gui.py` + `ui.html` + `tokens.css` (still DD-branded — Selica rewrites).

### Celine — pzmod.py

Start from `ddmod.py`, keep: `post/details/children/browse/cached_page/requires/Blocked/cache+rate-limit/strip_bb/maybe_id/as_id/steamcmd()/download()`.
Change:
1. `install_one(wid)`: after `download()`, scan item dir for mod roots = every dir whose child is `mod.info`, OR (B42) whose child `<41|42>/mod.info` exists; the mod root is the dir directly under `mods/`. Copy each root to `MODS/<ModName>` (dir name kept as-is — PZ matches mods by folder/id, NOT by title; never rename). Overwrite = rmtree old folders from state first.
2. `parse_modinfo(path) -> dict` (key=value lines, `#` comments, last wins; tolerate BOM + CRLF + non-UTF8 → decode utf-8 errors=replace).
3. Deps, two sources — both must run:
   - workshop `Required Items` scrape (ddmod `requires()`, reuse as-is) → workshop ids, install those first (post-order like ddmod `with_deps`).
   - after download: union of `require=` mod ids across the install queue minus known `modids` → search Workshop with `_` changed to spaces (raw mod id only when that search is empty), inspect at most the top three downloads, and accept only an exact `id=` match. Install verified matches dependency-first, recurse with the existing depth cap, and cache matches/misses in `resolved`. Never install a search guess. Any unresolved ids still print as `! thiếu mod bắt buộc: <ids>` and remain exposed to the GUI.
4. `list`/`update`/`remove` operate on `folders`, and must also clean the same folder if it sits in `OFF` (disabled).
5. Keep `selftest` in ddmod style: pure-logic asserts + parse_modinfo cases + one live details() check on 498441420 (`consumer_app_id == 108600`), network parts skippable on `Blocked`.
6. CLI surface (bat wrappers `pzmod.bat`, `pzmod-gui.bat` — copy DD ones, rename): `search info install remove list update reinstall enable disable bisect selftest`. `enable/disable/bisect` delegate to Ariel's module.

### Ariel — pzbisect.py

PZ stores its active-mod list inside save data; do NOT parse it. Enable/disable = filesystem move between `MODS` and `OFF` (`shutil.move`, same volume). Public API for Celine + Selica:

```python
installed_folders() -> [(folder, enabled: bool)]
enable(folder) / disable(folder)      # idempotent, no-op if already there
bisect_start(names=None) -> dict      # snapshot enabled set as candidates; save .pzbisect.json in USER
bisect_mark(bad: bool) -> dict        # bad = bug still happens with current split; narrow, apply next split
bisect_state() -> dict                # {round, candidates, enabled_now, suspect|None, done}
bisect_stop() -> dict                 # restore original enabled set exactly
```
Semantics: single-culprit binary search over the candidate list. Each round enables candidate half A, disables the rest of the candidates (mods outside the snapshot stay untouched). `bad=True` → culprit inside the enabled half; `bad=False` → culprit in the disabled half. Finish when 1 candidate left → `suspect`. Every state change writes `.pzbisect.json` (round, candidates, halves, original set) so a crashed session can resume. `bisect_stop` MUST restore the exact original enabled/disabled layout even mid-round.
Guards: refuse to move anything outside MODS/OFF; refuse if a name collides in both dirs; require ≥2 candidates to start. Ship a selftest that runs the whole bisect on a temp dir (fake mod folders, scripted culprit) with no game and no network.

### Selica — pzmod_gui.py + ui.html

Rebrand ddmod→pzmod (title, brand, footer, Vietnamese copy). Keep: img proxy allowlist, `_work` lock, `capture()`, loopback bind, port → **8773** (8772 is ddmod).
Add endpoints: `POST /api/enable {folder}`, `POST /api/disable {folder}`, `POST /api/bisect {op: start|bad|good|stop}`, `GET /api/bisect` (state). `snapshot()` must also return installed folders + enabled flag + missing-require list.
UI adds:
1. "Đã cài" rows: toggle Bật/Tắt per folder, red badge when a `require=` dep is missing.
2. Bisect panel (own tab): start / "vẫn lỗi" / "hết lỗi" / dừng, shows round N, how many candidates left, which mods are on this round, and the suspect when found. Copy explains the loop: bấm → mở game thử → trả lời.
3. Detail sheet: also show mod ids + required items already installed or not.
Keep the existing Hallmark styling and the bulk-install bar as-is. No new deps, stdlib + vanilla JS only.

## Rules

- Read the ddmod original before writing; do not reinvent what it solved (rate limit, stale cache, safe names, steamcmd cache busting).
- Vietnamese user-facing strings, English code comments.
- Every module keeps a runnable `selftest`.
- Do not touch `D:\Tools\DDMods`.
- Commit in `D:\Tools\PZMods` (git already init'd by Claire). One commit per module, message in Conventional Commits.
