/* bridge.js — làm cho ui.html chạy trong Tauri mà không phải sửa ui.html.
   ui.html gọi mọi thứ qua fetch("/api/..."), nên chỉ cần bắt fetch ở đây và
   chuyển thành invoke(). Route thiếu trong ROUTES là BUG, không phải việc chưa
   làm nốt: lane Python bị xoá 30/08/2026, không còn bản nào để lùi về. */
(function () {
  var core = window.__TAURI__ && window.__TAURI__.core;
  // Không có Tauri = ai đó mở thẳng index.html bằng trình duyệt. Không còn
  // server nào phục vụ /api/ nữa, nên để fetch nguyên trạng và cho nó fail lộ
  // ra, hơn là giả vờ chạy được.
  if (!core) return;

  // /api/<x> -> tên #[tauri::command]. Thêm hoặc đổi tên lệnh trong main.rs mà
  // quên bảng này = nút bấm chết. Sửa cùng commit, và cập nhật ROUTES.md.
  var ROUTES = {
    "/api/state": "state",
    "/api/enable": "enable",
    "/api/disable": "disable",
    "/api/browse": "browse",
    "/api/detail": "detail",
    "/api/bisect": "bisect",
    "/api/install": "install",
    "/api/remove": "remove",
    "/api/update": "update",
    "/api/prefetch": "prefetch",
    "/api/progress": "progress",
    "/api/launch": "launch",
    "/api/steam_webview_url": "steam_webview_url",
    "/api/steam_webview_navigate": "steam_webview_navigate",
    "/api/steam_webview_open": "steam_webview_open",
    "/api/steam_webview_reload": "steam_webview_reload",
    "/api/steam_webview_back": "steam_webview_back",
    "/api/steam_webview_forward": "steam_webview_forward",
    "/api/steam_webview_harvest": "steam_webview_harvest",
  };

  var NO_CMD = "Lỗi nội bộ: giao diện gọi một route không có trong bản Rust: ";

  function reply(data) {
    return Promise.resolve({
      ok: true,
      status: 200,
      json: function () { return Promise.resolve(data); },
    });
  }

  var nativeFetch = window.fetch.bind(window);

  window.fetch = function (input, init) {
    var url = typeof input === "string" ? input : (input && input.url) || "";
    if (url.indexOf("/api/") !== 0) return nativeFetch(input, init);

    var path = url.split("?")[0];
    var cmd = ROUTES[path];
    if (!cmd) return reply({ error: NO_CMD + path });

    var args = {};
    if (init && init.body) {
      try { args = JSON.parse(init.body); } catch (e) { args = {}; }
    } else {
      // GET: query string -> tham số lệnh
      var q = url.indexOf("?") >= 0 ? new URLSearchParams(url.split("?")[1]) : null;
      if (q) q.forEach(function (v, k) { args[k] = v; });
      if (cmd === "browse") {
        args.page = Math.max(1, Number(args.page) || 1);
        args.tags = q ? q.getAll("tag") : [];
        delete args.tag;
      } else if (cmd === "progress") {
        args.since = Math.max(0, Number(args.since) || 0);
      }
    }

    return core.invoke(cmd, args)
      .then(reply)
      .catch(function (e) { return reply({ error: String(e && e.message ? e.message : e) }); });
  };
})();
