#!/usr/bin/env node
// GitHub Release 上的 Tauri 静态更新清单 latest.json：xiaofany.com 不可用时，应用从
// https://github.com/<owner>/<repo>/releases/latest/download/latest.json 取更新（tauri.conf.json 的第二个端点）。
//
// 生成（release.yml 在 v* 标签的草稿 Release 里调用）：
//   node scripts/updater-latest-json.mjs --dir <安装包目录> --tag v0.1.1 [--repo owner/repo] [--out <路径>] [--release-body <正文.md>]
// 写入更新说明（Release 发布后，publish-xiaofany.yml 调用；只改 notes 和 pub_date）：
//   node scripts/updater-latest-json.mjs --set-notes <latest.json> --tag v0.1.1 --release-body <正文.md>
//
// 每个平台都要有更新包和同名 .sig，缺任何一个都失败，不生成残缺的清单。平台键是
// tauri-plugin-updater 查找的 `{os}-{arch}`（os 为 windows、darwin、linux）；它先找
// `{os}-{arch}-{安装方式}`，找不到再用 `{os}-{arch}`，所以 Linux 的 deb 不会拿到 AppImage：
// 应用里 deb 安装本来就只给官网链接。
// owner/repo 默认取环境变量 GITHUB_REPOSITORY。--out 默认 <目录>/latest.json。

import { readFile, stat, writeFile } from "node:fs/promises";
import path from "node:path";
import { parseArgs } from "node:util";

const { values: args } = parseArgs({
  options: {
    dir: { type: "string" },
    tag: { type: "string" },
    repo: { type: "string" },
    out: { type: "string" },
    "release-body": { type: "string" },
    "set-notes": { type: "string" },
    "pub-date": { type: "string" },
  },
});

function fail(message) {
  console.error(`✗ ${message}`);
  process.exit(1);
}

const USAGE =
  "用法：--dir <目录> --tag v<版本> [--repo owner/repo] [--out <路径>] [--release-body <文件>]，" +
  "或 --set-notes <latest.json> --tag v<版本> --release-body <文件>";

if (!args.tag) fail(USAGE);
const version = args.tag.replace(/^v(?=\d)/, "");
if (!/^\d+\.\d+\.\d+(-[0-9A-Za-z.-]+)?$/.test(version)) fail(`tag 不是 v<semver>：${args.tag}`);
const pubDate = args["pub-date"] ?? new Date().toISOString().replace(/\.\d{3}Z$/, "Z");
if (Number.isNaN(Date.parse(pubDate))) fail(`--pub-date 不是 RFC 3339 时间：${pubDate}`);

/** 文件名规则与 release.yml 的「Collect installers」一致。 */
const PLATFORMS = {
  "windows-x86_64": `lubobao_${version}_windows_x64-setup.exe`,
  "darwin-aarch64": `lubobao_${version}_macos_arm64.app.tar.gz`,
  "darwin-x86_64": `lubobao_${version}_macos_x64.app.tar.gz`,
  "linux-x86_64": `lubobao_${version}_linux_amd64.AppImage`,
};

const MARKER = "<!-- 以上为应用内更新说明 -->";
const PLACEHOLDER = "发布前把这里换成本版本的变化";

/**
 * 从 Release 正文取应用内更新说明，规则与 publish-xiaofany.yml 的「Read release」相同：
 * 分隔注释以上的部分，去掉开头的「## 更新内容」标题；没有分隔注释或还是占位句时失败。
 * 说明为空时返回 undefined（清单里不写 notes）。
 */
function notesFrom(body) {
  const text = body.replace(/\r/g, "");
  const index = text.indexOf(MARKER);
  if (index < 0) fail(`Release 正文里没有「${MARKER}」这一行`);
  let lines = text.slice(0, index).split("\n");
  if (/^## 更新内容\s*$/.test(lines[0] ?? "")) lines = lines.slice(1);
  const notes = lines.join("\n").trim();
  if (notes.includes(PLACEHOLDER)) fail("Release 正文里的「更新内容」还是占位句");
  if (!notes) {
    console.warn("! 更新说明是空的，清单里不写 notes");
    return undefined;
  }
  return notes;
}

async function readNotes() {
  const file = args["release-body"];
  return file === undefined ? undefined : notesFrom(await readFile(file, "utf8"));
}

/** 按 Tauri 静态清单的字段顺序输出；notes 为 undefined 时省略。 */
function manifest(notes, platforms) {
  return { version, ...(notes === undefined ? {} : { notes }), pub_date: pubDate, platforms };
}

async function write(out, data) {
  await writeFile(out, `${JSON.stringify(data, null, 2)}\n`);
}

if (args["set-notes"]) {
  if (args["release-body"] === undefined) fail(USAGE);
  const file = path.resolve(args["set-notes"]);
  const current = JSON.parse(await readFile(file, "utf8"));
  if (current.version !== version) fail(`${path.basename(file)} 的版本是 ${current.version}，应为 ${version}`);
  const missing = Object.keys(PLATFORMS).filter((key) => !current.platforms?.[key]?.signature || !current.platforms?.[key]?.url);
  if (missing.length) fail(`${path.basename(file)} 缺少平台：${missing.join("、")}`);
  const notes = await readNotes();
  await write(file, manifest(notes, current.platforms));
  console.log(`→ ${path.basename(file)}：${version}，已${notes === undefined ? "清空" : "写入"}更新说明`);
  process.exit(0);
}

if (!args.dir) fail(USAGE);
const repo = args.repo ?? process.env.GITHUB_REPOSITORY;
if (!repo || !/^[A-Za-z0-9_.-]+\/[A-Za-z0-9_.-]+$/.test(repo)) {
  fail(`需要 owner/repo（--repo 或环境变量 GITHUB_REPOSITORY），现在是：${repo ?? "未设置"}`);
}
const dir = path.resolve(args.dir);
const out = path.resolve(args.out ?? path.join(dir, "latest.json"));
const base = `https://github.com/${repo}/releases/download/${encodeURIComponent(`v${version}`)}`;

async function isFile(file) {
  try {
    return (await stat(file)).isFile();
  } catch {
    return false;
  }
}

const platforms = {};
const problems = [];
for (const [key, name] of Object.entries(PLATFORMS)) {
  const file = path.join(dir, name);
  if (!(await isFile(file))) {
    problems.push(`${key}：缺少 ${name}`);
    continue;
  }
  if (!(await isFile(`${file}.sig`))) {
    problems.push(`${key}：缺少 ${name}.sig`);
    continue;
  }
  // .sig 的内容就是一行 base64（minisign 文本再整体 base64），原样作为 signature。
  const signature = (await readFile(`${file}.sig`, "utf8")).trim();
  if (!signature || /\s/.test(signature)) {
    problems.push(`${key}：${name}.sig 不是一行 base64`);
    continue;
  }
  platforms[key] = { signature, url: `${base}/${encodeURIComponent(name)}` };
}
if (problems.length) fail(`不生成 latest.json：\n  ${problems.join("\n  ")}`);

await write(out, manifest(await readNotes(), platforms));
console.log(`→ ${path.relative(process.cwd(), out) || out}：${version}，${Object.keys(platforms).length} 个平台`);
for (const [key, entry] of Object.entries(platforms)) console.log(`  ${key}  ${entry.url}`);
