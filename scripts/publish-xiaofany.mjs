#!/usr/bin/env node
//
// ─────────────────────────────────────────────────────────────────────────────
// 来源：xiaofany.com 仓库（私有）的 scripts/release.mjs，commit aa265e02438bfdada1494c4fa017bc3cf1d326c8。
// 除这段说明外与原文件逐字相同，不要在这里修改：上游更新时整份重新拷贝，并改上面的 commit。
// 录播宝用通用模式（--manifest），由 .github/workflows/publish-xiaofany.yml 在 Release 发布后调用，
// 清单由 scripts/xiaofany-manifest.mjs 生成。脚本在公开仓库的 CI 中运行，日志已做脱敏（见 redact）。
// ─────────────────────────────────────────────────────────────────────────────
//
// 发布脚本:直传 R2,然后在 xiaofany.com 发布。见技术方案 9.1(Electron)与 9.4(通用模式)。
// 零依赖(Node 20+),可直接复制到各软件仓库,或抽成共享 npm 包。
//
// 环境变量:
//   RELEASE_API    主站地址,如 https://xiaofany.com
//   RELEASE_TOKEN  后台创建的 Token(权限 release:<slug>)
//   RELEASE_SLUG   软件 slug,如 youmix
//   (--dry-run 只需要 RELEASE_SLUG)
//
// 用法一:Electron(默认,在软件仓库根目录执行,读取 electron-builder 产物):
//   node scripts/release.mjs [--dir release] [--notes="..."] [--notes-file CHANGELOG.md] [--channel beta] [--force] [--dry-run]
//   --notes 请用等号连接(--notes="$NOTES"):更新日志常以「- 」开头,用空格分隔会被当成另一个选项。
//
// 用法二:通用模式(Tauri 等非 Electron 软件,多平台)。逐个指定文件,或用清单文件:
//   node scripts/release.mjs --version 1.2.0 \
//     --file win:x64:exe=dist/lubobao_1.2.0_windows_x64-setup.exe \
//     --file mac:arm64:dmg=dist/lubobao_1.2.0_macos_arm64.dmg \
//     --file dist/lubobao_1.2.0_linux_amd64.deb            # 省略说明时按文件名推断平台、架构、格式
//   node scripts/release.mjs --manifest release-files.json
//   --file 的说明写成 平台[:架构[:格式[:种类]]]=路径,留空的部分按文件名推断;
//     平台 win / mac / linux,架构 x64 / arm64(amd64、aarch64 也可),格式 exe / msi / dmg / pkg / appimage / deb / rpm / zip / tar.gz,
//     种类 installer / archive(默认:压缩包为 archive,其余为 installer)。
//   清单文件格式见技术方案 9.4;文件路径相对清单文件所在目录。命令行参数优先于清单。
//
// 其他选项:
//   --upstream-base <url>  软件自己的 GitHub Release 下载地址前缀,如 https://github.com/owner/repo/releases/download/v1.2.0,
//                          每个文件的「前缀/文件名」登记为备用线路(只用于通用模式)
//   --skip-existing        该版本已发布时视为成功并退出 0(CI 重跑时使用);已撤回的版本仍以非 0 退出
//   --state-file <path>    续传记录的位置,默认当前目录的 .release-state.json
//
// 安全:脚本不含任何密钥,权限只靠 RELEASE_TOKEN;日志中的 URL 会去掉查询串,Token、UploadId 会被替换。
// 续传记录(.release-state.json)含预签名上传地址,请加入 .gitignore,CI 中不要作为 artifact 上传。
//
// 中断后重新运行同一命令会从续传记录继续。CI 中没有续传记录时,遇到同版本的未发布草稿需加 --force 覆盖
// (--force 只覆盖草稿,已发布的版本仍会被拒绝)。

import { createHash } from "node:crypto";
import { createReadStream } from "node:fs";
import { open, readdir, readFile, rename, rm, stat, writeFile } from "node:fs/promises";
import path from "node:path";
import { parseArgs } from "node:util";

