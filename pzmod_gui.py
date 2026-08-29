#!/usr/bin/env python3
"""pzmod_gui - local web UI for pzmod. Binds 127.0.0.1 only; stdlib only.

    python pzmod_gui.py [port]
"""
import contextlib
import io
import json
import os
import re
import sys
import threading
import webbrowser
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import parse_qs, quote, urlparse

import pzmod

HERE = os.path.dirname(os.path.abspath(__file__))
PORT = int(sys.argv[1]) if len(sys.argv) > 1 else 8773  # 8772 is ddmod

# Thumbnails are proxied rather than hotlinked so the page works in browsers that
# block third-party images. The allowlist is what keeps /img from being an open
# SSRF hole - only Steam's own image CDNs, https only, no redirects followed.
IMG_HOSTS = frozenset({"images.steamusercontent.com", "steamuserimages-a.akamaihd.net",
                       "community.cloudflare.steamstatic.com",
                       "community.akamai.steamstatic.com"})
_img_cache = {}
_img_lock = threading.Lock()

# steamcmd is a single process with one workshop cache, and enable/disable moves
# folders on disk - one lock over every state-touching command keeps that honest.
_work = threading.Lock()


def bisect_mod():
    """pzbisect, or None when the module is missing - the GUI still has to boot."""
    try:
        import pzbisect
        return pzbisect
    except ImportError:
        return None


def capture(fn, *args):
    """Run a pzmod command, returning its printed lines. pzmod.die() raises SystemExit
    (bad id, steamcmd failure) - that must surface as a failed request, not kill the server."""
    buf = io.StringIO()
    try:
        with contextlib.redirect_stdout(buf):
            fn(*args)
        lines = buf.getvalue().splitlines()
        ok = not any(l.strip().startswith(("!", "ERROR")) for l in lines)
        return {"ok": ok, "log": lines}
    except SystemExit as e:
        return {"ok": False, "log": buf.getvalue().splitlines(), "error": str(e) or "aborted"}
    except Exception as e:
        return {"ok": False, "log": buf.getvalue().splitlines(),
                "error": "%s: %s" % (type(e).__name__, e)}


def snapshot():
    state = pzmod.read_state()
    installed = {}
    for wid, entry in state.items():
        folders = [{"name": f, "status": pzmod.folder_status(f)}
                   for f in entry.get("folders", []) if isinstance(f, str)]
        installed[wid] = {"title": entry.get("title") or wid,
                          "size": entry.get("size") or 0,
                          "updated": entry.get("updated") or 0,
                          "modids": entry.get("modids", []),
                          "require": entry.get("require", []),
                          "folders": folders,
                          "present": any(f["status"] in ("enabled", "disabled", "collision")
                                         for f in folders)}
    module = bisect_mod()
    # Folders on disk that no workshop item claims: mods copied in by hand.
    known = {f["name"] for e in installed.values() for f in e["folders"]}
    loose = []
    if module:
        loose = [{"name": n, "enabled": on} for n, on in module.installed_folders()
                 if n not in known]
    return {"game": pzmod.GAME, "mods": pzmod.MODS, "off": pzmod.OFF,
            "sorts": pzmod.SORTS, "tags": pzmod.TAGS,
            "installed": installed,
            "loose": loose,
            "missing": pzmod.missing_requirements(state),
            "bisect_ready": module is not None}


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, *a):
        return None  # a redirect could walk off the allowlist


_img_opener = urllib.request.build_opener(NoRedirect)

MAGIC = [(b"\xff\xd8\xff", "image/jpeg"), (b"\x89PNG\r\n\x1a\n", "image/png"),
         (b"GIF8", "image/gif"), (b"BM", "image/bmp")]


def sniff(blob):
    """Content type from magic bytes, or None if this is not an image we will relay."""
    for magic, ctype in MAGIC:
        if blob.startswith(magic):
            return ctype
    if blob[:4] == b"RIFF" and blob[8:12] == b"WEBP":
        return "image/webp"
    return None


def fetch_img(url):
    """-> (bytes, content-type). Raises on anything not an https Steam CDN image."""
    p = urlparse(url)
    if p.scheme != "https" or p.hostname not in IMG_HOSTS:
        raise ValueError("blocked image host: %s" % p.hostname)
    with _img_lock:
        if url in _img_cache:
            return _img_cache[url]
    with _img_opener.open(urllib.request.Request(url, headers=pzmod.UA), timeout=30) as r:
        blob = r.read(8 * 1024 * 1024)
    # Steam hands these back as application/octet-stream, so sniff the magic bytes
    # instead of trusting the header - and refuse to relay anything that is not an image.
    ctype = sniff(blob)
    if not ctype:
        raise ValueError("not an image")
    with _img_lock:
        if len(_img_cache) > 400:
            _img_cache.clear()  # ponytail: whole-cache flush, an LRU buys nothing here
        _img_cache[url] = (blob, ctype)
    return blob, ctype


