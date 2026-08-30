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

## Authenticated Workshop browse

Steam filters mature mods server-side for anonymous users. pzmod reads the
`steamLoginSecure` value from `%LOCALAPPDATA%\pzmod\steam-cookie.txt` and sends
it only to `steamcommunity.com`; the value must come from that domain, not
`store.steampowered.com` (`aud: web:community`, not `aud: web:store`). Keep one
trimmed value in the file, never commit or log it.

The token expires and is bound to the egress IP used when Steam issued it.
Recreate the file after expiry, a Steam logout, or a Warp/VPN IP change. The
value is read once at startup, so restart pzmod after replacing the file. A
logged-out Workshop response is an authentication error rather than a valid
partial listing. Cache keys are partitioned by whether authentication is
configured so old anonymous pages cannot hide mature results.

Acceptance case: an authenticated text search for `Tomb Player Body Overhaul`
under appid `108600` includes Workshop item `3429790870`; its Required Items
block resolves dependency `3431734923`.

## Paths (single source of truth: `paths()`, pzmod-rs/src/main.rs:179)

| name | value |
|---|---|
| APPID | `108600` |
| GAME | env `PZ_GAME`, default `D:\ProjectZomboid` |
| USER | env `PZ_USER`, default `%USERPROFILE%\Zomboid` |
| MODS | `<USER>\mods` — install target, created if missing |
| OFF | `<USER>\mods_off` — disabled mods parked here (bisect) |
| STATE | `<USER>\.pzmod.json` |

`dev.bat` sources `pz-paths.bat`, so `PZ_USER` is always set while developing;
tests point both vars at a throwaway tree instead of the live game profile.

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

`pzmod-rs/ui/` is MOSTLY generated. `sync-ui.py` writes `index.html`,
`tokens.css`, `fonts.css` and `fonts/` there from the repo root, and `dev.bat`
runs it before every `cargo run` — hand-editing any of those is wasted work, the
next build overwrites it. `ui.html` at the repo root is the only UI source.
(Claire got this wrong on 30/08/2026 and told Selica to port a fix into the
generated `index.html`; written down so nobody repeats it.)

THE ONE EXCEPTION: `pzmod-rs/ui/bridge.js` is hand-written source that happens to
live in that folder — `git ls-files pzmod-rs/ui/` returns it and nothing else.
`sync-ui.py` only injects a `<script src="bridge.js">` tag; it never writes the
file. Owner: **Selica**, as UI-side glue. It holds the `/api/<x>` -> command map,
so adding or renaming a `#[tauri::command]` means updating that map and
`ROUTES.md` **in the same commit** — a route with no map entry is a dead button
that fails only at runtime, and no test catches it. (Claire's rule here first read
"never hand-edit anything under `pzmod-rs/ui/`", which banned editing the one
source file in there. Corrected 30/08/2026.)

`sync-ui.py` and `fetch-fonts.py` stay Python. They are build tooling, not the
retired lane.

**Nobody edits someone else's file without that owner saying so on the bridge.**
Claire diagnosing is not Claire implementing: a measurement goes to the bridge
with the numbers attached, and the owner writes the fix. Claire may commit
directly only for CONTRACT.md, merges, and one-to-two-line obvious repairs.

Origin: `ui.html` + `tokens.css` started as ddmod copies and were rebranded in place.

### Celine — core commands in `main.rs`

Owns `state browse detail install remove update prefetch progress launch` and
everything they call: HTTP, cache + rate limit, steamcmd, state file, deps.
1. `install`: after the steamcmd download, scan item dir for mod roots = every dir whose child is `mod.info`, OR (B42) whose child `<41|42>/mod.info` exists; the mod root is the dir directly under `mods/`. Copy each root to `MODS/<ModName>` (dir name kept as-is — PZ matches mods by folder/id, NOT by title; never rename). Overwrite = remove the old folders recorded in state first. A root whose
   name is unsafe is skipped with a warning pushed into the progress stream —
   never an abort of the whole item (one odd folder must not block the mods
   queued behind it).
