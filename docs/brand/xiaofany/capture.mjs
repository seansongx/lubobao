// 生成 xiaofany.com 产品页用的图标、截图和封面。
//
// 用法（仓库根目录）：
//   npm run build
//   node docs/brand/xiaofany/capture.mjs
//
// 流程：用 vite preview 在 127.0.0.1:4180 提供正式构建（不在 Tauri 里，自动用 src/lib/mock-api.ts 的示例数据，
// 正式构建没有状态栏的「调试」入口），再用无头 Edge（或 Chrome）通过 DevTools 协议打开页面、点击、截图。
// 浅色，?platform=windows&ffmpeg=ready（状态栏显示 FFmpeg 可用），窗口 1440×900。不依赖额外的 npm 包，需要 Node 22 以上（内置 WebSocket）。
// 浏览器路径可用环境变量 BROWSER 指定。

import { spawn } from "node:child_process";
import { existsSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const root = resolve(here, "../../..");
const port = 4180;
const debugPort = 9333;
const base = `http://127.0.0.1:${port}/?platform=windows&ffmpeg=ready`;

const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

function findBrowser() {
  const candidates = [
    process.env.BROWSER,
    "C:/Program Files (x86)/Microsoft/Edge/Application/msedge.exe",
    "C:/Program Files/Microsoft/Edge/Application/msedge.exe",
    "C:/Program Files/Google/Chrome/Application/chrome.exe",
    "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
    "/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge",
    "/usr/bin/google-chrome",
    "/usr/bin/chromium",
  ];
  const found = candidates.find((path) => path && existsSync(path));
  if (!found) throw new Error("找不到 Edge 或 Chrome，请用 BROWSER 环境变量指定");
  return found;
}

async function waitFor(url, timeoutMs = 20000) {
  const start = Date.now();
  while (Date.now() - start < timeoutMs) {
    try {
      const response = await fetch(url);
      if (response.ok) return response;
    } catch {
      // 还没起来
    }
    await sleep(200);
  }
  throw new Error(`等待超时：${url}`);
}

class Cdp {
  constructor(ws) {
    this.ws = ws;
    this.nextId = 1;
    this.pending = new Map();
    this.listeners = new Set();
    ws.addEventListener("message", (event) => {
      const message = JSON.parse(event.data);
      if (message.id && this.pending.has(message.id)) {
        const { resolve, reject } = this.pending.get(message.id);
        this.pending.delete(message.id);
        if (message.error) reject(new Error(message.error.message));
        else resolve(message.result);
      } else if (message.method) {
        for (const listener of this.listeners) listener(message);
      }
    });
  }

  static async connect(url) {
    const ws = new WebSocket(url);
    await new Promise((resolve, reject) => {
      ws.addEventListener("open", resolve, { once: true });
      ws.addEventListener("error", reject, { once: true });
    });
    return new Cdp(ws);
  }

  send(method, params = {}) {
    const id = this.nextId++;
    this.ws.send(JSON.stringify({ id, method, params }));
    return new Promise((resolve, reject) => this.pending.set(id, { resolve, reject }));
  }

  waitEvent(method) {
    return new Promise((resolve) => {
      const listener = (message) => {
        if (message.method === method) {
          this.listeners.delete(listener);
          resolve(message.params);
        }
      };
      this.listeners.add(listener);
    });
  }

  async evaluate(expression) {
    const result = await this.send("Runtime.evaluate", { expression, awaitPromise: true, returnByValue: true });
    if (result.exceptionDetails) throw new Error(result.exceptionDetails.exception?.description ?? "evaluate 失败");
    return result.result.value;
  }

  close() {
    this.ws.close();
  }
}

async function setViewport(cdp, width, height, scale = 1, transparent = false) {
  await cdp.send("Emulation.setDeviceMetricsOverride", { width, height, deviceScaleFactor: scale, mobile: false });
  await cdp.send("Emulation.setEmulatedMedia", { features: [{ name: "prefers-color-scheme", value: "light" }] });
  await cdp.send("Emulation.setDefaultBackgroundColorOverride", transparent ? { color: { r: 0, g: 0, b: 0, a: 0 } } : {});
}

async function navigate(cdp, url, settleMs = 1500) {
  const loaded = cdp.waitEvent("Page.loadEventFired");
  await cdp.send("Page.navigate", { url });
  await loaded;
  await cdp.evaluate("document.fonts.ready.then(() => true)");
  await sleep(settleMs);
}

async function shot(cdp, file, options = {}) {
  const { format = "png", quality } = options;
  const result = await cdp.send("Page.captureScreenshot", { format, quality, captureBeyondViewport: false });
  writeFileSync(file, Buffer.from(result.data, "base64"));
  console.log(`已生成 ${file}`);
}

// 点左侧导航里文字为 label 的按钮。
const clickNav = (label) => `(() => {
  const button = [...document.querySelectorAll('nav[aria-label="主导航"] button')].find((item) => item.textContent.includes(${JSON.stringify(label)}));
  if (!button) throw new Error("找不到导航：" + ${JSON.stringify(label)});
  button.click();
  return true;
})()`;

// 点任务表里状态为「录制中」的第一行，右侧显示详情。
const selectRecordingRow = `(() => {
  const rows = [...document.querySelectorAll('tbody tr')];
  const row = rows.find((item) => item.textContent.includes("录制中")) ?? rows[0];
  if (!row) throw new Error("任务表是空的");
  row.click();
  return row.textContent.slice(0, 40);
})()`;

// 录像库里双击名为 name 的文件夹进入。
const openLibraryFolder = (name) => `(() => {
  const row = [...document.querySelectorAll('[role="option"][data-path]')].find((item) => item.dataset.path.endsWith(${JSON.stringify(name)}));
  if (!row) throw new Error("找不到文件夹：" + ${JSON.stringify(name)});
  row.dispatchEvent(new MouseEvent("dblclick", { bubbles: true }));
  return true;
})()`;

// 移走鼠标悬停和焦点，免得截图里出现悬停底色或焦点环。
const blur = `(() => { document.activeElement?.blur?.(); return true; })()`;

async function main() {
  if (!existsSync(join(root, "dist/index.html"))) throw new Error("先运行 npm run build");
  const assets = here;

  const preview = spawn("npx", ["vite", "preview", "--host", "127.0.0.1", "--port", String(port), "--strictPort"], {
    cwd: root,
    shell: process.platform === "win32", // Windows 上 npx 是 .cmd，需要经过 shell
    stdio: "ignore",
  });
  const profile = mkdtempSync(join(tmpdir(), "lubobao-capture-"));
  const browser = spawn(
    findBrowser(),
    [
      "--headless=new",
      `--remote-debugging-port=${debugPort}`,
      `--user-data-dir=${profile}`,
      "--no-first-run",
      "--no-default-browser-check",
      "--hide-scrollbars",
      "--force-color-profile=srgb",
      "--lang=zh-CN",
      "about:blank",
    ],
    { stdio: "ignore" },
  );

  let cdp;
  try {
    await waitFor(`http://127.0.0.1:${port}/`);
    await waitFor(`http://127.0.0.1:${debugPort}/json/version`);
    const targets = await (await fetch(`http://127.0.0.1:${debugPort}/json/list`)).json();
    const page = targets.find((target) => target.type === "page");
    cdp = await Cdp.connect(page.webSocketDebuggerUrl);
    await cdp.send("Page.enable");
    await cdp.send("Runtime.enable");

    // 1. 截图：1440×900，浅色。
    await setViewport(cdp, 1440, 900);
    await navigate(cdp, base, 2500);
    await cdp.evaluate(selectRecordingRow);
    await sleep(800);
    await cdp.evaluate(blur);
    await shot(cdp, join(assets, "screenshot-1-rooms.png"));

    // 录像库进到「抖音 / 一只小羊驼」，显示分段和 MP4 文件。
    await cdp.evaluate(clickNav("录像库"));
    await sleep(1200);
    for (const folder of ["抖音", "一只小羊驼"]) {
      await cdp.evaluate(openLibraryFolder(folder));
      await sleep(1200);
    }
    await sleep(2500);
    await cdp.evaluate(blur);
    await shot(cdp, join(assets, "screenshot-2-library.png"));

    for (const [label, file] of [
      ["动态", "screenshot-3-activity.png"],
      ["设置", "screenshot-4-settings.png"],
    ]) {
      await cdp.evaluate(clickNav(label));
      await sleep(1500);
      await cdp.evaluate(blur);
      await shot(cdp, join(assets, file));
    }

    // 2. 封面用的界面图：1280×800 两倍像素，直播间页不选中行。
    await setViewport(cdp, 1280, 800, 2);
    await navigate(cdp, base, 2500);
    await cdp.evaluate(blur);
    const coverSource = join(profile, "cover-source.png");
    await shot(cdp, coverSource);

    // 3. 封面：cover.html 把界面图放在纸色背景上，1600×1000。
    const coverHtml = readFileSync(join(assets, "cover.html"), "utf8")
      .replace('src="cover-source.png"', `src="${pathToFileURL(coverSource).href}"`)
      .replace('src="app-icon.svg"', `src="${pathToFileURL(join(root, "docs/brand/app-icon.svg")).href}"`);
    const coverPage = join(profile, "cover.html");
    writeFileSync(coverPage, coverHtml);
    await setViewport(cdp, 1600, 1000);
    await navigate(cdp, pathToFileURL(coverPage).href, 800);
    await shot(cdp, join(assets, "cover.png"));
    await shot(cdp, join(assets, "cover.webp"), { format: "webp", quality: 90 });

    // 4. 图标：app-icon.svg 渲染成 256×256 透明底 PNG。
    const iconPage = join(profile, "icon.html");
    const svg = pathToFileURL(join(root, "docs/brand/app-icon.svg")).href;
    writeFileSync(
      iconPage,
      `<!doctype html><html><body style="margin:0;background:transparent"><img src="${svg}" width="256" height="256" style="display:block"></body></html>`,
    );
    await setViewport(cdp, 256, 256, 1, true);
    await navigate(cdp, pathToFileURL(iconPage).href, 300);
    await shot(cdp, join(assets, "icon-256.png"));
  } finally {
    cdp?.close();
    browser.kill();
    if (process.platform === "win32") spawn("taskkill", ["/pid", String(preview.pid), "/t", "/f"], { stdio: "ignore" });
    else preview.kill();
    await sleep(1000);
    try {
      rmSync(profile, { recursive: true, force: true });
    } catch {
      // Edge 可能还没完全退出，临时目录留给系统清理
    }
  }
}

main().catch((error) => {
  console.error(error);
  process.exit(1);
});