def card(d):
    preview = d.get("preview_url") or ""
    return {"id": d["publishedfileid"],
            "title": d.get("title") or "",
            "preview": ("/img?u=" + quote(preview, safe="")) if preview else "",
            "subs": d.get("subscriptions") or 0,
            "size": int(d.get("file_size") or 0),
            "updated": d.get("time_updated") or 0,
            "tags": [t["tag"] for t in d.get("tags", [])],
            "summary": pzmod.strip_bb(d.get("description")).replace("\r", "").strip()[:220]}


def listing(q, sort, page, tags):
    ids = pzmod.browse(q, sort, page, tags)
    meta = pzmod.details(ids)
    # Steam pins its own "Modding Policy" notice to the top of every listing,
    # tag filter or not. Drop anything that lacks the tags that were asked for.
    want = set(tags)
    def keeps(item):
        return want <= {t.get("tag") for t in item.get("tags", [])}
    # Steam's own ordering is the useful one; details() comes back unordered.
    return [card(meta[i]) for i in ids if i in meta and keeps(meta[i])]


def detail(wid):
    d = pzmod.details([wid]).get(wid)
    if not d:
        return {"error": "not found"}
    out = card(d)
    out["description"] = pzmod.strip_bb(d.get("description")).replace("\r", "")
    out["children"] = len(pzmod.children(wid))
    out["created"] = d.get("time_created") or 0
    out["views"] = d.get("views") or 0
    out["favorited"] = d.get("favorited") or 0
    state = pzmod.read_state()
    entry = state.get(wid, {})
    out["folders"] = [{"name": f, "status": pzmod.folder_status(f)}
                      for f in entry.get("folders", []) if isinstance(f, str)]
    out["modids"] = entry.get("modids", [])
    # Required Items lives on the workshop page, so it is known before installing.
    try:
        req = pzmod.requires(wid)
    except pzmod.Blocked:
        req = []
        out["req_blocked"] = True
    meta = pzmod.details(req) if req else {}
    out["required"] = [{"id": r, "title": (meta.get(r) or {}).get("title") or r,
                        "installed": bool(state.get(r))} for r in req]
    return out


def bisect_view():
    """Bisect state plus the split the UI has to explain, or why it is unavailable."""
    module = bisect_mod()
    if not module:
        return {"ready": False, "error": "thiếu pzbisect.py"}
    st = module.bisect_state()
    enabled = set(st.get("enabled_now") or [])
    cands = st.get("candidates") or []
    # bisect_state does not name the halves; the enabled candidates ARE this round's half.
    st["tested"] = [c for c in cands if c in enabled]
    st["untested"] = [c for c in cands if c not in enabled]
    st["ready"] = True
    st["running"] = bool(cands) and not st.get("done")
    return st