let args;
try {
  ({ values: args } = parseArgs({
    options: {
      dir: { type: "string", default: "release" },
      notes: { type: "string" },
      "notes-file": { type: "string" },
      version: { type: "string" },
      channel: { type: "string" },
      force: { type: "boolean", default: false },
      "dry-run": { type: "boolean", default: false },
      concurrency: { type: "string", default: "4" },
      // 通用模式
      manifest: { type: "string" },
      file: { type: "string", multiple: true },
      "upstream-base": { type: "string" },
      "skip-existing": { type: "boolean", default: false },
      "state-file": { type: "string" },
    },
  }));
} catch (err) {
  // 参数解析失败时给出可操作的提示,不打印 Node 的堆栈
  if (err.code === "ERR_PARSE_ARGS_INVALID_OPTION_VALUE") {
    const opt = err.message.match(/'(--[\w-]+)'/)?.[1] ?? "--notes";
    console.error(
      `\n✗ ${opt} 的值以「-」开头(例如 Markdown 列表「- 修复…」),会被当成另一个选项。\n` +
      `  请用等号连接:${opt}="$NOTES";更新日志也可以改用 --notes-file CHANGELOG.md。`,
    );
  } else if (err.code === "ERR_PARSE_ARGS_UNKNOWN_OPTION" || err.code === "ERR_PARSE_ARGS_UNEXPECTED_POSITIONAL") {
    console.error(`\n✗ ${err.message.split("\n")[0]}(用法见脚本开头的注释)`);
  } else {
    console.error(`\n✗ 参数无效:${err.message}`);
  }
  process.exit(1);
}

const API = process.env.RELEASE_API?.replace(/\/+$/, "");
const TOKEN = process.env.RELEASE_TOKEN;
const SLUG = process.env.RELEASE_SLUG;
const STATE_FILE = path.resolve(args["state-file"] ?? ".release-state.json");
const CONCURRENCY = Math.max(1, Number(args.concurrency) || 4);
const GENERIC = Boolean(args.manifest || args.file?.length);
if (args.channel && !["stable", "beta"].includes(args.channel)) {
  console.error("\n✗ --channel 只能是 stable 或 beta");
  process.exit(1);
}

/**
 * 输出前脱敏:脚本可能在公开仓库的 CI 中运行,日志任何人都能看到。
 * - URL 去掉查询串(预签名地址的签名、UploadId 都在查询串里)
 * - Token(xft_ 开头)与 XML 中的 UploadId 替换为占位符
 * 本脚本自身只打印文件名、大小、版本号和 /dl 公开地址,这里兜底处理错误信息。
 */
