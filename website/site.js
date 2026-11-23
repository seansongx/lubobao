/*
 * 录播宝官网：下载区与复制按钮。
 * 版本信息来自 xiaofany.com 的公开接口；下载按钮始终用 /dl/lubobao/... 稳定地址（写在 HTML 里），
 * 脚本只补充版本、大小和 SHA-256，并按访客的系统推荐一项。所有文字都用 textContent 写入。
 */
(function () {
  "use strict";

  var DEFAULT_API = "https://xiaofany.com";
  var OS_LABEL = { win: "Windows", mac: "macOS", linux: "Linux" };

  // 本地测试时可以用 ?api=http://127.0.0.1:端口 指向假接口；线上始终用 xiaofany.com。
  function apiBase() {
    var host = location.hostname;
    if (host === "127.0.0.1" || host === "localhost") {
      var v = new URLSearchParams(location.search).get("api");
      if (v && /^http:\/\/(127\.0\.0\.1|localhost)(:\d{1,5})?$/.test(v)) return v;
    }
    return DEFAULT_API;
  }

  /* ---------- 复制 ---------- */

  function legacyCopy(text) {
    var ta = document.createElement("textarea");
    ta.value = text;
    ta.setAttribute("readonly", "");
    ta.className = "sr-only";
    document.body.appendChild(ta);
    ta.select();
    var ok = false;
    try { ok = document.execCommand("copy"); } catch (e) { ok = false; }
    document.body.removeChild(ta);
    return ok;
  }

  function copyText(text) {
    if (navigator.clipboard && window.isSecureContext) {
      return navigator.clipboard.writeText(text).then(
        function () { return true; },
        function () { return legacyCopy(text); }
      );
    }
    return Promise.resolve(legacyCopy(text));
  }

  function flash(btn, ok) {
    var label = btn.getAttribute("data-label") || btn.textContent;
    btn.setAttribute("data-label", label);
    btn.textContent = ok ? "已复制" : "复制失败";
    btn.classList.toggle("is-done", ok);
    var status = document.getElementById("copy-status");
    if (status) status.textContent = ok ? "已复制到剪贴板" : "复制失败，请手动选择文字复制";
    clearTimeout(btn._timer);
    btn._timer = setTimeout(function () {
      btn.textContent = label;
      btn.classList.remove("is-done");
    }, 1600);
  }

  document.addEventListener("click", function (event) {
    var btn = event.target.closest ? event.target.closest("button.copy") : null;
    if (!btn) return;
    var text = btn.getAttribute("data-copy");
    var targetId = btn.getAttribute("data-copy-target");
    if (targetId) {
      var target = document.getElementById(targetId);
      text = target ? target.textContent : "";
    }
    if (!text) return;
    copyText(text).then(function (ok) { flash(btn, ok); });
  });

  /* ---------- 访客系统 ---------- */

  function detectOs() {
    var ua = navigator.userAgent || "";
    var platform = (navigator.userAgentData && navigator.userAgentData.platform) || navigator.platform || "";
    if (/Android|iPhone|iPad|iPod|HarmonyOS/i.test(ua)) return "mobile";
    // iPadOS 的 Safari 自称 Macintosh，用触摸点区分
    if (/Mac/i.test(platform) && navigator.maxTouchPoints > 1) return "mobile";
    if (/Win/i.test(platform) || /Windows NT/.test(ua)) return "win";
    if (/Mac/i.test(platform) || /Mac OS X/.test(ua)) return "mac";
    if (/CrOS/.test(ua)) return null;
    if (/Linux/i.test(platform) || /Linux|X11/.test(ua)) return "linux";
    return null;
  }

  // 判断 Mac 的芯片。判断不了时返回 null，页面同时列出两个版本。
  function detectMacArch() {
    var renderer = "";
    try {
      var gl = document.createElement("canvas").getContext("webgl");
      var ext = gl && gl.getExtension("WEBGL_debug_renderer_info");
      renderer = ext ? String(gl.getParameter(ext.UNMASKED_RENDERER_WEBGL) || "") : "";
    } catch (e) { renderer = ""; }
    // Apple 芯片的 GPU 名称形如 Apple M1；Intel Mac 只会是 Intel、AMD 或 NVIDIA 的显卡
    if (/Apple M\d/.test(renderer)) return Promise.resolve("arm64");
    if (/Intel|AMD|Radeon|NVIDIA/i.test(renderer)) return Promise.resolve("x64");
    var uad = navigator.userAgentData;
    if (uad && uad.getHighEntropyValues) {
      return uad.getHighEntropyValues(["architecture"]).then(
        function (v) { return v && v.architecture === "arm" ? "arm64" : null; },
        function () { return null; }
      );
    }
    return Promise.resolve(null);
  }

  function recommend(os) {
    var hint = document.getElementById("dl-hint");
    if (os === "mobile") {
      if (hint) hint.hidden = false;
      return;
    }
    if (!os) return;
    var card = document.getElementById("dl-" + os);
    if (!card) return;
    card.classList.add("is-recommended");
    var badge = card.querySelector(".dl-badge");
    if (badge) badge.hidden = false;
    var hero = document.getElementById("hero-download");
    if (hero) {
      hero.textContent = "下载 " + OS_LABEL[os] + " 版";
      hero.setAttribute("href", "#dl-" + os);
    }
    if (os === "mac") {
      detectMacArch().then(function (arch) {
        if (!arch) return;
        var files = card.querySelectorAll('.dl-file[data-platform="mac"]');
        for (var i = 0; i < files.length; i++) {
          var btn = files[i].querySelector(".dl-btn");
          var match = files[i].getAttribute("data-arch") === arch;
          btn.classList.toggle("btn-primary", match);
          btn.classList.toggle("btn-ghost", !match);
        }
        var note = document.getElementById("mac-chip-note");
        if (note) {
          note.textContent = arch === "arm64"
            ? "看起来你的 Mac 是 Apple 芯片，选第一个。不确定时可在苹果菜单 →「关于本机」里看「芯片」一栏。"
            : "看起来你的 Mac 是 Intel 芯片，选第二个。不确定时可在苹果菜单 →「关于本机」里看「处理器」一栏。";
        }
      });
    }
  }

  /* ---------- 版本信息 ---------- */

  function formatSize(bytes) {
    if (typeof bytes !== "number" || !isFinite(bytes) || bytes <= 0) return "";
    if (bytes >= 1024 * 1024) return (bytes / 1024 / 1024).toFixed(1) + " MB";
    return Math.max(1, Math.round(bytes / 1024)) + " KB";
  }

  function formatDate(iso) {
    var d = new Date(iso);
    if (!iso || isNaN(d.getTime())) return "";
    try {
      return new Intl.DateTimeFormat("zh-CN", {
        timeZone: "Asia/Shanghai", year: "numeric", month: "long", day: "numeric"
      }).format(d);
    } catch (e) {
      return d.toISOString().slice(0, 10);
    }
  }

  function safeVersion(v) {
    return typeof v === "string" && /^[0-9A-Za-z.+-]{1,40}$/.test(v) ? v : "";
  }

  function setState(state) {
    var section = document.getElementById("download");
    if (section) section.setAttribute("data-state", state);
  }

  function disableButtons() {
    var btns = document.querySelectorAll("#download .dl-btn");
    for (var i = 0; i < btns.length; i++) disable(btns[i]);
  }

  function disable(btn) {
    if (btn.hasAttribute("href")) {
      btn.setAttribute("data-href", btn.getAttribute("href"));
      btn.removeAttribute("href");
    }
    btn.setAttribute("aria-disabled", "true");
    btn.classList.add("is-disabled");
  }

  function findItem(list, platform, arch, format) {
    for (var i = 0; i < list.length; i++) {
      var it = list[i];
      if (!it || typeof it !== "object") continue;
      if (String(it.platform).toLowerCase() === platform &&
          String(it.arch).toLowerCase() === arch &&
          String(it.format).toLowerCase() === format) return it;
    }
    return null;
  }

  function fill(data) {
    var latest = safeVersion(data.version);
    var date = formatDate(data.pubDate);
    var meta = document.getElementById("dl-version");
    if (meta) {
      meta.textContent = latest
        ? "最新版本 v" + latest + (date ? " · " + date + "发布" : "")
        : "选择适合你电脑的版本。";
    }
    if (typeof data.notes === "string" && data.notes.trim()) {
      var notes = document.getElementById("dl-notes");
      var text = document.getElementById("dl-notes-text");
      if (notes && text) {
        text.textContent = data.notes.trim();
        notes.hidden = false;
      }
    }

    var files = document.querySelectorAll("#download .dl-file");
    for (var i = 0; i < files.length; i++) {
      var el = files[i];
      var item = findItem(data.downloads, el.getAttribute("data-platform"),
        el.getAttribute("data-arch"), el.getAttribute("data-format"));
      var btn = el.querySelector(".dl-btn");
      var dl = el.querySelector(".dl-meta");
      if (!item) {
        el.classList.add("is-missing");
        disable(btn);
        btn.textContent = btn.textContent + " · 暂未提供";
        continue;
      }
      var name = typeof item.filename === "string" ? item.filename : "";
      var size = formatSize(item.size);
      var sha = typeof item.sha256 === "string" && /^[0-9a-f]{64}$/i.test(item.sha256) ? item.sha256.toLowerCase() : "";
      var fileVersion = safeVersion(item.version);
      setField(el, "name", name);
      setField(el, "size", size + (fileVersion && fileVersion !== latest ? "（v" + fileVersion + "）" : ""));
      setField(el, "sha256", sha);
      var copy = el.querySelector(".dl-hash .copy");
      if (copy) copy.setAttribute("data-copy", sha);
      if (dl && (size || sha)) dl.hidden = false;
    }
  }

  function setField(el, field, value) {
    var node = el.querySelector('[data-field="' + field + '"]');
    if (!node) return;
    node.textContent = value;
    var row = node.closest("div");
    if (row) row.hidden = !value;
  }

  function showSoon() {
    setState("soon");
    var meta = document.getElementById("dl-version");
    if (meta) meta.textContent = "首个公测版即将发布。";
    document.getElementById("dl-soon").hidden = false;
    disableButtons();
  }

  function showError() {
    setState("error");
    var meta = document.getElementById("dl-version");
    if (meta) meta.textContent = "选择适合你电脑的版本。";
    document.getElementById("dl-error").hidden = false;
  }

  function load() {
    var ctrl = typeof AbortController === "function" ? new AbortController() : null;
    var timer = setTimeout(function () { if (ctrl) ctrl.abort(); }, 10000);
    fetch(apiBase() + "/api/v1/products/lubobao/latest", {
      credentials: "omit",
      headers: { Accept: "application/json" },
      signal: ctrl ? ctrl.signal : undefined
    }).then(function (res) {
      clearTimeout(timer);
      if (res.status === 404) return showSoon();
      if (!res.ok) throw new Error("HTTP " + res.status);
      return res.json().then(function (data) {
        if (!data || !Array.isArray(data.downloads) || data.downloads.length === 0) return showSoon();
        setState("ready");
        fill(data);
      });
    }).catch(function () {
      clearTimeout(timer);
      showError();
    });
  }

  /* ---------- 首次打开：跳到某个系统的说明时自动展开 ---------- */

  function openDetails(id) {
    var el = id ? document.getElementById(id) : null;
    if (el && el.tagName === "DETAILS") el.open = true;
  }

  function hashId(hash) {
    try { return decodeURIComponent(String(hash || "").slice(1)); } catch (e) { return ""; }
  }

  document.addEventListener("click", function (event) {
    var link = event.target.closest ? event.target.closest('a[href^="#"]') : null;
    if (link) openDetails(hashId(link.getAttribute("href")));
  });
  window.addEventListener("hashchange", function () { openDetails(hashId(location.hash)); });
  openDetails(hashId(location.hash));

  recommend(detectOs());
  load();
})();