class Handler(BaseHTTPRequestHandler):
    def log_message(self, *a):
        pass  # ponytail: the UI reports its own errors; access logs add nothing here

    def send(self, body, ctype="application/json; charset=utf-8", code=200):
        blob = body if isinstance(body, bytes) else json.dumps(body).encode("utf-8")
        self.send_response(code)
        self.send_header("Content-Type", ctype)
        self.send_header("Content-Length", str(len(blob)))
        self.send_header("Cache-Control", "no-store")
        self.end_headers()
        self.wfile.write(blob)

    def file(self, name, ctype):
        with open(os.path.join(HERE, name), "rb") as f:
            self.send(f.read(), ctype)

    def do_GET(self):
        url = urlparse(self.path)
        qs = parse_qs(url.query)
        one = lambda k, dflt="": (qs.get(k) or [dflt])[0]
        try:
            if url.path in ("/", "/index.html"):
                return self.file("ui.html", "text/html; charset=utf-8")
            if url.path == "/tokens.css":
                return self.file("tokens.css", "text/css; charset=utf-8")
            if url.path == "/fonts.css":
                return self.file("fonts.css", "text/css; charset=utf-8")
            # Fonts are self-hosted so the app draws Vietnamese offline. Only bare
            # .woff2 names out of fonts/ — the name never leaves that directory.
            if url.path.startswith("/fonts/"):
                name = url.path[len("/fonts/"):]
                if not re.fullmatch(r"[A-Za-z0-9._-]+\.woff2", name):
                    return self.send({"error": "font không hợp lệ"}, code=400)
                return self.file(os.path.join("fonts", name), "font/woff2")
            if url.path == "/img":
                try:
                    blob, ctype = fetch_img(one("u"))
                except ValueError as e:
                    return self.send({"error": str(e)}, code=400)
                except Exception as e:
                    return self.send({"error": str(e)}, code=502)
                self.send_response(200)
                self.send_header("Content-Type", ctype)
                self.send_header("Content-Length", str(len(blob)))
                self.send_header("Cache-Control", "max-age=86400")
                self.end_headers()
                return self.wfile.write(blob)
            if url.path == "/api/state":
                return self.send(snapshot())
            if url.path == "/api/bisect":
                return self.send(bisect_view())
            if url.path == "/api/browse":
                page = max(1, min(50, int(one("page", "1") or 1)))
                sort = one("sort", "trend")
                if sort not in pzmod.SORTS:
                    sort = "trend"
                tags = [t for t in qs.get("tag", []) if t in pzmod.TAGS]
                try:
                    items = listing(one("q"), sort, page, tags)
                except pzmod.Blocked as e:
                    return self.send({"error": str(e)}, code=503)
                return self.send({"items": items, "page": page})
            if url.path == "/api/detail":
                wid = one("id")
                if not wid.isdigit():
                    return self.send({"error": "bad id"}, code=400)
                return self.send(detail(wid))
            self.send({"error": "not found"}, code=404)
        except Exception as e:
            self.send({"error": "%s: %s" % (type(e).__name__, e)}, code=500)

    def toggle(self, folder, on):
        module = bisect_mod()
        if not module:
            return self.send({"error": "thiếu pzbisect.py"}, code=500)
        try:
            (module.enable if on else module.disable)(folder)
        except (ValueError, RuntimeError, OSError) as e:
            return self.send({"ok": False, "log": [], "error": str(e)})
        return self.send({"ok": True, "log": ["%s %s" % ("bật" if on else "tắt", folder)]})

    def do_POST(self):
        url = urlparse(self.path)
        try:
            n = int(self.headers.get("Content-Length") or 0)
            body = json.loads(self.rfile.read(n) or b"{}")
        except Exception:
            return self.send({"error": "bad request body"}, code=400)
        wid = str(body.get("id") or "")
        folder = body.get("folder")
        try:
            if url.path in ("/api/install", "/api/remove") and not wid.isdigit():
                return self.send({"error": "bad id"}, code=400)
            with _work:
                if url.path == "/api/install":
                    return self.send(capture(pzmod.cmd_install, [wid], bool(body.get("force"))))
                if url.path == "/api/remove":
                    return self.send(capture(pzmod.cmd_remove, [wid]))
                if url.path == "/api/update":
                    return self.send(capture(pzmod.cmd_update, []))
                if url.path in ("/api/enable", "/api/disable"):
                    if not isinstance(folder, str) or not folder:
                        return self.send({"error": "bad folder"}, code=400)
                    return self.toggle(folder, url.path.endswith("enable"))
                if url.path == "/api/bisect":
                    module = bisect_mod()
                    if not module:
                        return self.send({"error": "thiếu pzbisect.py"}, code=500)
                    op = str(body.get("op") or "")
                    try:
                        if op == "start":
                            module.bisect_start(body.get("names") or None)
                        elif op == "bad":
                            module.bisect_mark(True)
                        elif op == "good":
                            module.bisect_mark(False)
                        elif op == "stop":
                            module.bisect_stop()
                        else:
                            return self.send({"error": "bad op"}, code=400)
                    except (ValueError, RuntimeError, OSError) as e:
                        return self.send({"ok": False, "error": str(e), "state": bisect_view()})
                    return self.send({"ok": True, "state": bisect_view()})
            self.send({"error": "not found"}, code=404)
        except Exception as e:
            self.send({"error": "%s: %s" % (type(e).__name__, e)}, code=500)


if __name__ == "__main__":
    if not os.path.isdir(pzmod.GAME):
        pzmod.die("không thấy thư mục game: %s (đặt PZ_GAME để đổi)" % pzmod.GAME)
    # Loopback only - this server installs files and must not be reachable from the network.
    srv = ThreadingHTTPServer(("127.0.0.1", PORT), Handler)
    url = "http://127.0.0.1:%d/" % PORT
    print("pzmod GUI -> %s   (Ctrl+C to stop)" % url)
    if not os.environ.get("PZMOD_NOOPEN"):
        threading.Timer(0.6, webbrowser.open, [url]).start()
    try:
        srv.serve_forever()
    except KeyboardInterrupt:
        print("\nstopped")