2. `parse_modinfo` (key=value lines, `#` comments, last wins; tolerate BOM + CRLF + non-UTF8 → decode utf-8 errors=replace).
3. Deps, two sources — both must run:
   - workshop `Required Items` scrape (ddmod `requires()`, reuse as-is) → workshop ids, install those first (post-order like ddmod `with_deps`).
   - after download: union of `require=` mod ids across the install queue minus known `modids` → search Workshop with `_` changed to spaces (raw mod id only when that search is empty), inspect at most the top three downloads, and accept only an exact `id=` match. Install verified matches dependency-first, recurse with the existing depth cap, and cache matches/misses in `resolved`. Never install a search guess. Any unresolved ids still print as `! thiếu mod bắt buộc: <ids>` and remain exposed to the GUI.
4. `list`/`update`/`remove` operate on `folders`, and must also clean the same folder if it sits in `OFF` (disabled).
5. Tests live in `mod tests` inside `main.rs` and run under `cargo test --bins`:
   pure-logic asserts, parse_modinfo cases, fixture-driven scrape parsing. Nothing
   may need the network or the real game to pass. The old live `details(498441420)`
   check died with the Python lane and has no replacement — say so, do not fake it.

### Ariel — bisect block in `main.rs` (~2726-3031)

PZ stores its active-mod list inside save data; do NOT parse it. Enable/disable = a
filesystem move between `MODS` and `OFF` (same volume). Surface:

```
#[tauri::command] enable(folder) / disable(folder)   // idempotent, "already there" = success
#[tauri::command] bisect(op, names)                  // op = start | bad | good | stop | none = state
   bisect_start_internal(names, user)   // snapshot enabled set; write .pzbisect.json in USER
   bisect_mark_internal(bad, user)      // bad = bug still happens with this split; narrow, re-split
   bisect_state_internal(user)          // {round, candidates, enabled_now, suspect|None, done}
   bisect_stop_internal(user)           // restore original enabled set exactly
```
Semantics: single-culprit binary search over the candidate list. Each round enables candidate half A, disables the rest of the candidates (mods outside the snapshot stay untouched). `bad=True` → culprit inside the enabled half; `bad=False` → culprit in the disabled half. Finish when 1 candidate left → `suspect`. Every state change writes `.pzbisect.json` (round, candidates, halves, original set) so a crashed session can resume. `bisect_stop` MUST restore the exact original enabled/disabled layout even mid-round.
Guards: refuse to move anything outside MODS/OFF; refuse if a name collides in both
dirs; require ≥2 candidates to start. `.pzbisect.json` is user-editable, so every name
read back out of it is re-checked before it is moved. Tests run the whole bisect on a
temp dir (fake mod folders, scripted culprit) with no game and no network.

User-facing strings point at the GUI. There is no CLI to tell anyone to run.

### Selica — ui.html

`ui.html` at the repo root is the only UI source. It talks to Rust through
`bridge.js`, which `sync-ui.py` injects while generating `pzmod-rs/ui/index.html`:
an old `fetch('/api/x')` becomes `invoke('x')`. Route map: `pzmod-rs/ROUTES.md`.
The `state` command returns installed folders + enabled flag + missing-require list.
UI carries:
1. "Đã cài" rows: toggle Bật/Tắt per folder, red badge when a `require=` dep is missing.
2. Bisect panel (own tab): start / "vẫn lỗi" / "hết lỗi" / dừng, shows round N, how many candidates left, which mods are on this round, and the suspect when found. Copy explains the loop: bấm → mở game thử → trả lời.
3. Detail sheet: also show mod ids + required items already installed or not.
Keep the existing Hallmark styling and the bulk-install bar as-is. No new deps,
vanilla JS only.

## Rules

- Read the ddmod original before writing; do not reinvent what it solved (rate limit, stale cache, safe names, steamcmd cache busting).
- Vietnamese user-facing strings, English code comments.
- Every change ships tests in `mod tests`; `cargo test --bins` is the only suite left.
- Do not touch `D:\Tools\DDMods`.
- Commit in `D:\Tools\PZMods` (git already init'd by Claire). One commit per module,
  message in Conventional Commits.
- `main.rs` is shared. Run `git status` before every commit; if someone else's file or
  block shows as modified, stop and ask on the bridge. Stage by path, never
  `git commit -a`. A commit message must describe everything inside the commit — a
  message narrower than its diff breaks `git log`, `git blame` and `git bisect`,
  which is the very tool we are building for Yang. (Happened 30/08/2026.)
