/* bridge.js — làm cho ui.html chạy trong Tauri mà không phải sửa ui.html.
   ui.html gọi mọi thứ qua fetch("/api/..."), nên chỉ cần bắt fetch ở đây và
   chuyển thành invoke(). Route nào chưa port thì trả lỗi rõ ràng, không im lặng. */
(function () {
  var core = window.__TAURI__ && window.__TAURI__.core;
  if (!core) return; // mở bằng trình duyệt thường -> để nguyên fetch của bản Python

  // /api/<x> -> tên #[tauri::command]. Thiếu ở đây = chưa port.
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
  };

  var NOT_YET = "Chức năng này chưa port sang bản Rust. Dùng pzmod-gui.bat (bản Python) cho tới khi xong.";

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
    if (!cmd) return reply({ error: NOT_YET });

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
      }
    }

    return core.invoke(cmd, args)
      .then(reply)
      .catch(function (e) { return reply({ error: String(e && e.message ? e.message : e) }); });
  };
})();
