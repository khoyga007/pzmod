# pzmod: Python -> Rust port map

Single source of UI: `../ui.html`. `sync-ui.py` copies it to `ui/index.html`
and injects `bridge.js` (fetch -> `invoke`). Never edit `ui/index.html`.

Build: `build.bat` / `dev.bat`. Both call vcvars64 first — see memory
`rust-build-env-windows` (git-bash `link.exe` shadows MSVC; LNK1181 without LIB).

## Route table

| UI fetch | Python (`pzmod_gui.py`) | Rust command | State |
|---|---|---|---|
| `GET /api/state` | `snapshot()` | `state` | partial — folders only; no workshop entries, tags, sorts |
| `POST /api/enable` | `pzbisect.enable` | `enable` | done |
| `POST /api/disable` | `pzbisect.disable` | `disable` | done |
| `GET /api/browse` | `pzmod.browse()` HTML scrape | `browse` | done |
| `GET /api/detail` | `pzmod.details()` + `requires()` | `detail` | done |
| `POST /api/install` | steamcmd + `with_deps` | — | todo, `Command` spawn |
| `POST /api/update` / `/api/remove` | state-file lane | — | todo |
| `POST /api/bisect` | `pzbisect.bisect_*` | — | todo, blocked on Selica's 4 findings |
| `GET /img?u=` | CDN proxy + allowlist | none needed | webview loads Steam CDN direct (`csp: null`) |
| `GET /tokens.css` | file serve | none needed | shipped inside `ui/` |

Unmapped routes return a "chưa port" error from `bridge.js`, so the Python GUI
stays the working build until the table is full.

## Ported invariants (do not drop)

- folder name from webview = untrusted: no separators, no `.`/`..`, no drive letter.
- both `mods/` and `mods_off/` holding the same name = refuse, never delete either.
- toggle is idempotent; "already there" is success, not an error.
- `isdir`, not `exists` — a stray file must not read as an installed mod.
- `PZ_GAME` / `PZ_USER` env overrides, same defaults as `pzmod.py`.

## Next

1. steamcmd lane: `std::process::Command`, same argv as `pzmod.py`.
2. bisect lane: port only AFTER Ariel's fixes land — the Python version is the spec.
