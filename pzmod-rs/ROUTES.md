# pzmod: Python -> Rust port map

Single source of UI: `../ui.html`. `sync-ui.py` copies it to `ui/index.html`
and injects `bridge.js` (fetch -> `invoke`). Never edit `ui/index.html`.

Development: use `dev.bat`; it calls vcvars64 first — see memory
`rust-build-env-windows` (git-bash `link.exe` shadows MSVC; LNK1181 without LIB).
Do not run `build.bat` / release builds unless Yang explicitly asks.

## Route table

| UI fetch | Python (`pzmod_gui.py`) | Rust command | State |
|---|---|---|---|
| `GET /api/state` | `snapshot()` | `state` | done |
| `POST /api/enable` | `pzbisect.enable` | `enable` | done |
| `POST /api/disable` | `pzbisect.disable` | `disable` | done |
| `GET /api/browse` | `pzmod.browse()` HTML scrape | `browse` | done |
| `GET /api/detail` | `pzmod.details()` + `requires()` | `detail` | done |
| `POST /api/install` | steamcmd + `with_deps` | `install` | done |
| `POST /api/update` / `/api/remove` | state-file lane | `update` / `remove` | done |
| `POST /api/bisect` | `pzbisect.bisect_*` | `bisect` | done |
| `POST /api/prefetch` | `pzmod.prefetch()` background warm | `prefetch` | done |
| `POST /api/launch` | `launch_game()` -> PZ-D.bat | `launch` | done |
| `GET /api/steam` | none (new) | `steam` | done — mở cửa sổ Steam nhúng; cũng là nguồn cookie phiên |
| `GET /img?u=` | CDN proxy + allowlist | none needed | webview loads Steam CDN direct (`csp: null`) |
| `GET /tokens.css` | file serve | none needed | shipped inside `ui/` |

Table is full and the Python lane is gone (Yang, 30/08/2026). The column headed
`Python` is kept as history — those files no longer exist. `bridge.js` still errors
on an unmapped route, which now means a bug, not a pending port.

## Ported invariants (do not drop)

- folder name from webview = untrusted: no separators, no `.`/`..`, no drive letter.
- both `mods/` and `mods_off/` holding the same name = refuse, never delete either.
- toggle is idempotent; "already there" is success, not an error.
- `isdir`, not `exists` — a stray file must not read as an installed mod.
- `PZ_GAME` / `PZ_USER` env overrides; defaults `D:\ProjectZomboid` and `%USERPROFILE%\Zomboid`.

## Next

All Python GUI routes used by `ui.html` are now mapped.
