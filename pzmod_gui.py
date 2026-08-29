#!/usr/bin/env python3
"""ddmod_gui - local web UI for ddmod. Binds 127.0.0.1 only; stdlib only.

    python ddmod_gui.py [port]
"""
import contextlib
import io
import json
import os
import sys
import threading
import webbrowser
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import parse_qs, quote, urlparse

import ddmod

HERE = os.path.dirname(os.path.abspath(__file__))
PORT = int(sys.argv[1]) if len(sys.argv) > 1 else 8772

# Thumbnails are proxied rather than hotlinked so the page works in browsers that
# block third-party images. The allowlist is what keeps /img from being an open
# SSRF hole - only Steam's own image CDNs, https only, no redirects followed.
IMG_HOSTS = frozenset({"images.steamusercontent.com", "steamuserimages-a.akamaihd.net",
                       "community.cloudflare.steamstatic.com",
                       "community.akamai.steamstatic.com"})
_img_cache = {}
_img_lock = threading.Lock()

# steamcmd is a single process with one workshop cache; two installs at once would
# fight over it. One lock for every file-touching command keeps that honest.
_work = threading.Lock()


def capture(fn, *args):
    """Run a ddmod command, returning its printed lines. ddmod.die() raises SystemExit
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
    st = ddmod.read_state()
    return {"game": ddmod.GAME, "mods": ddmod.MODS,
            "sorts": ddmod.SORTS, "tags": ddmod.TAGS,
            "installed": {wid: dict(e, present=bool(ddmod.folder_of(wid)))
                          for wid, e in st.items()}}


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
    with _img_opener.open(urllib.request.Request(url, headers=ddmod.UA), timeout=30) as r:
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
            "summary": ddmod.strip_bb(d.get("description")).replace("\r", "").strip()[:220]}


def listing(q, sort, page, tags):
    ids = ddmod.browse(q, sort, page, tags)
    meta = ddmod.details(ids)
    # Steam's own ordering is the useful one; details() comes back unordered.
    return [card(meta[i]) for i in ids if i in meta]


def detail(wid):
    d = ddmod.details([wid]).get(wid)
    if not d:
        return {"error": "not found"}
    out = card(d)
    out["description"] = ddmod.strip_bb(d.get("description")).replace("\r", "")
    out["children"] = len(ddmod.children(wid))
    out["created"] = d.get("time_created") or 0
    out["views"] = d.get("views") or 0
    out["favorited"] = d.get("favorited") or 0
    out["folder"] = ddmod.folder_of(wid) or ""
    return out


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
            if url.path == "/api/browse":
                page = max(1, min(50, int(one("page", "1") or 1)))
                sort = one("sort", "trend")
                if sort not in ddmod.SORTS:
                    sort = "trend"
                tags = [t for t in qs.get("tag", []) if t in ddmod.TAGS]
                try:
                    items = listing(one("q"), sort, page, tags)
                except ddmod.Blocked as e:
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

    def do_POST(self):
        url = urlparse(self.path)
        try:
            n = int(self.headers.get("Content-Length") or 0)
            body = json.loads(self.rfile.read(n) or b"{}")
        except Exception:
            return self.send({"error": "bad request body"}, code=400)
        wid = str(body.get("id") or "")
        try:
            if url.path in ("/api/install", "/api/remove") and not wid.isdigit():
                return self.send({"error": "bad id"}, code=400)
            with _work:
                if url.path == "/api/install":
                    return self.send(capture(ddmod.cmd_install, [wid], bool(body.get("force"))))
                if url.path == "/api/remove":
                    return self.send(capture(ddmod.cmd_remove, [wid]))
                if url.path == "/api/update":
                    return self.send(capture(ddmod.cmd_update, []))
            self.send({"error": "not found"}, code=404)
        except Exception as e:
            self.send({"error": "%s: %s" % (type(e).__name__, e)}, code=500)


if __name__ == "__main__":
    if not os.path.isdir(ddmod.GAME):
        ddmod.die("game folder not found: %s (set DD_GAME to override)" % ddmod.GAME)
    # Loopback only - this server installs files and must not be reachable from the network.
    srv = ThreadingHTTPServer(("127.0.0.1", PORT), Handler)
    url = "http://127.0.0.1:%d/" % PORT
    print("ddmod GUI -> %s   (Ctrl+C to stop)" % url)
    if not os.environ.get("DDMOD_NOOPEN"):
        threading.Timer(0.6, webbrowser.open, [url]).start()
    try:
        srv.serve_forever()
    except KeyboardInterrupt:
        print("\nstopped")