function redact(text) {
  return String(text)
    .replace(/(https?:\/\/[^\s?#"'<>]+)\?[^\s"'<>]*/g, "$1?…")
    .replace(/xft_[A-Za-z0-9_-]+/g, "xft_***")
    .replace(/<UploadId>[^<]*<\/UploadId>/g, "<UploadId>***</UploadId>")
    .replace(/(uploadId["'=:\s]+)[^\s"'&<]+/gi, "$1***");
}

function fail(message) {
  console.error(`\n✗ ${redact(message)}`);
  process.exit(1);
}

if (!SLUG || (!args["dry-run"] && (!API || !TOKEN))) {
  fail("需要环境变量 RELEASE_API、RELEASE_TOKEN、RELEASE_SLUG(--dry-run 只需要 RELEASE_SLUG)");
}

// ---------- 读取产物 ----------

async function findArtifacts(dir, version) {
  const names = await readdir(dir).catch(() => fail(`找不到产物目录:${dir}`));
  const latestYml = names.includes("latest.yml")
    ? await readFile(path.join(dir, "latest.yml"), "utf8")
    : null;

  // 优先使用 latest.yml 的 path 字段定位安装包
  const ymlPath = latestYml?.match(/^path:\s*['"]?(.+?)['"]?\s*$/m)?.[1];
  const ymlSha512 = latestYml?.match(/^sha512:\s*['"]?(.+?)['"]?\s*$/m)?.[1];
  const ymlVersion = latestYml?.match(/^version:\s*['"]?(.+?)['"]?\s*$/m)?.[1];
  if (ymlVersion && ymlVersion !== version) {
    fail(`latest.yml 的版本 ${ymlVersion} 与 package.json 的 ${version} 不一致,请重新打包`);
  }

  const installer =
    ymlPath && names.includes(ymlPath)
      ? ymlPath
      : names.find((n) => n.endsWith(".exe") && n.includes(version));
  if (!installer) fail(`在 ${dir} 中找不到版本 ${version} 的安装包(.exe)`);

  const files = [{ filename: installer, kind: "installer" }];
  if (names.includes(`${installer}.blockmap`)) {
    files.push({ filename: `${installer}.blockmap`, kind: "blockmap" });
  } else {
    console.warn("! 未找到 .blockmap,客户端将无法差分更新");
  }

  for (const f of files) {
    if (!/^[A-Za-z0-9][A-Za-z0-9._-]*$/.test(f.filename)) {
      fail(`文件名只能包含字母、数字、点、下划线和连字符:${f.filename}(请在 electron-builder 的 artifactName 中使用英文)`);
    }
    f.path = path.join(dir, f.filename);
  }
  return { files, ymlSha512 };
}

// ---------- 通用模式:逐个指定文件(规则与 xiaofany.com 的 src/lib/platforms.ts 一致) ----------

const PLATFORMS = ["win", "mac", "linux"];
const ARCHES = ["x64", "arm64", "ia32"];
const FORMATS = ["exe", "msi", "zip", "dmg", "pkg", "appimage", "deb", "rpm", "tar.gz", "tar.xz", "app.tar.gz"];
// updater:只用于应用内更新、不出现在下载列表的文件(Tauri 在 macOS 上的 .app.tar.gz)
const KINDS = ["installer", "archive", "updater"];
const ALIAS = {
  platform: { windows: "win", win32: "win", win64: "win", macos: "mac", osx: "mac", darwin: "mac" },
  arch: { amd64: "x64", x86_64: "x64", "x86-64": "x64", aarch64: "arm64", x86: "ia32", i386: "ia32", i686: "ia32" },
  format: { tgz: "tar.gz", txz: "tar.xz" },
};
const FORMAT_PLATFORM = { exe: "win", msi: "win", dmg: "mac", pkg: "mac", "app.tar.gz": "mac", appimage: "linux", deb: "linux", rpm: "linux" };

function normalize(type, list, value) {
  const v = String(value ?? "").trim().toLowerCase().replace(type === "format" ? /^\./ : /^$/, "");
  if (!v) return null;
  if (list.includes(v)) return v;
  // 只取自身的键:constructor、toString 等原型链上的键不算别名
  return (Object.hasOwn(ALIAS[type], v) ? ALIAS[type][v] : null) ?? fail(`不支持的 ${type}:${value}(可选 ${list.join("、")})`);
}

function inferFormat(name) {
  const n = name.toLowerCase();
  if (n.endsWith(".app.tar.gz")) return "app.tar.gz";
  if (n.endsWith(".tar.gz") || n.endsWith(".tgz")) return "tar.gz";
  if (n.endsWith(".tar.xz") || n.endsWith(".txz")) return "tar.xz";
  const ext = n.slice(n.lastIndexOf(".") + 1);
  return n.includes(".") && FORMATS.includes(ext) ? ext : null;
}

/** 由文件名推断平台、架构,如 lubobao_1.2.0_macos_arm64.dmg、lubobao_1.2.0_linux_amd64.AppImage */
function inferTarget(name, format) {
  const lower = name.toLowerCase();
  const tokens = lower.split(/[^a-z0-9]+/);
  const pick = (type, list) => {
    for (const t of tokens) {
      if (list.includes(t)) return t;
      if (Object.hasOwn(ALIAS[type], t)) return ALIAS[type][t];
    }
    return null;
  };
  return {
    platform: (format && Object.hasOwn(FORMAT_PLATFORM, format) && FORMAT_PLATFORM[format]) || pick("platform", PLATFORMS),
    arch: /x86[_-]64/.test(lower) ? "x64" : pick("arch", ARCHES),
  };
}

/** 解析 --file "平台:架构:格式:种类=路径";没有 "=" 时整个参数都是路径 */
function parseFileArg(value) {
  const eq = value.indexOf("=");
  // Windows 盘符(C:\...)里也有冒号,只有在 = 前面的才是说明
  if (eq < 0) return { path: value };
  const [platform, arch, format, kind] = value.slice(0, eq).split(":");
  return { path: value.slice(eq + 1), platform, arch, format, kind };
}

/** 补全并检查一个文件的 平台 / 架构 / 格式 / 种类 */
function resolveFile(entry, baseDir, upstreamBase) {
  if (!entry.path) fail("文件缺少路径");
  if (/\.sig$/i.test(entry.path)) {
    fail(`签名文件不用单独指定:${entry.path}(同目录的「文件名.sig」会自动附到对应文件上,也可在清单中用 signature 指定)`);
  }
  const file = path.resolve(baseDir, entry.path);
  const filename = path.basename(file);
  if (!/^[A-Za-z0-9][A-Za-z0-9._-]{0,199}$/.test(filename)) {
    fail(`文件名只能包含字母、数字、点、下划线和连字符:${filename}`);
  }
  const inferredFormat = inferFormat(filename);
  const format = normalize("format", FORMATS, entry.format) ?? inferredFormat;
  if (!format) fail(`无法从文件名判断格式,请指定:${filename}(可选 ${FORMATS.join("、")})`);
  if (inferredFormat && inferredFormat !== format) fail(`格式 ${format} 与文件名后缀不一致:${filename}`);
  const guess = inferTarget(filename, format);
  const platform = normalize("platform", PLATFORMS, entry.platform) ?? guess.platform;
  if (!platform) fail(`无法从文件名判断平台,请指定:${filename}(如 --file linux:x64:${format}=${entry.path})`);
  const arch = normalize("arch", ARCHES, entry.arch) ?? guess.arch;
  if (!arch) fail(`无法从文件名判断架构,请指定:${filename}(如 --file ${platform}:x64:${format}=${entry.path})`);
  const kind = entry.kind
    ? String(entry.kind).toLowerCase()
    : format === "app.tar.gz"
      ? "updater"
      : ["zip", "tar.gz", "tar.xz"].includes(format)
        ? "archive"
        : "installer";
  if (!KINDS.includes(kind)) fail(`kind 只能是 ${KINDS.join(" / ")}:${filename}`);
  const base = entry.upstreamUrl ? null : upstreamBase?.replace(/\/+$/, "");
  const upstreamUrl = entry.upstreamUrl ?? (base ? `${base}/${encodeURIComponent(filename)}` : undefined);
  // 签名:清单里的 signature(.sig 文件路径,相对清单目录);没写时自动使用同目录的「文件名.sig」
  const signaturePath = entry.signature ? path.resolve(baseDir, entry.signature) : `${file}.sig`;
  const signatureRequired = Boolean(entry.signature);
  return { filename, path: file, platform, arch, format, kind, signaturePath, signatureRequired, ...(upstreamUrl ? { upstreamUrl } : {}) };
}

/** 通用模式的版本号、渠道、更新日志与文件列表(命令行参数优先于清单文件) */
async function genericPlan() {
  let manifest = {};
  let baseDir = process.cwd();
  if (args.manifest) {
    const file = path.resolve(args.manifest);
    manifest = JSON.parse(await readFile(file, "utf8").catch(() => fail(`读取清单失败:${file}`)));
    baseDir = path.dirname(file);
  }
  let version = args.version ?? manifest.version;
  if (!version) {
    const pkg = await readFile("package.json", "utf8").then(JSON.parse, () => null);
    version = pkg?.version;
  }
  if (!version) fail("通用模式需要 --version(或在清单中写 version)");
  // GitHub 的 tag 常写成 v1.2.0
  version = String(version).replace(/^v(?=\d)/, "");

  const upstreamBase = args["upstream-base"] ?? manifest.upstreamBase;
  const entries = [
    ...(manifest.files ?? []).map((f) => ({ ...f, baseDir })),
    ...(args.file ?? []).map((f) => ({ ...parseFileArg(f), baseDir: process.cwd() })),
  ];
  if (!entries.length) fail("没有要发布的文件:用 --file 指定,或在清单的 files 中列出");
  const files = entries.map((e) => resolveFile(e, e.baseDir, upstreamBase));

  const seen = new Map();
  for (const f of files) {
    const key = `${f.platform}/${f.arch}/${f.format}`;
    if (seen.has(key)) fail(`平台/架构/格式重复(${key}):${seen.get(key)}、${f.filename}`);
    if (files.filter((x) => x.filename === f.filename).length > 1) fail(`文件名重复:${f.filename}`);
    seen.set(key, f.filename);
  }
  for (const f of files) {
    const info = await stat(f.path).catch(() => fail(`找不到文件:${f.path}`));
    if (!info.isFile()) fail(`不是文件:${f.path}`);
    const sig = await readFile(f.signaturePath, "utf8").catch(() => null);
    if (sig === null && f.signatureRequired) fail(`找不到签名文件:${f.signaturePath}`);
    if (sig !== null) {
      // .sig 的内容是一行 base64 文本(Tauri updater 的签名),原样提交
      f.signature = sig.trim();
      if (!/^[A-Za-z0-9+/=]+$/.test(f.signature)) fail(`签名文件内容不是 base64 文本:${f.signaturePath}`);
    }
    delete f.signaturePath;
    delete f.signatureRequired;
  }

  const channel = args.channel ?? manifest.channel ?? "stable";
  if (!["stable", "beta"].includes(channel)) fail("channel 只能是 stable 或 beta");
  const notesFile = manifest.notesFile ? path.resolve(baseDir, manifest.notesFile) : undefined;
  const notes = args.notes ?? (args["notes-file"] ? undefined : manifest.notes);
  return { version, channel, files, notes, notesFile };
}

function hashFile(file) {
  return new Promise((resolve, reject) => {
    const sha256 = createHash("sha256");
    const sha512 = createHash("sha512");
    createReadStream(file)
      .on("data", (chunk) => {
        sha256.update(chunk);
        sha512.update(chunk);
      })
      .on("error", reject)
      .on("end", () =>
        resolve({ sha256: sha256.digest("hex"), sha512: sha512.digest("base64") }),
      );
  });
}

async function readNotes(version, fallbackFile) {
  if (args.notes) return args.notes;
  const file = args["notes-file"] ?? fallbackFile ?? "CHANGELOG.md";
  const text = await readFile(file, "utf8").catch(() => null);
  if (!text) return "";
  // 取出 "## 1.2.1" 或 "## [1.2.1]" 标题下的内容
  const lines = text.split(/\r?\n/);
  const heading = new RegExp(`^##\\s*\\[?v?${version.replace(/\./g, "\\.")}\\]?(\\s|$)`);
  const start = lines.findIndex((l) => heading.test(l));
  if (start < 0) return "";
  const end = lines.findIndex((l, i) => i > start && /^##\s/.test(l));
  return lines.slice(start + 1, end < 0 ? undefined : end).join("\n").trim();
}

// ---------- 网络 ----------

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

async function retry(label, fn, attempts = 5) {
  for (let i = 1; ; i++) {
    try {
      return await fn();
    } catch (err) {
      if (err.fatal || i >= attempts) throw err;
      const wait = Math.min(30_000, 1000 * 2 ** (i - 1));
      console.warn(redact(`! ${label} 失败(${err.message}),${wait / 1000}s 后重试 ${i}/${attempts - 1}`));
      await sleep(wait);
    }
  }
}

async function api(method, pathname, body) {
  return retry(`${method} ${pathname}`, async () => {
    const res = await fetch(`${API}${pathname}`, {
      method,
      headers: { Authorization: `Bearer ${TOKEN}`, "Content-Type": "application/json" },
      body: body === undefined ? undefined : JSON.stringify(body),
    });
    const data = await res.json().catch(() => ({}));
    if (!res.ok) {
      const err = new Error(data.error ?? `HTTP ${res.status}`);
      err.status = res.status;
      err.code = data.code;
      // 4xx 是参数或状态问题、R2 未配置是服务端配置问题,重试都无意义
      err.fatal = (res.status >= 400 && res.status < 500) || data.code === "r2_not_configured";
      throw err;
    }
    return data;
  });
}

async function readRange(file, start, length) {
  const fh = await open(file, "r");
  try {
    const buf = Buffer.alloc(length);
    const { bytesRead } = await fh.read(buf, 0, length, start);
    return buf.subarray(0, bytesRead);
  } finally {
    await fh.close();
  }
}

async function putOnce(url, headers, body) {
  // Content-Length 由 fetch 根据 body 自动设置(值与签名中的一致)
  const sendHeaders = Object.fromEntries(
    Object.entries(headers).filter(([k]) => k.toLowerCase() !== "content-length"),
  );
  const res = await fetch(url, { method: "PUT", headers: sendHeaders, body });
  if (!res.ok) {
    const text = await res.text().catch(() => "");
    // 只取 S3 错误 XML 里的 Code / Message:其余字段(CanonicalRequest、Key 等)可能含签名参数和 UploadId
    const code = text.match(/<Code>([^<]{0,100})<\/Code>/)?.[1] ?? "";
    const msg = text.match(/<Message>([^<]{0,200})<\/Message>/)?.[1] ?? "";
    const err = new Error(`R2 返回 ${res.status} ${code} ${msg}`.trim());
    // 403 多为签名过期,交给上层重新签发
    err.expired = res.status === 403;
    throw err;
  }
  return res.headers.get("etag");
}

// ---------- 状态(续传) ----------

async function loadState() {
  try {
    return JSON.parse(await readFile(STATE_FILE, "utf8"));
  } catch {
    return null;
  }
}

// 续传记录里有预签名上传地址:只给当前用户读写;不要提交到仓库,也不要作为 CI artifact 上传
// 每次先写临时文件(新建,权限 0600)再改名,覆盖已有文件时权限也保持 0600,中途中断也不会留下半个文件
const saveState = async (state) => {
  const tmp = `${STATE_FILE}.${process.pid}.tmp`;
  await writeFile(tmp, JSON.stringify(state, null, 2), { mode: 0o600 });
  await rename(tmp, STATE_FILE);
};

// ---------- 上传 ----------

async function uploadSingle(releaseId, asset, file, upload) {
  const body = await readFile(file.path);
  await retry(`上传 ${file.filename}`, async () => {
    try {
      await putOnce(upload.url, upload.headers, body);
    } catch (err) {
      if (err.expired) {
        upload = (await api("POST", `/api/v1/releases/${releaseId}/assets/${asset.id}/parts`, {})).upload;
      }
      throw err;
    }
  });
}

async function uploadMultipart(releaseId, asset, file, upload, state, saveFn) {
  const done = (state.parts ??= {});
  const pending = [];
  for (let n = 1; n <= upload.partCount; n++) if (!done[n]) pending.push(n);
  if (!pending.length) return;

  // 每次运行都重新签发需要的分片地址,避免使用过期地址
  const fresh = await api("POST", `/api/v1/releases/${releaseId}/assets/${asset.id}/parts`, {
    partNumbers: pending,
  });
  const urls = new Map(fresh.upload.parts.map((p) => [p.partNumber, p.url]));
  const { partSize } = fresh.upload;
  let finished = upload.partCount - pending.length;

  const worker = async () => {
    for (;;) {
      const n = pending.shift();
      if (n === undefined) return;
      const start = (n - 1) * partSize;
      const chunk = await readRange(file.path, start, Math.min(partSize, file.size - start));
      const etag = await retry(`分片 ${n}/${upload.partCount}`, async () => {
        try {
          return await putOnce(urls.get(n), {}, chunk);
        } catch (err) {
          if (err.expired) {
            const re = await api("POST", `/api/v1/releases/${releaseId}/assets/${asset.id}/parts`, {
              partNumbers: [n],
            });
            urls.set(n, re.upload.parts[0].url);
          }
          throw err;
        }
      });
      if (!etag) throw new Error(`分片 ${n} 未返回 ETag`);
      done[n] = etag;
      await saveFn();
      finished++;
      process.stdout.write(`\r  ${file.filename}: ${finished}/${upload.partCount} 分片`);
    }
  };
  await Promise.all(Array.from({ length: CONCURRENCY }, worker));
  process.stdout.write("\n");
}

// ---------- 主流程 ----------

/** Electron 模式:读取 electron-builder 产物(与之前的行为一致) */
async function electronPlan() {
  const pkg = JSON.parse(await readFile("package.json", "utf8"));
  const version = args.version ?? pkg.version;
  const { files, ymlSha512 } = await findArtifacts(path.resolve(args.dir), version);
  return { version, channel: args.channel ?? "stable", files, ymlSha512 };
}

/** 提交给 POST /api/v1/releases 的文件信息 */
function assetPayload(f) {
  const base = { filename: f.filename, size: f.size, sha256: f.sha256, sha512: f.sha512, kind: f.kind };
  if (!GENERIC) return { ...base, platform: "win", arch: "x64" };
  return {
    ...base,
    platform: f.platform,
    arch: f.arch,
    format: f.format,
    ...(f.signature ? { signature: f.signature } : {}),
    ...(f.upstreamUrl ? { upstreamUrl: f.upstreamUrl } : {}),
  };
}

async function main() {
  const plan = GENERIC ? await genericPlan() : await electronPlan();
  const { version, channel, files, ymlSha512 } = plan;

  console.log(`→ ${SLUG} ${version}(${channel}),计算校验值…`);
  for (const f of files) {
    f.size = (await stat(f.path)).size;
    Object.assign(f, await hashFile(f.path));
    const target = GENERIC ? `  [${f.platform} ${f.arch} ${f.format}${f.kind === "updater" ? " 更新包" : ""}${f.signature ? " 已签名" : ""}]` : "";
    console.log(`  ${f.filename}  ${(f.size / 1024 / 1024).toFixed(1)} MB${target}`);
  }
  const installer = files.find((f) => f.kind === "installer");
  if (ymlSha512 && ymlSha512 !== installer?.sha512) {
    fail("安装包的 sha512 与 latest.yml 不一致:产物被修改过,请重新打包");
  }

  const notes = plan.notes ?? (await readNotes(version, plan.notesFile));
  if (args["dry-run"]) {
    // Electron 模式保持原来的输出;通用模式输出实际提交给接口的文件信息
    const preview = GENERIC
      ? { product: SLUG, version, channel, notes, assets: files.map(assetPayload) }
      : { version, channel, notes, files };
    console.log("\n(dry-run)将提交:", JSON.stringify(preview, null, 2));
    return;
  }

  let state = await loadState();
  if (state && (state.slug !== SLUG || state.version !== version || args.force)) {
    state = null;
  }

  if (!state) {
    let created;
    try {
      created = await api("POST", "/api/v1/releases", {
        product: SLUG,
        version,
        channel,
        notes,
        force: args.force,
        assets: files.map(assetPayload),
      });
    } catch (err) {
      if (err.code === "version_exists" && args["skip-existing"]) {
        console.log(`\n✓ ${SLUG} ${version} 已发布,跳过(--skip-existing)`);
        return;
      }
      // 已撤回的版本不能当作「已发布」跳过:版本号作废,需要发新版本
      if (err.code === "version_withdrawn") {
        fail(`${SLUG} ${version} 已撤回,版本号不能再用;请发布新版本号`);
      }
      if (err.code === "draft_exists") {
        fail(`${err.message}\n  (本机没有该版本的续传记录;确认后用 --force 重新上传)`);
      }
      throw err;
    }
    state = {
      slug: SLUG,
      version,
      releaseId: created.release.id,
      assets: Object.fromEntries(
        created.assets.map((a) => [a.filename, { id: a.id, upload: a.upload, done: false }]),
      ),
    };
    await saveState(state);
  } else {
    console.log(`→ 从 ${path.basename(STATE_FILE)} 续传草稿 ${state.releaseId}`);
  }

  for (const f of files) {
    const s = state.assets[f.filename];
    if (!s) fail(`续传记录中没有 ${f.filename},请使用 --force 重新上传`);
    if (s.done) continue;
    console.log(`↑ ${f.filename}`);
    if (s.upload.type === "multipart") {
      await uploadMultipart(state.releaseId, s, f, s.upload, s, () => saveState(state));
      await api("POST", `/api/v1/releases/${state.releaseId}/assets/${s.id}/complete`, {
        parts: Object.entries(s.parts).map(([n, etag]) => ({ partNumber: Number(n), etag })),
      });
    } else {
      await uploadSingle(state.releaseId, s, f, s.upload);
    }
    s.done = true;
    await saveState(state);
  }

  console.log("→ 发布");
  const result = await api("POST", `/api/v1/releases/${state.releaseId}/publish`);
  await rm(STATE_FILE, { force: true });
  if (GENERIC && result.downloads?.length) {
    console.log(`\n✓ 已发布 ${result.slug} ${result.version}\n  下载地址:`);
    for (const d of result.downloads) console.log(`    ${d.platform} ${d.arch} ${d.format}  ${d.url}`);
  } else {
    console.log(`\n✓ 已发布 ${result.slug} ${result.version}\n  下载地址:${result.download}`);
  }
}

main().catch((err) => fail(err?.message ?? String(err)));
